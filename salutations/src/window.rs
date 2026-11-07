//! The main window: folder pane | message list | viewer, and the glue
//! between them and the accounts.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use salutations_core::camel::{self, prelude::*};
use salutations_core::edataserver::SourceRegistry;
use salutations_core::offline;
use salutations_core::{Account, Session, Settings, SpecialFolders};

use crate::folder_object::{FolderObject, SyncState};
use crate::folder_pane::{DraggedMessages, FolderPane};
use crate::manage::{self, FolderChoice};
use crate::notify::{self, Notifier};
use crate::search::{self, Search};
use crate::message_list::MessageList;
use crate::composer::Composer;
use crate::viewer::Viewer;
use actions_bar::{ActionButtons, Place};

mod actions_bar;
mod compose;
mod import;
mod keys;
mod outbox;

const SEEN: u32 = camel::MessageFlags::SEEN.bits();
const FLAGGED: u32 = camel::MessageFlags::FLAGGED.bits();
const DELETED: u32 = camel::MessageFlags::DELETED.bits();
/// Network changes come in bursts; accounts are synced at most this often
/// because of them.
const NETWORK_RESYNC_SECONDS: u64 = 30;
/// Connecting and listing folders take at most this long; a connection
/// left dead by sleep or a network change otherwise hangs a sync for
/// minutes, and every later sync waits on it.
const STEP_LIMIT: std::time::Duration = std::time::Duration::from_secs(45);
/// Seconds before retrying a failed sync, by failures in a row; the last
/// repeats.
const RETRY_SECONDS: [u32; 4] = [30, 60, 120, 300];
/// Minutes between push connection checks, off and on metered connections.
const KEEPALIVE_MINUTES: u32 = 15;
const KEEPALIVE_METERED_MINUTES: u32 = 30;

struct AccountCtl {
    account: Account,
    obj: FolderObject,
    store: RefCell<Option<camel::Store>>,
    /// Ends the running sync; see `resync`.
    abort: RefCell<Option<futures_channel::oneshot::Sender<()>>>,
    /// A sync was asked for while one ran; it starts when that one ends.
    again: Cell<bool>,
    /// Failed syncs in a row, for the retry delay.
    failures: Cell<u32>,
    /// The retry after a failed sync.
    retry: RefCell<Option<glib::SourceId>>,
    /// The store's connection is watched for drops.
    watching: Cell<bool>,
    /// When the running sync started.
    sync_started: Cell<Option<std::time::Instant>>,
    /// INBOX's last unread count, kept for the badge while the folder is
    /// unavailable.
    unread: Cell<Option<i64>>,
    /// Saved by the last session, then checked against the server once
    /// per session.
    special: RefCell<Option<SpecialFolders>>,
    special_checked: Cell<bool>,
    /// The last sync failed to sign in, so the error popover offers Online
    /// Accounts.
    sign_in_failed: Cell<bool>,
    /// The check-interval timer.
    timer: RefCell<Option<glib::SourceId>>,
    /// Paused for Power Saver, as last applied.
    paused: Cell<bool>,
    /// With push on: the periodic check that notices a dead push
    /// connection; see `schedule_keepalive`.
    keepalive: RefCell<Option<glib::SourceId>>,
    /// Folders kept offline as of the last maintenance run, to find the
    /// ones switched off since. Empty until the first run.
    synced: RefCell<HashSet<String>>,
    /// The window's edge at the last maintenance run; later runs prune
    /// only messages that crossed it since. None means check everything.
    last_cutoff: Cell<Option<i64>>,
    /// A maintenance run is under way; another request waits for it.
    maintaining: Cell<bool>,
    /// Requested while a run was under way: Some(full).
    maintain_again: Cell<Option<bool>>,
    last_sync: Cell<Option<std::time::Instant>>,
    /// INBOX, watched for new mail while Salutations runs.
    inbox: RefCell<Option<(camel::Folder, glib::SignalHandlerId)>>,
    /// Messages with a notification of their own, oldest first.
    noted: RefCell<Vec<String>>,
    /// Messages counted in the combined notification.
    combined: RefCell<Vec<String>>,
    /// The highest INBOX UID considered for notifying; saved between runs.
    last_uid: Cell<Option<u64>>,
    /// Messages Salutations itself is moving into INBOX, which must not
    /// notify.
    expected: RefCell<Vec<MessageKey>>,
    /// Waiting messages are being sent; another retry waits for the next
    /// sync.
    sending: Cell<bool>,
    /// The waiting draft being sent right now.
    sending_uid: RefCell<Option<String>>,
}

/// The messages an action applies to.
#[derive(Clone)]
struct Target {
    account: Rc<AccountCtl>,
    folder: camel::Folder,
    uids: Vec<String>,
    /// From a thread's summary row: all of its messages.
    thread: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Read,
    Star,
    Move,
    Archive,
    Junk,
    NotJunk,
    Restore,
    /// Trash; in Trash, or without one, deletes permanently.
    Delete,
    /// Moves to Trash, never deleting.
    Trash,
    DeletePermanently,
}

/// One undoable action as the toast describes it; repeated actions of the
/// same kind merge into one batch while its toast is shown.
struct Batch {
    /// The destination's full name.
    dest: String,
    /// "deleted" or "moved to <path>".
    verb: String,
    count: usize,
    moves: Vec<Undo>,
}

impl Batch {
    fn text(&self) -> String {
        match self.count {
            1 => format!("Message {}", self.verb),
            n => format!("{n} messages {}", self.verb),
        }
    }
}

/// Moves one transfer back.
enum Undo {
    /// A move between accounts, already done.
    Done {
        /// Where the messages are now, and their UIDs there.
        from: camel::Folder,
        uids: Vec<String>,
        to: camel::Folder,
        /// Of `uids`, those marked read on the way; unread again on undo.
        unread: Vec<String>,
    },
    /// A move within an account, done or not yet.
    Queued(Rc<Job>),
}

/// A move within an account. Its messages leave the list at once; the
/// server is asked in order, one job at a time per account.
struct Job {
    account: Rc<AccountCtl>,
    folder: camel::Folder,
    uids: Vec<String>,
    dest: String,
    /// Deleted for good once in `dest`, the account's Trash.
    permanent: bool,
    /// Of `uids`, those marked read on the way.
    unread: Vec<String>,
    state: RefCell<JobState>,
}

enum JobState {
    Waiting,
    Running,
    /// Done: the destination and the messages' UIDs there.
    Moved(camel::Folder, Vec<String>),
    /// Done, but the server did not say where the messages went.
    Lost,
    Failed,
    Cancelled,
}

impl Job {
    fn waiting(&self) -> bool {
        matches!(*self.state.borrow(), JobState::Waiting)
    }

    /// Waiting or running: its messages are left out of the list.
    fn pending(&self) -> bool {
        matches!(*self.state.borrow(), JobState::Waiting | JobState::Running)
    }
}

struct OpenFolder {
    account: Rc<AccountCtl>,
    obj: FolderObject,
    folder: camel::Folder,
    handler: glib::SignalHandlerId,
}

pub struct Window {
    pub window: adw::ApplicationWindow,
    registry: SourceRegistry,
    session: Session,
    accounts: RefCell<Vec<Rc<AccountCtl>>>,
    folders: Rc<FolderPane>,
    list: Rc<MessageList>,
    viewer: Rc<Viewer>,
    title: adw::WindowTitle,
    outer: adw::OverlaySplitView,
    inner: adw::NavigationSplitView,
    open: RefCell<Option<OpenFolder>>,
    read_timer: RefCell<Option<glib::SourceId>>,
    /// The message view's header bar actions.
    viewer_buttons: ActionButtons,
    toasts: adw::ToastOverlay,
    /// The session's undoable actions, oldest first; Ctrl+Z takes the last.
    undo: RefCell<Vec<Batch>>,
    /// The toast of the last batch while it is shown.
    toast: RefCell<Option<adw::Toast>>,
    search: Rc<Search>,
    notifier: Rc<Notifier>,
    /// The search button's icon or spinner.
    search_icons: gtk::Stack,
    /// The folder's list or the search results.
    list_stack: gtk::Stack,
    search_cancel: RefCell<Option<gio::Cancellable>>,
    /// Accounts whose server search is still running.
    searching: Cell<usize>,
    /// Counts searches, so results of a replaced one are dropped.
    search_gen: Cell<u64>,
    /// Selection mode's toolbar, under the search bar.
    select_bar: gtk::Revealer,
    /// "N selected", with Select all and Select none.
    select_count: gtk::MenuButton,
    select_buttons: ActionButtons,
    /// Selection mode's toggle, pressed while it is on.
    select_toggle: gtk::ToggleButton,
    search_button: gtk::ToggleButton,
    /// Shows its accent colour while a filter is on.
    filter_button: gtk::MenuButton,
    filter_empty: adw::StatusPage,
    /// Pull to refresh on the message list and the search results.
    pull: Rc<crate::pull_refresh::PullRefresh>,
    /// Over the message list while the open folder's account is paused.
    pause_banner: adw::Banner,
    /// "Empty Trash" or "Empty Junk" over those folders while they hold
    /// messages.
    empty_banner: adw::Banner,
    /// Kept so its notifications keep coming.
    power_monitor: RefCell<Option<gio::PowerProfileMonitor>>,
    /// logind's wake signal, kept for the window's lifetime.
    sleep_watch: RefCell<Option<gio::SignalSubscription>>,
    /// The folder being opened, so startup can wait for INBOX.
    opening: RefCell<Option<glib::JoinHandle<()>>>,
    composer: Rc<Composer>,
    /// The message view or the composer.
    content_stack: gtk::Stack,
    compose: RefCell<Option<compose::Compose>>,
    /// Its bottom bar holds Reply, Reply All and Forward.
    viewer_view: adw::ToolbarView,
    /// Reply and Reply All as a button or as a format menu.
    reply_stack: gtk::Stack,
    reply_all_stack: gtk::Stack,
    /// The floating New Message button over the list.
    compose_float: gtk::Button,
    /// A notification's Reply: replied to once the viewer shows it.
    reply_when_shown: RefCell<Option<(camel::Folder, String)>>,
    /// OpenPGP for reading and sending.
    gpg: camel::GpgContext,
    /// The split view, or the pane with the message below the list.
    layout: gtk::Stack,
    paned: gtk::Paned,
    list_page: adw::NavigationPage,
    viewer_page: adw::NavigationPage,
    /// The list side and the message side, moved between the layouts.
    list_side: adw::ToolbarView,
    viewer_side: gtk::Overlay,
    viewer_header: adw::HeaderBar,
    /// Drags the list's width beside the message.
    width_handle: gtk::Box,
    /// UIDs the next transfer's messages had unread before a delete or
    /// junk marked them read, for its undo.
    unread_before: RefCell<Vec<String>>,
    /// Moves not yet finished, in the order asked.
    queue: RefCell<Vec<Rc<Job>>>,
    /// Accounts whose jobs are being worked through.
    working: RefCell<HashSet<String>>,
    /// When the pane's divider was last placed by Salutations; positions
    /// reported soon after are its own, not the user's.
    divider_set: Cell<Option<std::time::Instant>>,
}

impl Window {
    pub fn new(app: &adw::Application, registry: SourceRegistry, session: Session) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Salutations")
            .default_width(1280)
            .default_height(800)
            .width_request(360)
            .height_request(294)
            .build();

        let this = Rc::new_cyclic(|weak: &std::rc::Weak<Window>| {
            let w1 = weak.clone();
            let w2 = weak.clone();
            let w3 = weak.clone();
            let w4 = weak.clone();
            let w5 = weak.clone();
            let w6 = weak.clone();
            let w7 = weak.clone();
            let folders = FolderPane::new(
                move |obj| {
                    if let Some(this) = w1.upgrade() {
                        this.open_folder(obj);
                    }
                },
                // Arrow keys move the selection without closing the overlay.
                move || {
                    if let Some(this) = w3.upgrade()
                        && this.outer.is_collapsed()
                    {
                        this.outer.set_show_sidebar(false);
                    }
                },
                move |obj, button| {
                    if let Some(this) = w2.upgrade() {
                        this.sync_button_pressed(obj, button);
                    }
                },
                move |obj, m| {
                    if let Some(this) = w4.upgrade() {
                        this.dropped(obj, m);
                    }
                },
                move |obj, files| {
                    if let Some(this) = w5.upgrade() {
                        this.import_files(files, Some((obj.account_uid(), obj.full_name())));
                    }
                },
                move |obj, widget, x, y| {
                    if let Some(this) = w7.upgrade() {
                        this.folder_menu(obj, widget, x, y);
                    }
                },
            );
            let list = MessageList::new();
            let viewer = Viewer::new();
            let gpg = salutations_core::crypto::context(&session);
            viewer.set_gpg(gpg.clone());
            let title = adw::WindowTitle::new("", "");

            let folders_header = adw::HeaderBar::builder()
                .title_widget(&adw::WindowTitle::new("Salutations", ""))
                .build();
            let menu = gio::Menu::new();
            let accounts = gio::Menu::new();
            accounts.append(Some("_Online Accounts…"), Some("app.online-accounts"));
            menu.append_section(None, &accounts);
            let app_items = gio::Menu::new();
            let import_items = gio::Menu::new();
            import_items.append(Some("_Import messages…"), Some("win.import"));
            menu.append_section(None, &import_items);
            app_items.append(Some("_Preferences"), Some("win.preferences"));
            app_items.append(Some("_Keyboard shortcuts"), Some("app.shortcuts"));
            app_items.append(Some("_About Salutations"), Some("app.about"));
            menu.append_section(None, &app_items);
            let quit = gio::Menu::new();
            quit.append(Some("_Quit"), Some("app.quit"));
            menu.append_section(None, &quit);
            folders_header.pack_end(
                &gtk::MenuButton::builder()
                    .icon_name("open-menu-symbolic")
                    .tooltip_text("Main menu")
                    .menu_model(&menu)
                    .primary(true)
                    .build(),
            );
            // While the pane slides over the list, it can be closed from
            // its header bar.
            let hide_folders = gtk::ToggleButton::builder()
                .icon_name("sidebar-show-symbolic")
                .tooltip_text("Hide folders")
                .active(true)
                .build();
            folders_header.pack_start(&hide_folders);
            let folders_view = adw::ToolbarView::new();
            folders_view.add_top_bar(&folders_header);
            folders_view.set_content(Some(&folders.widget));
            let compose_pill = gtk::Button::builder()
                .child(&adw::ButtonContent::builder().icon_name("mail-message-new-symbolic").label("New message").build())
                .action_name("win.compose")
                .tooltip_text("New message (Ctrl+N)")
                .margin_start(12)
                .margin_end(12)
                .margin_top(6)
                .margin_bottom(12)
                .css_classes(["pill", "suggested-action"])
                .build();
            folders_view.add_bottom_bar(&compose_pill);

            let list_header = adw::HeaderBar::builder().title_widget(&title).build();
            let toggle = gtk::ToggleButton::builder()
                .icon_name("sidebar-show-symbolic")
                .tooltip_text("Hide folders")
                .build();
            list_header.pack_start(&toggle);
            // Shows a spinner instead of its icon while a search runs.
            let search_icons = gtk::Stack::new();
            search_icons.add_named(&gtk::Image::from_icon_name("system-search-symbolic"), Some("icon"));
            search_icons.add_named(&adw::Spinner::new(), Some("spinner"));
            let search_button = gtk::ToggleButton::builder().child(&search_icons).tooltip_text("Search").build();
            // A custom child loses the icon-button styling the other
            // header buttons get from their icon names.
            search_button.add_css_class("image-button");
            list_header.pack_start(&search_button);
            let sort = gio::Menu::new();
            let order = gio::Menu::new();
            order.append(Some("_Newest first"), Some("win.order::newest"));
            order.append(Some("_Oldest first"), Some("win.order::oldest"));
            sort.append_section(None, &order);
            let group = gio::Menu::new();
            group.append(Some("Group by _thread"), Some("win.threads"));
            sort.append_section(None, &group);
            let select_toggle = gtk::ToggleButton::builder()
                .icon_name("selection-mode-symbolic")
                .tooltip_text("Select messages")
                .action_name("win.select-mode")
                .build();
            list_header.pack_end(&select_toggle);
            list_header.pack_end(
                &gtk::MenuButton::builder()
                    .icon_name("view-sort-descending-symbolic")
                    .tooltip_text("Sort and grouping")
                    .menu_model(&sort)
                    .build(),
            );
            let filters = gio::Menu::new();
            filters.append(Some("_All messages"), Some("win.filter::all"));
            filters.append(Some("_Unread"), Some("win.filter::unread"));
            filters.append(Some("_Starred"), Some("win.filter::starred"));
            filters.append(Some("With _attachments"), Some("win.filter::attachments"));
            let filter_button = gtk::MenuButton::builder()
                .icon_name("salutations-filter-symbolic")
                .tooltip_text("Show")
                .menu_model(&filters)
                .build();
            list_header.pack_end(&filter_button);
            // Selection mode adds a toolbar under the header bar (and under
            // the search bar when it is open).
            let select_menu = gio::Menu::new();
            select_menu.append(Some("Select _all"), Some("win.select-all"));
            select_menu.append(Some("Select _none"), Some("win.select-none"));
            let kinds = gio::Menu::new();
            kinds.append(Some("Select _read"), Some("win.select-read"));
            kinds.append(Some("Select _unread"), Some("win.select-unread"));
            kinds.append(Some("Select _starred"), Some("win.select-starred"));
            kinds.append(Some("Select un_starred"), Some("win.select-unstarred"));
            select_menu.append_section(None, &kinds);
            let select_count = gtk::MenuButton::builder().label("0 selected").menu_model(&select_menu).build();
            select_count.add_css_class("flat");
            let select_buttons = ActionButtons::new();
            let select_box = gtk::CenterBox::new();
            select_box.add_css_class("toolbar");
            select_box.set_start_widget(Some(&select_buttons.start));
            select_box.set_center_widget(Some(&select_count));
            select_box.set_end_widget(Some(&select_buttons.end));
            let select_bar = gtk::Revealer::builder()
                .transition_type(gtk::RevealerTransitionType::SlideDown)
                .child(&select_box)
                .build();
            let list_view = adw::ToolbarView::new();
            list_view.add_top_bar(&list_header);
            let search = Search::new();
            list_view.add_top_bar(&search.bar);
            list_view.add_top_bar(&select_bar);
            let pause_banner = adw::Banner::builder()
                .title("Syncing is paused for power save mode")
                .button_label("Preferences")
                .action_name("win.account-preferences")
                .build();
            list_view.add_top_bar(&pause_banner);
            let empty_banner = adw::Banner::builder().action_name("win.empty-folder").build();
            list_view.add_top_bar(&empty_banner);
            search
                .bar
                .bind_property("search-mode-enabled", &search_button, "active")
                .bidirectional()
                .sync_create()
                .build();
            let list_stack = gtk::Stack::new();
            // Over the list when the filter leaves nothing to show.
            let filter_empty = adw::StatusPage::builder()
                .icon_name("salutations-filter-symbolic")
                .child(
                    &gtk::Button::builder()
                        .label("Show all messages")
                        .action_name("win.filter")
                        .action_target(&"all".to_variant())
                        .halign(gtk::Align::Center)
                        .css_classes(["pill"])
                        .build(),
                )
                .visible(false)
                .build();
            filter_empty.add_css_class("compact");
            let list_overlay = gtk::Overlay::builder().child(&list.widget).build();
            list_overlay.add_overlay(&filter_empty);
            list_stack.add_named(&list_overlay, Some("list"));
            list_stack.add_named(&search.results, Some("results"));
            let compose_float = gtk::Button::builder()
                .icon_name("mail-message-new-symbolic")
                .action_name("win.compose")
                .tooltip_text("New message (Ctrl+N)")
                .halign(gtk::Align::End)
                .valign(gtk::Align::End)
                .margin_end(18)
                .margin_bottom(18)
                .css_classes(["circular", "suggested-action", "compose-float"])
                .build();
            // Files dropped on the list are imported into the open folder.
            let list_drop = gtk::DropTarget::new(gtk::gdk::FileList::static_type(), gtk::gdk::DragAction::COPY);
            list_drop.connect_drop(move |_, value, _, _| {
                let (Some(this), Ok(list)) = (w6.upgrade(), value.get::<gtk::gdk::FileList>()) else { return false };
                let open = this.open.borrow().as_ref().map(|o| (o.account.account.uid.clone(), o.obj.full_name()));
                this.import_files(list.files(), open);
                true
            });
            let list_overlay = gtk::Overlay::new();
            list_overlay.add_controller(list_drop);
            let (wa, wb, wc) = (weak.clone(), weak.clone(), weak.clone());
            let pull = crate::pull_refresh::PullRefresh::new(
                &list_stack,
                move || {
                    let this = wa.upgrade()?;
                    Some(if this.showing_results() { this.search.results.clone() } else { this.list.widget.clone() })
                },
                // Checked messages keep the list as it is.
                move || wb.upgrade().is_some_and(|this| !this.list.select_mode()),
                move || {
                    if let Some(this) = wc.upgrade() {
                        this.pull_refreshed();
                    }
                },
            );
            let list_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
            list_column.append(&pull.indicator);
            list_stack.set_vexpand(true);
            list_column.append(&list_stack);
            list_overlay.set_child(Some(&list_column));
            list_overlay.add_overlay(&compose_float);
            list_view.set_content(Some(&list_overlay));
            let list_page = adw::NavigationPage::builder()
                .title("Messages")
                .tag("messages")
                .child(&list_view)
                .build();

            let viewer_header = adw::HeaderBar::builder().show_title(false).build();
            let viewer_buttons = ActionButtons::new();
            viewer_header.pack_start(&viewer_buttons.start);
            let more = gio::Menu::new();
            more.append(Some("_Print…"), Some("win.print"));
            more.append(Some("_Save as file…"), Some("win.save-message"));
            let more = gtk::MenuButton::builder()
                .icon_name("view-more-symbolic")
                .tooltip_text("More")
                .menu_model(&more)
                .build();
            viewer_buttons.end.bind_property("visible", &more, "visible").sync_create().build();
            viewer_header.pack_end(&more);
            viewer_header.pack_end(&viewer_buttons.end);
            viewer_buttons.set_visible(false);
            let viewer_view = adw::ToolbarView::new();
            viewer_view.add_top_bar(&viewer_header);
            viewer_view.set_content(Some(&viewer.widget));
            let reply_bar = gtk::Box::builder().homogeneous(true).spacing(6).css_classes(["toolbar"]).build();
            let reply_menu = |all: bool| {
                let m = gio::Menu::new();
                let base = if all { "reply-all" } else { "reply" };
                m.append(Some("Plain text"), Some(&format!("win.{base}-plain")));
                m.append(Some("Markdown"), Some(&format!("win.{base}-markdown")));
                m
            };
            let reply_choice = |icon: &str, label: &str, action: &str, all: bool| {
                let stack = gtk::Stack::new();
                let (plain, plain_label) = crate::label_bar::content(icon, label, false);
                let (menu, menu_label) = crate::label_bar::content(icon, label, false);
                stack.add_named(
                    &gtk::Button::builder().child(&plain).action_name(action).tooltip_text(label).css_classes(["raised"]).build(),
                    Some("button"),
                );
                stack.add_named(
                    &gtk::MenuButton::builder()
                        .child(&menu)
                        .menu_model(&reply_menu(all))
                        .tooltip_text(label)
                        .css_classes(["raised"])
                        .build(),
                    Some("menu"),
                );
                (stack, vec![plain_label, menu_label])
            };
            let (reply_stack, reply_labels) = reply_choice("mail-reply-sender-symbolic", "Reply", "win.reply", false);
            let (reply_all_stack, reply_all_labels) =
                reply_choice("mail-reply-all-symbolic", "Reply all", "win.reply-all", true);
            reply_bar.append(&reply_stack);
            reply_bar.append(&reply_all_stack);
            let forward_menu = gio::Menu::new();
            forward_menu.append(Some("Inline"), Some("win.forward"));
            forward_menu.append(Some("As attachment"), Some("win.forward-attachment"));
            let (forward, forward_label) = crate::label_bar::content("mail-forward-symbolic", "Forward", false);
            reply_bar.append(
                &gtk::MenuButton::builder()
                    .child(&forward)
                    .menu_model(&forward_menu)
                    .tooltip_text("Forward")
                    .css_classes(["raised"])
                    .build(),
            );
            let reply_bar = crate::label_bar::LabelBar::new(&reply_bar, vec![reply_labels, vec![forward_label], reply_all_labels]);
            // Wide windows leave space at both ends rather than stretch the
            // buttons.
            viewer_view.add_bottom_bar(&adw::Clamp::builder().maximum_size(720).tightening_threshold(720).child(&reply_bar).build());
            viewer_view.set_reveal_bottom_bars(false);
            let composer = Composer::new();
            let contacts = salutations_core::contacts::Contacts::new(&registry);
            crate::people::start(contacts.clone());
            composer.set_contacts(contacts);
            let content_stack = gtk::Stack::new();
            content_stack.add_named(&viewer_view, Some("viewer"));
            content_stack.add_named(&composer.widget, Some("composer"));
            // The message list's width is dragged from the message's left
            // edge; the list's right edge holds its scrollbar.
            let handle = gtk::Box::builder()
                .width_request(8)
                .halign(gtk::Align::Start)
                .cursor(&gtk::gdk::Cursor::from_name("col-resize", None).expect("cursor"))
                .build();
            let viewer_overlay = gtk::Overlay::new();
            viewer_overlay.set_child(Some(&content_stack));
            viewer_overlay.add_overlay(&handle);
            let viewer_page = adw::NavigationPage::builder()
                .title("Message")
                .tag("message")
                .child(&viewer_overlay)
                .build();

            let inner = adw::NavigationSplitView::builder()
                .sidebar(&list_page)
                .content(&viewer_page)
                .min_sidebar_width(300.0)
                .max_sidebar_width(440.0)
                .sidebar_width_fraction(0.36)
                .build();
            if let Some(w) = Settings::get().list_width {
                set_list_width(&inner, w);
            }
            // Measured in the split view's coordinates: the handle moves with
            // the edge, so offsets relative to it would feed back.
            let drag = gtk::GestureDrag::new();
            let grab = Rc::new(Cell::new(0.0));
            {
                let grab = grab.clone();
                drag.connect_drag_begin(move |_, x, _| grab.set(x));
            }
            {
                let (inner, h) = (inner.clone(), handle.downgrade());
                drag.connect_drag_update(move |g, _, _| {
                    let (Some(handle), Some((x, _))) = (h.upgrade(), g.point(None)) else { return };
                    let Some(p) = handle.compute_point(&inner, &gtk::graphene::Point::new(x as f32, 0.0)) else { return };
                    let px = (p.x() as f64 - grab.get()).min(inner.width() as f64 * LIST_FRACTION);
                    set_list_width(&inner, adw::LengthUnit::Sp.from_px(px, gtk::Settings::default().as_ref()));
                });
            }
            {
                let inner = inner.clone();
                drag.connect_drag_end(move |_, _, _| {
                    let mut settings = Settings::get();
                    settings.list_width = Some(inner.max_sidebar_width());
                    let _ = settings.save();
                });
            }
            handle.add_controller(drag);
            // The message below the list: the same two parts, moved into a
            // vertical pane while the setting asks for it and the window is
            // wide.
            let paned = gtk::Paned::builder()
                .orientation(gtk::Orientation::Vertical)
                .shrink_start_child(false)
                .shrink_end_child(false)
                .wide_handle(true)
                .build();
            let layout = gtk::Stack::new();
            layout.add_named(&inner, Some("beside"));
            layout.add_named(&paned, Some("below"));
            let outer = adw::OverlaySplitView::builder()
                .sidebar(&folders_view)
                .content(&layout)
                .min_sidebar_width(200.0)
                .max_sidebar_width(300.0)
                .sidebar_width_fraction(0.2)
                .build();
            let toasts = adw::ToastOverlay::new();
            toasts.set_child(Some(&outer));
            window.set_content(Some(&toasts));
            outer
                .bind_property("show-sidebar", &toggle, "active")
                .bidirectional()
                .sync_create()
                .build();
            outer
                .bind_property("show-sidebar", &toggle, "tooltip-text")
                .transform_to(|_, shown: bool| Some(if shown { "Hide folders" } else { "Show folders" }))
                .sync_create()
                .build();
            outer
                .bind_property("show-sidebar", &hide_folders, "active")
                .bidirectional()
                .sync_create()
                .build();
            outer.bind_property("collapsed", &hide_folders, "visible").sync_create().build();

            // Below 900 sp the folder pane slides over the message list; below
            // 600 sp the list and the message are separate pages as well.
            // Collapsing hides the sidebar and expanding shows it again.
            let medium = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 900sp").expect("condition"));
            medium.add_setter(&outer, "collapsed", Some(&true.to_value()));
            let small = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 600sp").expect("condition"));
            small.add_setter(&outer, "collapsed", Some(&true.to_value()));
            small.add_setter(&inner, "collapsed", Some(&true.to_value()));
            window.add_breakpoint(medium);
            window.add_breakpoint(small);
            list_header.set_show_back_button(false);

            Window {
                window,
                registry,
                session,
                accounts: RefCell::new(Vec::new()),
                folders,
                list,
                viewer,
                title,
                outer,
                inner,
                open: RefCell::new(None),
                read_timer: RefCell::new(None),
                viewer_buttons,
                toasts,
                undo: RefCell::new(Vec::new()),
                toast: RefCell::new(None),
                search,
                notifier: Notifier::new(),
                search_icons,
                list_stack,
                search_cancel: RefCell::new(None),
                searching: Cell::new(0),
                search_gen: Cell::new(0),
                select_bar,
                select_count,
                select_buttons,
                select_toggle,
                search_button: search_button.clone(),
                opening: RefCell::new(None),
                pause_banner,
                empty_banner,
                pull,
                power_monitor: RefCell::new(None),
                sleep_watch: RefCell::new(None),
                composer,
                content_stack,
                compose: RefCell::new(None),
                viewer_view,
                reply_stack,
                reply_all_stack,
                compose_float,
                reply_when_shown: RefCell::new(None),
                gpg: gpg.clone(),
                layout,
                paned,
                list_page,
                viewer_page,
                list_side: list_view,
                viewer_side: viewer_overlay,
                viewer_header,
                width_handle: handle,
                divider_set: Cell::new(None),
                unread_before: RefCell::new(Vec::new()),
                queue: RefCell::new(Vec::new()),
                filter_button,
                filter_empty,
                working: RefCell::new(HashSet::new()),
            }
        });

        this.install_actions();
        this.connect_composer();
        this.connect_close_saving();
        this.apply_order();
        this.connect_parts();
        this.load_accounts();
        // The registry emits on the main context it was created on.
        let weak = glib::thread_guard::ThreadGuard::new(Rc::downgrade(&this));
        salutations_core::watch_mail_accounts(&this.registry, move |_| {
            if let Some(this) = weak.get_ref().upgrade() {
                this.load_accounts();
            }
        });
        this.watch_network_and_sleep();
        this.watch_power_and_metering();
        // Hidden while running in background: drop the shown page, so
        // WebKit's web process holds no message.
        let weak = Rc::downgrade(&this);
        this.window.connect_hide(move |_| {
            if let Some(this) = weak.upgrade() {
                this.close_composer();
                this.cancel_read_timer();
                this.list.unselect();
                this.viewer.clear();
                this.update_viewer_read();
            }
        });
        let weak = Rc::downgrade(&this);
        this.notifier.connect_action(move |action| {
            if let Some(this) = weak.upgrade() {
                this.notification_action(action);
            }
        });
        this
    }

    /// Before the process ends.
    pub fn shutdown(&self) {
        self.notifier.withdraw_all();
    }

    fn connect_parts(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.list.connect_mode_request(move |on| {
            if let Some(this) = weak.upgrade() {
                this.window.change_action_state("select-mode", &on.to_variant());
            }
        });
        let weak = Rc::downgrade(self);
        self.list.connect_checked(move || {
            if let Some(this) = weak.upgrade() {
                this.checked_changed();
            }
        });
        self.list.set_compact(self.inner.is_collapsed());
        self.connect_layout();
        let weak = Rc::downgrade(self);
        self.inner.connect_collapsed_notify(move |inner| {
            if let Some(this) = weak.upgrade() {
                this.list.set_compact(inner.is_collapsed());
                // Narrowing keeps an open message or the composer in view;
                // the split view otherwise keeps whatever page it last showed.
                if inner.is_collapsed() {
                    let open = !this.list.select_mode() && (this.viewer.current().is_some() || this.compose.borrow().is_some());
                    inner.set_show_content(open);
                }
            }
        });
        // Going back to the list on a phone ends the reading: the
        // mark-as-read timer stops, and the row can be tapped again to
        // reopen the message.
        let weak = Rc::downgrade(self);
        self.inner.connect_show_content_notify(move |inner| {
            if let Some(this) = weak.upgrade()
                && inner.is_collapsed()
                && !inner.shows_content()
            {
                this.close_composer();
                this.cancel_read_timer();
                this.list.unselect();
                this.viewer.forget();
                this.update_viewer_read();
            }
        });
        // On a phone, a message pane with no message (it was deleted, or
        // the list let go of it) goes back to the list. The split view has
        // no such rule.
        let weak = Rc::downgrade(self);
        self.inner.connect_show_content_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.leave_empty_viewer();
            }
        });
        let weak = Rc::downgrade(self);
        self.viewer.widget.connect_visible_child_name_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.leave_empty_viewer();
            }
        });
        let weak = Rc::downgrade(self);
        self.outer.connect_show_sidebar_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.update_compose_float();
            }
        });
        self.update_compose_float();
        let weak = Rc::downgrade(self);
        let w = Rc::downgrade(self);
        self.list.set_hidden_filter(move |folder, uid| w.upgrade().is_some_and(|this| this.is_hidden(folder, uid)));
        self.list.connect_open(move |target| {
            let Some(this) = weak.upgrade() else { return };
            this.cancel_read_timer();
            match target {
                Some((folder, uid)) if this.is_drafts(&folder) => this.open_draft(folder, uid),
                Some((folder, uid)) => {
                    // Selecting a message saves and closes the composer.
                    this.close_composer();
                    this.viewer.show(folder, uid);
                    if this.inner.is_collapsed() {
                        this.inner.set_show_content(true);
                    }
                }
                None => this.viewer.clear(),
            }
            this.update_viewer_read();
        });
        let weak = Rc::downgrade(self);
        self.list.connect_activate(move || {
            if let Some(this) = weak.upgrade()
                && this.inner.is_collapsed()
            {
                this.inner.set_show_content(true);
            }
        });
        let weak = Rc::downgrade(self);
        self.search.bar.connect_search_mode_enabled_notify(move |bar| {
            let Some(this) = weak.upgrade() else { return };
            if bar.is_search_mode() {
                this.search.entry.grab_focus();
            } else {
                this.search.entry.set_text("");
                this.run_search(String::new(), search::Scope::Folder);
            }
        });
        let weak = Rc::downgrade(self);
        self.search.connect_query(move |q, scope| {
            if let Some(this) = weak.upgrade() {
                this.run_search(q, scope);
            }
        });
        let weak = Rc::downgrade(self);
        self.search.connect_checked(move || {
            if let Some(this) = weak.upgrade() {
                this.checked_changed();
            }
        });
        let weak = Rc::downgrade(self);
        self.search.connect_open(move |folder, uid| {
            let Some(this) = weak.upgrade() else { return };
            if this.is_drafts(&folder) {
                this.open_draft(folder, uid);
                return;
            }
            this.close_composer();
            this.cancel_read_timer();
            this.list.unselect();
            this.viewer.show(folder, uid);
            if this.inner.is_collapsed() {
                this.inner.set_show_content(true);
            }
            this.update_viewer_read();
        });
        let weak = Rc::downgrade(self);
        self.list.connect_drag(move |uids, thread| {
            let this = weak.upgrade()?;
            let open = this.open.borrow();
            let o = open.as_ref()?;
            let m = DraggedMessages { account_uid: o.account.obj.account_uid(), folder: o.folder.clone(), uids, thread };
            Some(gtk::gdk::ContentProvider::for_value(&m.to_value()))
        });
        if let Some(contacts) = self.composer.contacts() {
            let (w1, w2) = (Rc::downgrade(self), Rc::downgrade(self));
            let ctx = Rc::new(crate::contact_menu::Ctx {
                contacts,
                compose: Box::new(move |to| {
                    if let Some(this) = w1.upgrade() {
                        let to = to.to_string();
                        this.compose_external(Some(salutations_core::mailto::Mailto { to, ..Default::default() }));
                    }
                }),
                toast: Box::new(move |text| {
                    if let Some(this) = w2.upgrade() {
                        this.toast(text, false);
                    }
                }),
            });
            self.viewer.set_contact_ctx(ctx.clone());
            self.composer.set_contact_ctx(ctx);
        }
        let weak = Rc::downgrade(self);
        self.viewer.connect_shown(move |folder, uid| {
            if let Some(this) = weak.upgrade() {
                this.start_read_timer(folder, uid);
                this.update_viewer_read();
                this.consider_keys(folder);
                this.show_responses(folder, uid);
                let pending = this.reply_when_shown.borrow_mut().take();
                if pending.is_some_and(|(f, u)| &f == folder && u == uid) {
                    gtk::prelude::ActionGroupExt::activate_action(&this.window, "reply", None);
                }
            }
        });
    }

    fn install_actions(self: &Rc<Self>) {
        let win = &self.window;
        // Not application accelerators: those run before a focused text
        // field sees the key, so "m" could not be typed into search.
        let keys = gtk::EventControllerKey::new();
        let w = win.downgrade();
        keys.connect_key_pressed(move |_, key, _, mods| {
            use gtk::gdk::{Key, ModifierType as M};
            let Some(win) = w.upgrade() else { return glib::Propagation::Proceed };
            let mods = mods & (M::CONTROL_MASK | M::SHIFT_MASK | M::ALT_MASK | M::SUPER_MASK);
            let typing = gtk::prelude::GtkWindowExt::focus(&win).is_some_and(|f| f.is::<gtk::Editable>() || f.is::<gtk::TextView>());
            let name = match (key.to_lower(), mods) {
                (Key::n, M::CONTROL_MASK) => "compose",
                (Key::r, M::CONTROL_MASK) => "reply",
                (Key::r, m) if m == M::CONTROL_MASK | M::SHIFT_MASK => "reply-all",
                (Key::f, m) if m == M::CONTROL_MASK | M::SHIFT_MASK => "forward",
                _ if typing => return glib::Propagation::Proceed,
                (Key::m, m) if m.is_empty() => "toggle-read",
                (Key::a, m) if m.is_empty() => "archive",
                (Key::Delete | Key::KP_Delete, m) if m.is_empty() => "delete",
                (Key::Delete | Key::KP_Delete, M::SHIFT_MASK) => "delete-permanently",
                (Key::z, M::CONTROL_MASK) => "undo",
                _ => return glib::Propagation::Proceed,
            };
            match win.lookup_action(name) {
                Some(a) if a.is_enabled() => {
                    a.activate(None);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        win.add_controller(keys);
        // Escape leaves selection mode before it closes search.
        let escape = gtk::EventControllerKey::new();
        escape.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        escape.connect_key_pressed(move |_, key, _, mods| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if key == gtk::gdk::Key::Escape && mods.is_empty() && this.list.select_mode() {
                this.window.change_action_state("select-mode", &false.to_variant());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        win.add_controller(escape);
        let weak = Rc::downgrade(self);
        let toggle_read = gio::SimpleAction::new("toggle-read", None);
        toggle_read.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.toggle_read();
            }
        });
        win.add_action(&toggle_read);

        for (name, op) in [
            ("delete", Op::Delete),
            ("trash", Op::Trash),
            ("delete-permanently", Op::DeletePermanently),
            ("move", Op::Move),
            ("archive", Op::Archive),
            ("toggle-star", Op::Star),
            ("restore", Op::Restore),
            ("junk", Op::Junk),
            ("not-junk", Op::NotJunk),
        ] {
            let weak = Rc::downgrade(self);
            let a = gio::SimpleAction::new(name, None);
            a.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    let targets = this.current_targets();
                    if !targets.is_empty() {
                        this.request_all(op, targets);
                    }
                }
            });
            win.add_action(&a);
        }
        let weak = Rc::downgrade(self);
        let search = gio::SimpleAction::new("search", None);
        search.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.search.bar.set_search_mode(true);
                this.search.entry.grab_focus();
            }
        });
        win.add_action(&search);
        let weak = Rc::downgrade(self);
        let undo = gio::SimpleAction::new("undo", None);
        undo.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.undo();
            }
        });
        win.add_action(&undo);

        let weak = Rc::downgrade(self);
        let toggle_folders = gio::SimpleAction::new("toggle-folders", None);
        toggle_folders.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.outer.set_show_sidebar(!this.outer.shows_sidebar());
            }
        });
        win.add_action(&toggle_folders);

        let weak = Rc::downgrade(self);
        let sync = gio::SimpleAction::new("sync", None);
        sync.connect_activate(move |_, _| {
            let Some(this) = weak.upgrade() else { return };
            let ctl = this.open.borrow().as_ref().map(|o| o.account.clone());
            if let Some(ctl) = ctl.or_else(|| this.accounts.borrow().first().cloned()) {
                this.sync(ctl);
            }
        });
        win.add_action(&sync);

        let weak = Rc::downgrade(self);
        let view = Settings::get();
        let order = gio::SimpleAction::new_stateful(
            "order",
            Some(glib::VariantTy::STRING),
            &(if view.newest_first { "newest" } else { "oldest" }).to_variant(),
        );
        order.connect_change_state(move |a, v| {
            let Some(v) = v else { return };
            a.set_state(v);
            if let Some(this) = weak.upgrade() {
                this.apply_order();
            }
        });
        win.add_action(&order);
        let weak = Rc::downgrade(self);
        let threads = gio::SimpleAction::new_stateful("threads", None, &view.threaded.to_variant());
        threads.connect_change_state(move |a, v| {
            let Some(v) = v else { return };
            a.set_state(v);
            if let Some(this) = weak.upgrade() {
                this.apply_order();
            }
        });
        win.add_action(&threads);
        let weak = Rc::downgrade(self);
        let filter = gio::SimpleAction::new_stateful("filter", Some(glib::VariantTy::STRING), &"all".to_variant());
        filter.connect_change_state(move |a, v| {
            let Some(v) = v else { return };
            a.set_state(v);
            if let Some(this) = weak.upgrade() {
                let name = v.get::<String>().unwrap_or_default();
                this.list.set_filter(crate::message_list::Filter::from_name(&name));
                this.update_filter();
            }
        });
        win.add_action(&filter);
        let weak = Rc::downgrade(self);
        self.viewer.set_account_address(move |folder| {
            let this = weak.upgrade()?;
            this.account_of(folder).and_then(|c| c.account.address.clone())
        });
        let weak = Rc::downgrade(self);
        self.list.connect_quick(move |action, uids, thread| {
            use crate::message_list::Quick;
            let Some(this) = weak.upgrade() else { return };
            let Some((account, folder)) = this.open.borrow().as_ref().map(|o| (o.account.clone(), o.folder.clone())) else {
                return;
            };
            let op = match action {
                Quick::Read => Op::Read,
                Quick::Star => Op::Star,
                Quick::Archive => Op::Archive,
                Quick::Delete => Op::Delete,
            };
            this.request(op, Target { account, folder, uids, thread });
        });
        let weak = Rc::downgrade(self);
        self.list.connect_rows(move |_| {
            if let Some(this) = weak.upgrade() {
                this.update_filter_empty();
            }
        });

        let weak = Rc::downgrade(self);
        let prefs = gio::SimpleAction::new("preferences", None);
        prefs.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.show_preferences(None);
            }
        });
        win.add_action(&prefs);
        let weak = Rc::downgrade(self);
        let import = gio::SimpleAction::new("import", None);
        import.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.choose_import_files();
            }
        });
        win.add_action(&import);
        // The pause banner's button: the open folder's account.
        let weak = Rc::downgrade(self);
        let prefs = gio::SimpleAction::new("account-preferences", None);
        prefs.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                let uid = this.open.borrow().as_ref().map(|o| o.account.account.uid.clone());
                this.show_preferences(uid.as_deref());
            }
        });
        win.add_action(&prefs);
        // The empty banner's button: the open Trash or Junk.
        let weak = Rc::downgrade(self);
        let empty = gio::SimpleAction::new("empty-folder", None);
        empty.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                let open = this.open.borrow().as_ref().map(|o| (o.account.clone(), o.obj.full_name()));
                if let Some((ctl, name)) = open {
                    this.empty_folder(&ctl, &name);
                }
            }
        });
        win.add_action(&empty);
        for (name, f) in [("print", Viewer::print as fn(&Viewer)), ("save-message", Viewer::save_message)] {
            let weak = Rc::downgrade(self);
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    f(&this.viewer);
                }
            });
            win.add_action(&action);
        }

        let weak = Rc::downgrade(self);
        let select = gio::SimpleAction::new_stateful("select-mode", None, &false.to_variant());
        select.connect_change_state(move |a, v| {
            let Some(on) = v.and_then(|v| v.get::<bool>()) else { return };
            if a.state().and_then(|s| s.get::<bool>()) == Some(on) {
                return;
            }
            a.set_state(&on.to_variant());
            if let Some(this) = weak.upgrade() {
                if on {
                    this.close_composer();
                }
                this.apply_select_mode(on);
                this.update_compose_float();
            }
        });
        win.add_action(&select);
        for (name, on) in [("select-all", true), ("select-none", false)] {
            let weak = Rc::downgrade(self);
            let a = gio::SimpleAction::new(name, None);
            a.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    if this.selecting_results() {
                        this.search.set_all_checked(on);
                    } else {
                        this.list.set_all_checked(on);
                    }
                }
            });
            win.add_action(&a);
        }
        use crate::message_list::Kind;
        for (name, kind) in
            [("select-read", Kind::Read), ("select-unread", Kind::Unread), ("select-starred", Kind::Starred), ("select-unstarred", Kind::Unstarred)]
        {
            let weak = Rc::downgrade(self);
            let a = gio::SimpleAction::new(name, None);
            a.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    if this.selecting_results() {
                        this.search.check_kind(kind);
                    } else {
                        this.list.check_kind(kind);
                    }
                }
            });
            win.add_action(&a);
        }
    }

    fn show_preferences(self: &Rc<Self>, open_account: Option<&str>) {
        fn flatten(store: &gio::ListStore, prefix: &str, out: &mut Vec<(String, String)>) {
            for i in 0..store.n_items() {
                let Some(o) = store.item(i).and_downcast::<FolderObject>() else { continue };
                let path = if prefix.is_empty() { o.title() } else { format!("{prefix} / {}", o.title()) };
                out.push((o.full_name(), path.clone()));
                flatten(o.children(), &path, out);
            }
        }
        let entries = self
            .accounts
            .borrow()
            .iter()
            .map(|ctl| {
                let mut folders = Vec::new();
                flatten(ctl.obj.children(), "", &mut folders);
                crate::preferences::AccountEntry {
                    account: ctl.account.clone(),
                    store: ctl.store.borrow().clone(),
                    folders,
                }
            })
            .collect();
        let weak = Rc::downgrade(self);
        let on_offline: crate::preferences::AccountChanged = Rc::new(move |uid: &str, change| {
            let Some(this) = weak.upgrade() else { return };
            let ctl = this.accounts.borrow().iter().find(|c| c.account.uid == uid).cloned();
            let Some(ctl) = ctl else { return };
            match change {
                crate::preferences::AccountChange::Offline => {
                    this.apply_conditions(&ctl);
                    this.maintain(&ctl, true, None);
                }
                crate::preferences::AccountChange::Refresh => this.schedule_refresh(&ctl),
                crate::preferences::AccountChange::Order => this.load_accounts(),
                // A paused account takes the setting when the pause ends.
                crate::preferences::AccountChange::Push => {
                    this.schedule_keepalive(&ctl);
                    if !this.paused(&ctl) {
                        this.reconnect(&ctl);
                    }
                }
                crate::preferences::AccountChange::Sync => {
                    this.schedule_refresh(&ctl);
                    this.apply_conditions(&ctl);
                    this.maintain(&ctl, true, None);
                }
            }
        });
        crate::preferences::show(&self.window, &self.registry, entries, on_offline, open_account);
    }

    /// Brings the account's offline copies in line with its settings, in
    /// the background: refreshes and downloads the folders kept offline,
    /// deletes bodies that fell out of the window, and deletes all bodies of
    /// folders no longer kept. `full` (after a settings change) checks every
    /// message against the window instead of only those that crossed its
    /// edge since the last run.
    /// `refreshed` names a folder the caller has just refreshed, which the
    /// run does not refresh again.
    fn maintain(self: &Rc<Self>, ctl: &Rc<AccountCtl>, full: bool, refreshed: Option<String>) {
        let Some(store) = ctl.store.borrow().clone() else { return };
        if store.connection_status() != camel::ServiceConnectionStatus::Connected || self.paused(ctl) {
            return;
        }
        if ctl.maintaining.replace(true) {
            // The rerun refreshes everything, the skipped folder included.
            let again = ctl.maintain_again.get().unwrap_or(false) || full;
            ctl.maintain_again.set(Some(again));
            return;
        }
        let settings = offline::read(&self.registry, &ctl.account).unwrap_or(offline::DEFAULT);
        let prefs = Settings::get().sync_for(&ctl.account.uid);
        let metered = metered();
        let now = glib::DateTime::now_utc().ok();
        let expression = self.download_plan(ctl, &prefs, settings.window);
        // Only the window itself deletes, and not on metered connections,
        // where a narrower window only limits downloading.
        let cutoff = if metered || prefs.never_download {
            None
        } else {
            now.as_ref().and_then(|now| settings.window.cutoff(now))
        };
        let since = if full { None } else { ctl.last_cutoff.get() };
        let previous = ctl.synced.borrow().clone();
        let (this, ctl) = (self.clone(), ctl.clone());
        glib::spawn_future_local(async move {
            let s = store.clone();
            let result = gio::spawn_blocking(move || {
                let Some(o) = s.dynamic_cast_ref::<camel::OfflineStore>() else { return (HashSet::new(), 0, 0) };
                let folders = o.dup_downsync_folders();
                let names: HashSet<String> =
                    folders.iter().filter_map(|f| f.full_name().map(|n| n.to_string())).collect();
                let mut pruned = 0;
                for f in &folders {
                    // Camel downloads only messages its summary knows, so
                    // the folder is refreshed first.
                    if let Some(of) = f.dynamic_cast_ref::<camel::OfflineFolder>()
                        && let Some(expression) = &expression
                    {
                        let fresh = refreshed.is_some() && f.full_name().as_deref() == refreshed.as_deref();
                        let refresh = if fresh { Ok(()) } else { f.refresh_info_sync(None::<&gio::Cancellable>) };
                        let _ = refresh.and_then(|()| of.downsync_sync(expression.as_deref(), None::<&gio::Cancellable>));
                    }
                    if let Some(c) = cutoff {
                        pruned += offline::prune(f, c, since);
                    }
                }
                let mut cleared = 0;
                for name in previous.difference(&names) {
                    if let Ok(Some(f)) = s.folder_sync(name, camel::StoreGetFolderFlags::empty(), None::<&gio::Cancellable>) {
                        offline::clear_folder(&f);
                        cleared += 1;
                    }
                }
                (names, pruned, cleared)
            })
            .await;
            ctl.maintaining.set(false);
            if let Ok((names, pruned, cleared)) = result {
                crate::debug::log(&format!(
                    "{}: offline maintenance: {} folders kept, {pruned} bodies pruned, {cleared} folders cleared",
                    ctl.account.uid,
                    names.len()
                ));
                *ctl.synced.borrow_mut() = names;
                // Unpruned runs leave the edge where the last pruning put it.
                if cutoff.is_some() {
                    ctl.last_cutoff.set(cutoff);
                }
            }
            if let Some(full) = ctl.maintain_again.take() {
                this.maintain(&ctl, full, None);
            }
        });
    }

    /// First sync of the session: applies the defaults to an account
    /// Salutations has not configured, then starts the check timer.
    async fn first_sync_setup(&self, ctl: &Rc<AccountCtl>, store: &camel::Store) {
        let uid = &ctl.account.uid;
        if !offline::configured(uid, "offline") {
            let written = offline::write(&self.registry, &ctl.account, &offline::DEFAULT);
            if let Ok(inbox) = self.session.folder_future(store, "INBOX").await {
                offline::set_folder_synced(&inbox, true);
            }
            match written {
                Ok(()) => offline::mark_configured(uid, "offline"),
                Err(e) => glib::g_warning!("salutations", "Cannot save offline settings: {}", e),
            }
        }
        if !offline::configured(uid, "refresh") {
            match offline::write_refresh(&self.registry, &ctl.account, offline::DEFAULT_REFRESH) {
                Ok(()) => offline::mark_configured(uid, "refresh"),
                Err(e) => glib::g_warning!("salutations", "Cannot save the check interval: {}", e),
            }
        }
    }

    /// (Re)starts the account's check timer from its setting.
    fn schedule_refresh(self: &Rc<Self>, ctl: &Rc<AccountCtl>) {
        if let Some(id) = ctl.timer.borrow_mut().take() {
            id.remove();
        }
        self.schedule_keepalive(ctl);
        if self.paused(ctl) {
            return;
        }
        let offline::Refresh::Minutes(m) = self.refresh_now(ctl) else { return };
        let (weak, ctl_weak) = (Rc::downgrade(self), Rc::downgrade(ctl));
        let id = glib::timeout_add_seconds_local(m.saturating_mul(60), move || {
            let (Some(this), Some(ctl)) = (weak.upgrade(), ctl_weak.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            this.sync(ctl);
            glib::ControlFlow::Continue
        });
        *ctl.timer.borrow_mut() = Some(id);
    }

    /// Whether the account does nothing automatic now: Power Saver is on and
    /// the account pauses for it.
    fn paused(&self, ctl: &AccountCtl) -> bool {
        power_saver() && Settings::get().sync_for(&ctl.account.uid).pause_power_save
    }

    /// The check interval for the connection in use.
    fn refresh_now(&self, ctl: &AccountCtl) -> offline::Refresh {
        let normal = offline::read_refresh(&self.registry, &ctl.account).unwrap_or(offline::DEFAULT_REFRESH);
        let metered_refresh = Settings::get().sync_for(&ctl.account.uid).metered_refresh;
        match metered_refresh {
            Some(r) if metered() => r,
            _ => normal,
        }
    }

    /// "Download From" for the connection in use.
    fn download_now(&self, ctl: &AccountCtl, prefs: &salutations_core::SyncPrefs) -> offline::Download {
        let normal = if prefs.never_download {
            offline::Download::Never
        } else {
            offline::Download::Within(offline::read(&self.registry, &ctl.account).unwrap_or(offline::DEFAULT).window)
        };
        // A metered window never reaches past the normal one.
        match (prefs.metered_download, glib::DateTime::now_utc()) {
            (Some(d), Ok(now)) if metered() => d.min(normal, &now),
            _ => normal,
        }
    }

    /// Applies Power Saver and the connection to the account: holds push
    /// and Camel's downloads while it is paused, and Camel's downloads
    /// while it downloads nothing. Leaving the pause syncs once.
    fn apply_conditions(self: &Rc<Self>, ctl: &Rc<AccountCtl>) {
        let paused = self.paused(ctl);
        let was = ctl.paused.replace(paused);
        self.apply_holds(ctl);
        if paused && !was {
            crate::debug::log(&format!("{}: paused for Power Saver", ctl.account.uid));
            if let Some(id) = ctl.retry.borrow_mut().take() {
                id.remove();
            }
            if let Some(id) = ctl.timer.borrow_mut().take() {
                id.remove();
            }
            if let Some(id) = ctl.keepalive.borrow_mut().take() {
                id.remove();
            }
            if let Some(tx) = ctl.abort.borrow_mut().take() {
                let _ = tx.send(());
            }
            // Closing the connection ends push.
            if let Some(store) = ctl.store.borrow().clone() {
                glib::spawn_future_local(async move {
                    let _ = glib::future_with_timeout(
                        std::time::Duration::from_secs(5),
                        store.disconnect_future(true, glib::ffi::G_PRIORITY_DEFAULT),
                    )
                    .await;
                });
            }
        } else if was && !paused {
            crate::debug::log(&format!("{}: resumed after Power Saver", ctl.account.uid));
            self.schedule_refresh(ctl);
            self.resync(ctl, true);
        }
        self.update_pause_banner();
    }

    /// What maintenance downloads: None for nothing, Some(None) for Camel's
    /// window, Some(expression) for a narrower downsync expression.
    fn download_plan(&self, ctl: &AccountCtl, prefs: &salutations_core::SyncPrefs, window: offline::Window) -> Option<Option<String>> {
        match self.download_now(ctl, prefs) {
            offline::Download::Never => None,
            offline::Download::Within(w) => {
                let now = glib::DateTime::now_utc().ok()?;
                Some(w.cutoff(&now).and_then(|c| offline::narrower(window, c, &now)))
            }
        }
    }

    /// Camel downloads by the account's window, so its downloads are held
    /// whenever another applies; Salutations downloads instead.
    fn camel_downloads_held(&self, ctl: &AccountCtl, prefs: &salutations_core::SyncPrefs) -> bool {
        self.paused(ctl)
            || self.download_now(ctl, prefs) == offline::Download::Never
            || (metered() && prefs.metered_download.is_some())
    }

    fn apply_holds(&self, ctl: &AccountCtl) {
        let paused = self.paused(ctl);
        offline::hold_push(&self.registry, &ctl.account, paused);
        let prefs = Settings::get().sync_for(&ctl.account.uid);
        let hold = self.camel_downloads_held(ctl, &prefs);
        if let Some(store) = ctl.store.borrow().as_ref() {
            salutations_core::downsync::hold(store, hold);
        }
    }

    /// New INBOX messages, downloaded by the metered window while Camel's
    /// downloads are held for it. Other folders wait for the next sync.
    fn download_arrivals(&self, ctl: &AccountCtl, folder: &camel::Folder) {
        let prefs = Settings::get().sync_for(&ctl.account.uid);
        if self.paused(ctl) || !self.camel_downloads_held(ctl, &prefs) {
            return;
        }
        let window = offline::read(&self.registry, &ctl.account).unwrap_or(offline::DEFAULT).window;
        let Some(expression) = self.download_plan(ctl, &prefs, window) else { return };
        let Some(of) = folder.dynamic_cast_ref::<camel::OfflineFolder>().cloned() else { return };
        gio::spawn_blocking(move || {
            let _ = of.downsync_sync(expression.as_deref(), None::<&gio::Cancellable>);
        });
    }

    /// Trash or Junk, for the account's folder `full_name`.
    fn emptiable(ctl: &AccountCtl, full_name: &str) -> Option<&'static str> {
        let special = ctl.special.borrow();
        let s = special.as_ref()?;
        if s.trash.as_deref() == Some(full_name) {
            Some("Trash")
        } else if s.junk.as_deref() == Some(full_name) {
            Some("Junk")
        } else {
            None
        }
    }

    fn update_empty_banner(&self) {
        let open = self.open.borrow();
        let shown = open.as_ref().and_then(|o| {
            let name = Self::emptiable(&o.account, &o.obj.full_name())?;
            let count = o.folder.uids().iter().filter(|u| o.folder.message_flags(u) & DELETED == 0).count();
            (count > 0).then_some((name, count))
        });
        if let Some((name, count)) = shown {
            self.empty_banner.set_title(&format!("{count} message{} in {name}", if count == 1 { "" } else { "s" }));
            self.empty_banner.set_button_label(Some(&format!("Empty {name}")));
        }
        self.empty_banner.set_revealed(shown.is_some());
    }

    /// The folder menu: Empty Trash or Empty Junk on those folders.
    fn folder_menu(self: &Rc<Self>, obj: &FolderObject, widget: &gtk::Widget, x: f64, y: f64) {
        let Some(ctl) = self.accounts.borrow().iter().find(|c| c.account.uid == obj.account_uid()).cloned() else { return };
        let Some(name) = Self::emptiable(&ctl, &obj.full_name()) else { return };
        let menu = gio::Menu::new();
        let group = gio::SimpleActionGroup::new();
        let empty = gio::SimpleAction::new("empty", None);
        let (weak, obj) = (Rc::downgrade(self), obj.clone());
        empty.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.empty_folder(&ctl, &obj.full_name());
            }
        });
        group.add_action(&empty);
        menu.append(Some(&format!("Empty {name}…")), Some("folder.empty"));
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.insert_action_group("folder", Some(&group));
        popover.set_parent(widget);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    /// Deletes every message in the account's Trash or Junk, after asking.
    fn empty_folder(self: &Rc<Self>, ctl: &Rc<AccountCtl>, full_name: &str) {
        let Some(name) = Self::emptiable(ctl, full_name) else { return };
        let (this, ctl, full_name) = (self.clone(), ctl.clone(), full_name.to_string());
        glib::spawn_future_local(async move {
            let Ok(store) = this.store_for(&ctl) else { return };
            let folder = match this.session.folder_future(&store, &full_name).await {
                Ok(f) => f,
                Err(e) => return this.toast(&format!("Cannot open {name}: {}", e.message()), false),
            };
            let uids: Vec<String> =
                folder.uids().iter().map(|u| u.to_string()).filter(|u| folder.message_flags(u) & DELETED == 0).collect();
            if uids.is_empty() {
                return;
            }
            let n = uids.len();
            let dialog = adw::AlertDialog::builder()
                .heading(format!("Empty {name}?"))
                .body(format!(
                    "{} in {name} will be deleted permanently. This cannot be undone.",
                    if n == 1 { "The message".to_string() } else { format!("All {n} messages") }
                ))
                .build();
            dialog.add_responses(&[("cancel", "_Cancel"), ("empty", &format!("_Empty {name}"))]);
            dialog.set_response_appearance("empty", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            if dialog.choose_future(Some(&this.window)).await != "empty" {
                return;
            }
            let t = Target { account: ctl.clone(), folder: folder.clone(), uids, thread: false };
            if this.shows_any(&t) {
                this.cancel_read_timer();
                this.after_removal(None);
            }
            this.search.remove(&folder, &t.uids);
            folder.freeze();
            for uid in &t.uids {
                folder.set_message_flags(uid, DELETED | SEEN, DELETED | SEEN);
            }
            folder.thaw();
            if let Err(e) = folder.synchronize_future(true, glib::ffi::G_PRIORITY_DEFAULT).await {
                this.toast(&format!("Cannot empty {name}: {}", e.message()), false);
            }
            this.update_empty_banner();
        });
    }

    fn update_pause_banner(&self) {
        let paused = self.open.borrow().as_ref().is_some_and(|o| self.paused(&o.account));
        self.pause_banner.set_revealed(paused);
    }

    /// Power Saver and metered connections change how each account syncs.
    fn watch_power_and_metering(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let power = gio::PowerProfileMonitor::get_default();
        power.connect_power_saver_enabled_notify(move |_| {
            let Some(this) = weak.upgrade() else { return };
            let accounts = this.accounts.borrow().clone();
            for ctl in &accounts {
                this.apply_conditions(ctl);
            }
        });
        *self.power_monitor.borrow_mut() = Some(power);
        let weak = Rc::downgrade(self);
        gio::NetworkMonitor::default().connect_network_metered_notify(move |monitor| {
            let Some(this) = weak.upgrade() else { return };
            crate::debug::log(&format!("network metered: {}", monitor.is_network_metered()));
            let accounts = this.accounts.borrow().clone();
            for ctl in &accounts {
                this.schedule_refresh(ctl);
                this.apply_conditions(ctl);
                // Off a metered connection, the account's window downloads and
                // deletes again.
                if !monitor.is_network_metered() {
                    this.maintain(ctl, false, None);
                }
            }
        });
    }

    /// Camel does not notice a push connection that went silent (a dropped
    /// route, a NAT that forgot it), and no mail arrives until the next
    /// check. While push is on, INBOX is checked every so often, which
    /// fails on a dead connection and so reconnects; a sync in the
    /// meantime counts as the check.
    fn schedule_keepalive(self: &Rc<Self>, ctl: &Rc<AccountCtl>) {
        if let Some(id) = ctl.keepalive.borrow_mut().take() {
            id.remove();
        }
        if self.paused(ctl) || !offline::read_push(&self.registry, &ctl.account) {
            return;
        }
        let minutes = if metered() { KEEPALIVE_METERED_MINUTES } else { KEEPALIVE_MINUTES };
        let (weak, c) = (Rc::downgrade(self), Rc::downgrade(ctl));
        let id = glib::timeout_add_seconds_local(minutes * 60, move || {
            let (Some(this), Some(ctl)) = (weak.upgrade(), c.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            let recent = ctl.last_sync.get().is_some_and(|t| t.elapsed().as_secs() < u64::from(minutes) * 60 - 5);
            let inbox = ctl.inbox.borrow().as_ref().map(|(f, _)| f.clone());
            if !recent
                && ctl.obj.sync_state() == SyncState::Idle
                && let Some(inbox) = inbox
            {
                glib::spawn_future_local(async move {
                    let check = within(STEP_LIMIT, inbox.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT)).await;
                    if let Err(e) = check {
                        crate::debug::log(&format!("{}: push check failed: {}", ctl.account.uid, e.message()));
                        this.resync(&ctl, true);
                    }
                });
            }
            glib::ControlFlow::Continue
        });
        *ctl.keepalive.borrow_mut() = Some(id);
    }

    /// Push stops when the connection drops, so every account is synced when
    /// the network returns and after the computer wakes, without waiting for
    /// its timer.
    fn watch_network_and_sleep(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        gio::NetworkMonitor::default().connect_network_changed(move |_, available| {
            let Some(this) = weak.upgrade().filter(|_| available) else { return };
            let accounts = this.accounts.borrow().clone();
            for ctl in accounts.into_iter().filter(|c| !this.paused(c)) {
                let recent = |t: Option<std::time::Instant>| t.is_some_and(|t| t.elapsed().as_secs() < NETWORK_RESYNC_SECONDS);
                // A sync that just started (at launch, say) already uses the
                // network as it is now.
                let starting = ctl.obj.sync_state() == SyncState::Syncing && recent(ctl.sync_started.get());
                // Before the first sync (the monitor reports at launch too),
                // the sync on its way covers it.
                let first = ctl.last_sync.get().is_none() && !matches!(ctl.obj.sync_state(), SyncState::Offline | SyncState::Error);
                if !recent(ctl.last_sync.get()) && !starting && !first {
                    this.resync(&ctl, true);
                }
            }
        });
        // logind's PrepareForSleep(false) is sent on waking. A Flatpak needs
        // --system-talk-name=org.freedesktop.login1 for it.
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(bus) = gio::bus_get_future(gio::BusType::System).await else { return };
            let watch_weak = weak.clone();
            let sub = bus.subscribe_to_signal(
                Some("org.freedesktop.login1"),
                Some("org.freedesktop.login1.Manager"),
                Some("PrepareForSleep"),
                Some("/org/freedesktop/login1"),
                None,
                gio::DBusSignalFlags::NONE,
                move |signal| {
                    let sleeping = signal.parameters.get::<(bool,)>().is_some_and(|(s,)| s);
                    if sleeping {
                        return;
                    }
                    if let Some(this) = watch_weak.upgrade() {
                        let accounts = this.accounts.borrow().clone();
                        for ctl in accounts.into_iter().filter(|c| !this.paused(c)) {
                            this.resync(&ctl, true);
                        }
                    }
                },
            );
            if let Some(this) = weak.upgrade() {
                *this.sleep_watch.borrow_mut() = Some(sub);
            }
        });
    }

    fn apply_select_mode(&self, on: bool) {
        self.cancel_read_timer();
        self.list.set_select_mode(on);
        self.search.set_select_mode(on);
        self.select_bar.set_reveal_child(on);
        if on {
            self.select_toggle.set_tooltip_text(Some("Leave selection mode"));
            // The list is where checking happens; on a phone, go back to it.
            if self.inner.is_collapsed() {
                self.inner.set_show_content(false);
            }
            self.checked_changed();
        } else {
            self.select_toggle.set_tooltip_text(Some("Select messages"));
            self.viewer.clear();
            self.lock_search(false);
        }
        self.update_viewer_read();
    }

    /// Whether checking acts on search results rather than the folder.
    fn selecting_results(&self) -> bool {
        self.showing_results()
    }

    /// The checked messages as (folder, UID).
    fn checked(&self) -> Vec<(camel::Folder, String)> {
        if self.selecting_results() {
            return self.search.checked();
        }
        let Some(folder) = self.list.folder() else { return Vec::new() };
        self.list.checked_uids().into_iter().map(|u| (folder.clone(), u)).collect()
    }

    /// Updates the count, the toolbar and the message pane after the
    /// checked messages change.
    fn checked_changed(&self) {
        if !self.list.select_mode() {
            return;
        }
        let checked = self.checked();
        let n = checked.len();
        self.select_count.set_label(&format!("{n} selected"));
        let unread = n == 0 || checked.iter().any(|(f, u)| f.message_flags(u) & SEEN == 0);
        let starred = n > 0 && checked.iter().all(|(f, u)| f.message_flags(u) & FLAGGED != 0);
        self.select_buttons.set_flags(unread, starred);
        let (place, has_junk) = self.place_of(&checked);
        self.select_buttons.set_place(place, has_junk);
        self.viewer.show_count(n);
        self.lock_search(n > 0);
        self.update_targets();
    }

    /// While messages are checked the visible messages must not change:
    /// a search bar without a query closes, and search cannot be changed.
    fn lock_search(&self, locked: bool) {
        if locked && !self.showing_results() && self.search.bar.is_search_mode() {
            self.search.bar.set_search_mode(false);
        }
        self.search_button.set_sensitive(!locked);
        self.search.set_locked(locked);
    }

    /// The kind of folder the messages are in, and whether every account
    /// involved has a Junk folder.
    fn place_of(&self, msgs: &[(camel::Folder, String)]) -> (Place, bool) {
        let mut places = Vec::new();
        let mut has_junk = !msgs.is_empty();
        for (folder, _) in msgs {
            let Some(ctl) = self.account_of(folder) else { continue };
            has_junk &= ctl.special.borrow().as_ref().is_some_and(|s| s.junk.is_some());
            let p = folder_place(&ctl, folder);
            if !places.contains(&p) {
                places.push(p);
            }
        }
        match places.as_slice() {
            [] => (Place::Ordinary, has_junk),
            [p] => (*p, has_junk),
            _ => (Place::Mixed, has_junk),
        }
    }

    fn apply_order(&self) {
        let newest = self
            .window
            .action_state("order")
            .and_then(|v| v.get::<String>())
            .is_none_or(|s| s == "newest");
        let threaded = self.window.action_state("threads").and_then(|v| v.get::<bool>()).unwrap_or(true);
        self.list.set_order(newest, threaded);
        let mut settings = Settings::get();
        if (settings.newest_first, settings.threaded) != (newest, threaded) {
            settings.newest_first = newest;
            settings.threaded = threaded;
            if let Err(e) = settings.save() {
                glib::g_warning!("salutations", "Could not save settings: {}", e);
            }
        }
    }

    fn load_accounts(self: &Rc<Self>) {
        let found = salutations_core::mail_accounts(&self.registry);
        let mut accounts = self.accounts.borrow_mut();
        accounts.retain(|ctl| found.iter().any(|a| a.uid == ctl.account.uid));
        let mut new = Vec::new();
        // The user's order first, then the rest by name (as found).
        let saved = Settings::get().account_order;
        let mut order: Vec<String> = found.iter().map(|a| a.uid.clone()).collect();
        order.sort_by_key(|u| saved.iter().position(|s| s == u).unwrap_or(usize::MAX));
        for a in found {
            if let Some(ctl) = accounts.iter().find(|c| c.account.uid == a.uid) {
                // A rename (in GOA, for instance) arrives as a change of the
                // source the account already reads its name from.
                let name = ctl.account.display_name();
                if ctl.obj.title() != name {
                    ctl.obj.set_title(name.as_str());
                }
                continue;
            }
            let special = SpecialFolders::load(&a.uid);
            let ctl = Rc::new(AccountCtl {
                obj: FolderObject::account(&a.uid, &a.display_name()),
                account: a,
                store: RefCell::new(None),
                abort: RefCell::new(None),
                again: Cell::new(false),
                failures: Cell::new(0),
                retry: RefCell::new(None),
                watching: Cell::new(false),
                sync_started: Cell::new(None),
                unread: Cell::new(None),
                special: RefCell::new(special),
                special_checked: Cell::new(false),
                sign_in_failed: Cell::new(false),
                timer: RefCell::new(None),
                paused: Cell::new(false),
                keepalive: RefCell::new(None),
                synced: RefCell::new(HashSet::new()),
                last_cutoff: Cell::new(None),
                maintaining: Cell::new(false),
                maintain_again: Cell::new(None),
                last_sync: Cell::new(None),
                inbox: RefCell::new(None),
                noted: RefCell::new(Vec::new()),
                combined: RefCell::new(Vec::new()),
                last_uid: Cell::new(None),
                expected: RefCell::new(Vec::new()),
                sending: Cell::new(false),
                sending_uid: RefCell::new(None),
            });
            accounts.push(ctl.clone());
            new.push(ctl);
        }
        accounts.sort_by_key(|c| order.iter().position(|u| *u == c.account.uid));
        let objs: Vec<FolderObject> = accounts.iter().map(|c| c.obj.clone()).collect();
        drop(accounts);
        // Rebuilding the rows would collapse every expanded folder, so it
        // is done only when accounts were added, removed or reordered.
        let store = &self.folders.accounts;
        let unchanged = store.n_items() as usize == objs.len()
            && objs.iter().enumerate().all(|(i, o)| store.item(i as u32).as_ref() == Some(o.upcast_ref()));
        if !unchanged {
            store.splice(0, store.n_items(), &objs);
        }
        self.update_title();
        let this = self.clone();
        glib::spawn_future_local(async move {
            // A paused account syncs when the pause ends, or when asked.
            for ctl in &new {
                ctl.paused.set(this.paused(ctl));
                this.apply_holds(ctl);
            }
            this.show_cached(&new).await;
            for ctl in new {
                if !ctl.paused.get() {
                    this.sync(ctl);
                }
            }
        });
    }

    /// Shows the cached folder trees and opens the first account's INBOX
    /// from Camel's summaries before any connection. The session is offline
    /// meanwhile, since Camel connects a store on any folder call while it is
    /// online. Skipped when a store is already connecting or connected, as
    /// taking the session offline would fail its operations.
    async fn show_cached(self: &Rc<Self>, new: &[Rc<AccountCtl>]) {
        let camel_session = self.session.upcast_ref::<camel::Session>();
        // Stores are created online so they can later connect without
        // Camel's go-online step (see Session::connect_future()).
        let stores: Vec<_> = new.iter().filter_map(|c| self.store_for(c).ok().map(|s| (c.clone(), s))).collect();
        let busy = camel_session
            .list_services()
            .iter()
            .any(|s| s.connection_status() != camel::ServiceConnectionStatus::Disconnected);
        if busy || !camel_session.is_online() {
            return;
        }
        camel_session.set_online(false);
        for (ctl, store) in &stores {
            if let Ok(cached) = self.session.folders_future(store, false).await {
                crate::debug::log(&format!("{}: cached tree, {} top-level folders", ctl.account.uid, cached.len()));
                self.set_folders(ctl, &cached, false);
                let opening = self.opening.borrow_mut().take();
                if let Some(h) = opening {
                    let _ = h.await;
                }
            }
        }
        camel_session.set_online(true);
    }

    fn sync_button_pressed(self: &Rc<Self>, obj: &FolderObject, button: &gtk::Button) {
        let Some(ctl) = self.accounts.borrow().iter().find(|c| &c.obj == obj).cloned() else { return };
        match obj.sync_state() {
            SyncState::Syncing => {
                if let Some(tx) = ctl.abort.borrow_mut().take() {
                    let _ = tx.send(());
                }
            }
            SyncState::Error => {
                let text = gtk::Label::builder()
                    .label(obj.sync_message())
                    .wrap(true)
                    .max_width_chars(36)
                    .xalign(0.0)
                    .build();
                let retry = gtk::Button::builder().label("_Retry").use_underline(true).build();
                retry.add_css_class("suggested-action");
                let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                buttons.set_halign(gtk::Align::End);
                let accounts = ctl.sign_in_failed.get().then(|| {
                    gtk::Button::builder()
                        .label("Open _Online Accounts")
                        .use_underline(true)
                        .action_name("app.online-accounts")
                        .build()
                });
                if let Some(a) = &accounts {
                    buttons.append(a);
                }
                buttons.append(&retry);
                let b = gtk::Box::new(gtk::Orientation::Vertical, 12);
                b.set_margin_top(6);
                b.set_margin_bottom(6);
                b.set_margin_start(6);
                b.set_margin_end(6);
                b.append(&text);
                b.append(&buttons);
                let pop = gtk::Popover::builder().child(&b).build();
                pop.set_parent(button);
                if let Some(a) = accounts {
                    let p = pop.clone();
                    a.connect_clicked(move |_| p.popdown());
                }
                let weak = Rc::downgrade(self);
                let p = pop.clone();
                retry.connect_clicked(move |_| {
                    p.popdown();
                    if let Some(this) = weak.upgrade() {
                        this.sync(ctl.clone());
                    }
                });
                pop.connect_closed(|p| {
                    let p = p.clone();
                    glib::idle_add_local_once(move || p.unparent());
                });
                pop.popup();
            }
            SyncState::Idle | SyncState::Offline => self.sync(ctl),
        }
    }

    fn store_for(&self, ctl: &AccountCtl) -> Result<camel::Store, glib::Error> {
        if let Some(s) = ctl.store.borrow().as_ref() {
            return Ok(s.clone());
        }
        let s = self.session.store(&ctl.account)?;
        *ctl.store.borrow_mut() = Some(s.clone());
        self.apply_holds(ctl);
        Ok(s)
    }

    /// A connection dropped outside a sync (the server closed it, or push
    /// ended) is restored by syncing again shortly.
    fn watch_connection(self: &Rc<Self>, ctl: &Rc<AccountCtl>, store: &camel::Store) {
        if ctl.watching.replace(true) {
            return;
        }
        let refs = std::sync::Arc::new(glib::thread_guard::ThreadGuard::new((Rc::downgrade(self), Rc::downgrade(ctl))));
        store.connect_notify(Some("connection-status"), move |store, _| {
            if store.connection_status() != camel::ServiceConnectionStatus::Disconnected {
                return;
            }
            // Camel notifies from its worker threads too; the references are
            // only touched on the main thread.
            let refs = refs.clone();
            glib::MainContext::default().invoke(move || {
                let (Some(this), Some(ctl)) = (refs.get_ref().0.upgrade(), refs.get_ref().1.upgrade()) else { return };
                // Only a drop after a good sync: failed attempts have a growing
                // retry delay.
                if ctl.obj.sync_state() == SyncState::Idle
                    && ctl.retry.borrow().is_none()
                    && gio::NetworkMonitor::default().is_network_available()
                {
                    this.schedule_retry(&ctl, 5);
                }
            });
        });
    }

    /// Disconnects and syncs again, so a changed connection setting applies.
    fn reconnect(self: &Rc<Self>, ctl: &Rc<AccountCtl>) {
        self.resync(ctl, true);
    }

    /// Ends a running sync and starts another; `drop_connection` first
    /// closes the connection without the polite logout, for sockets that
    /// sleep or a network change may have left dead.
    fn resync(self: &Rc<Self>, ctl: &Rc<AccountCtl>, drop_connection: bool) {
        crate::debug::log(&format!("{}: resync (dropping the connection: {drop_connection})", ctl.account.uid));
        if let Some(tx) = ctl.abort.borrow_mut().take() {
            let _ = tx.send(());
        }
        let Some(store) = ctl.store.borrow().clone() else {
            self.sync(ctl.clone());
            return;
        };
        let (this, ctl) = (self.clone(), ctl.clone());
        glib::spawn_future_local(async move {
            if drop_connection {
                let _ = glib::future_with_timeout(
                    std::time::Duration::from_secs(5),
                    store.disconnect_future(false, glib::ffi::G_PRIORITY_DEFAULT),
                )
                .await;
            }
            // The ended sync gives up its state when it notices the abort,
            // which is after this point.
            if ctl.obj.sync_state() == SyncState::Syncing {
                ctl.again.set(true);
            } else {
                this.sync(ctl);
            }
        });
    }

    /// Tries again after `seconds`, replacing any retry already waiting.
    /// Without a network, the network's return syncs instead.
    fn schedule_retry(self: &Rc<Self>, ctl: &Rc<AccountCtl>, seconds: u32) {
        if let Some(id) = ctl.retry.borrow_mut().take() {
            id.remove();
        }
        if self.paused(ctl) || !gio::NetworkMonitor::default().is_network_available() {
            return;
        }
        let (weak, c) = (Rc::downgrade(self), Rc::downgrade(ctl));
        let id = glib::timeout_add_seconds_local_once(seconds, move || {
            let (Some(this), Some(ctl)) = (weak.upgrade(), c.upgrade()) else { return };
            ctl.retry.borrow_mut().take();
            this.sync(ctl);
        });
        *ctl.retry.borrow_mut() = Some(id);
    }

    /// Retries a failed sync, less often each time but never less often
    /// than the account is checked. An account never checked stops
    /// retrying after the longest step.
    fn retry_failed(self: &Rc<Self>, ctl: &Rc<AccountCtl>) {
        let n = ctl.failures.get() as usize;
        ctl.failures.set(ctl.failures.get().saturating_add(1));
        let interval = match self.refresh_now(ctl) {
            offline::Refresh::Minutes(m) => m.saturating_mul(60),
            offline::Refresh::Never if n >= RETRY_SECONDS.len() => return,
            offline::Refresh::Never => u32::MAX,
        };
        let delay = RETRY_SECONDS[n.min(RETRY_SECONDS.len() - 1)].min(interval);
        self.schedule_retry(ctl, delay);
    }

    /// Connects, refreshes the folder tree and the open folder.
    fn sync(self: &Rc<Self>, ctl: Rc<AccountCtl>) {
        if ctl.obj.sync_state() == SyncState::Syncing {
            ctl.again.set(true);
            return;
        }
        if let Some(id) = ctl.retry.borrow_mut().take() {
            id.remove();
        }
        let (tx, rx) = futures_channel::oneshot::channel::<()>();
        *ctl.abort.borrow_mut() = Some(tx);
        ctl.obj.set_sync_state(SyncState::Syncing);
        ctl.sync_started.set(Some(std::time::Instant::now()));
        let this = self.clone();
        glib::spawn_future_local(async move {
            // Dropping the sync's future when aborted cancels its Camel calls.
            let result = match race(this.sync_inner(&ctl), rx).await {
                Either::Left(r) => Some(r),
                Either::Right(_) => None,
            };
            ctl.abort.borrow_mut().take();
            match result {
                None => ctl.obj.set_sync_state(SyncState::Idle),
                Some(Ok(())) => {
                    ctl.failures.set(0);
                    ctl.obj.set_sync_state(SyncState::Idle);
                    this.retry_waiting(&ctl);
                    this.update_badge();
                }
                Some(Err(e)) => {
                    crate::debug::log(&format!("{}: sync failed: {}", ctl.account.uid, e.message()));
                    let auth = e.matches(camel::ServiceError::CantAuthenticate);
                    ctl.sign_in_failed.set(auth);
                    ctl.obj.set_sync_message(e.message());
                    ctl.obj.set_sync_state(if is_offline(&e) { SyncState::Offline } else { SyncState::Error });
                    // A rejected sign-in waits for the user; anything else
                    // is tried again, less often each time.
                    if !auth {
                        this.retry_failed(&ctl);
                    }
                }
            }
            if ctl.again.replace(false) {
                this.sync(ctl);
            }
        });
    }

    async fn sync_inner(self: &Rc<Self>, ctl: &Rc<AccountCtl>) -> Result<(), glib::Error> {
        let store = self.store_for(ctl)?;
        crate::debug::log(&format!("{}: sync start", ctl.account.uid));
        self.watch_connection(ctl, &store);
        within(STEP_LIMIT, self.session.connect_future(&store)).await?;
        crate::debug::log(&format!("{}: connected", ctl.account.uid));
        let fresh = within(STEP_LIMIT, self.session.folders_future(&store, true)).await?;
        crate::debug::log(&format!("{}: folder list refreshed", ctl.account.uid));
        self.set_folders(ctl, &fresh, false);
        let open = self
            .open
            .borrow()
            .as_ref()
            .filter(|o| Rc::ptr_eq(&o.account, ctl))
            .map(|o| o.folder.clone());
        let refreshed = open.as_ref().and_then(|f| f.full_name()).map(|n| n.to_string());
        if let Some(folder) = open {
            folder.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT).await?;
            crate::debug::log(&format!("{}: open folder refreshed", ctl.account.uid));
        }
        if !ctl.special_checked.get()
            && let Ok(special) = self.session.special_folders(&store).await
        {
            let special = special.merged(ctl.special.borrow().as_ref());
            crate::debug::log(&format!("{}: special folders checked", ctl.account.uid));
            ctl.special_checked.set(true);
            if ctl.special.borrow().as_ref() != Some(&special) {
                if let Err(e) = special.save(&ctl.account.uid) {
                    glib::g_warning!("salutations", "Cannot save special folders: {}", e);
                }
                *ctl.special.borrow_mut() = Some(special);
                self.set_folders(ctl, &fresh, true);
            }
        }
        if ctl.timer.borrow().is_none() {
            self.first_sync_setup(ctl, &store).await;
            self.schedule_refresh(ctl);
        }
        self.watch_inbox(ctl, &store).await;
        ctl.last_sync.set(Some(std::time::Instant::now()));
        // The first run of a session checks every message against the window.
        let full = ctl.last_cutoff.get().is_none();
        self.maintain(ctl, full, refreshed);
        Ok(())
    }

    /// Updates counts in place when the tree is unchanged, so expanded rows
    /// stay expanded; rebuilds it otherwise or when `rebuild` is set.
    fn set_folders(self: &Rc<Self>, ctl: &Rc<AccountCtl>, nodes: &[camel::FolderNode], rebuild: bool) {
        let special = ctl.special.borrow().clone().unwrap_or_default();
        let nodes = &sort_folders(nodes, &special)[..];
        fn names(nodes: &[camel::FolderNode], out: &mut Vec<String>) {
            for n in nodes {
                out.push(n.full_name.clone());
                names(&n.children, out);
            }
        }
        fn existing(store: &gio::ListStore, out: &mut Vec<String>) {
            for i in 0..store.n_items() {
                if let Some(o) = store.item(i).and_downcast::<FolderObject>() {
                    out.push(o.full_name());
                    existing(o.children(), out);
                }
            }
        }
        fn counts(obj: &FolderObject, nodes: &[camel::FolderNode], open: Option<&str>) {
            for n in nodes {
                if let Some(o) = obj.find(&n.full_name)
                    && open != Some(n.full_name.as_str())
                {
                    o.set_unread(n.unread);
                }
                counts(obj, &n.children, open);
            }
        }
        let (mut a, mut b) = (Vec::new(), Vec::new());
        names(nodes, &mut a);
        existing(ctl.obj.children(), &mut b);
        let open = self
            .open
            .borrow()
            .as_ref()
            .filter(|o| Rc::ptr_eq(&o.account, ctl))
            .map(|o| o.folder.full_name().map(|s| s.to_string()).unwrap_or_default());
        if a == b && !rebuild {
            counts(&ctl.obj, nodes, open.as_deref());
            return;
        }
        let objs: Vec<FolderObject> = nodes.iter().map(|n| FolderObject::folder(&ctl.account.uid, n)).collect();
        let children = ctl.obj.children();
        children.splice(0, children.n_items(), &objs);
        for (name, icon) in special_icons(&special) {
            if let Some(o) = ctl.obj.find(name) {
                o.set_icon_name(icon);
            }
        }
        // A rebuilt tree drops the selection; restore it, or open INBOX
        // of the first account on first load.
        let reselect = open
            .as_deref()
            .and_then(|name| ctl.obj.find(name))
            .or_else(|| {
                let first = self.accounts.borrow().first().is_some_and(|c| Rc::ptr_eq(c, ctl));
                (first && self.open.borrow().is_none())
                    .then(|| ctl.obj.find("INBOX"))
                    .flatten()
            });
        if let Some(obj) = reselect {
            self.folders.select(&obj);
        }
    }

    fn open_folder(self: &Rc<Self>, obj: &FolderObject) {
        // A rebuilt tree has new objects for the same folders; the open
        // folder stays open and only its row object is swapped.
        if let Some(o) = self.open.borrow_mut().as_mut()
            && o.obj.account_uid() == obj.account_uid()
            && o.obj.full_name() == obj.full_name()
        {
            if let Some(s) = o.folder.folder_summary() {
                obj.set_unread(s.unread_count() as i32 - self.hidden_unread(&o.folder));
            }
            o.obj = obj.clone();
            return;
        }
        let Some(ctl) = self.accounts.borrow().iter().find(|c| c.account.uid == obj.account_uid()).cloned() else {
            return;
        };
        if self.list.select_mode() {
            self.window.change_action_state("select-mode", &false.to_variant());
        }
        self.title.set_title(&obj.title());
        self.title.set_subtitle(&ctl.account.display_name());
        let junk = ctl.special.borrow().as_ref().and_then(|s| s.junk.clone()).is_some_and(|j| j == obj.full_name());
        crate::people::set_junk_shown(junk);
        let obj = obj.clone();
        let this = self.clone();
        let handle = glib::spawn_future_local(async move {
            let Ok(store) = this.store_for(&ctl) else { return };
            let folder = match this.session.folder_future(&store, &obj.full_name()).await {
                Ok(f) => f,
                Err(e) => {
                    glib::g_warning!("salutations", "Cannot open {}: {}", obj.full_name(), e);
                    return;
                }
            };
            // The user may have picked another folder meanwhile.
            if this.folders.selected().as_ref() != Some(&obj) {
                return;
            }
            if let Some(old) = this.open.borrow_mut().take() {
                old.folder.disconnect(old.handler);
            }
            // Camel emits "changed" from an idle on the session's main
            // context, which is this thread's.
            let weak = glib::thread_guard::ThreadGuard::new(Rc::downgrade(&this));
            let handler = folder.connect_changed(move |folder, changes| {
                if let Some(this) = weak.get_ref().upgrade() {
                    this.folder_changed(folder, changes);
                }
            });
            let archive = ctl.special.borrow().as_ref().and_then(|s| s.archive.clone());
            this.list.set_in_archive(archive.as_deref() == folder.full_name().as_deref());
            this.list.set_folder(Some(&folder), ctl.account.address.clone());
            // A new folder starts unfiltered.
            if let Some(a) = this.window.lookup_action("filter").and_downcast::<gio::SimpleAction>() {
                a.set_state(&"all".to_variant());
            }
            this.update_filter();
            crate::debug::log(&format!("{}: {} listed", ctl.account.uid, obj.full_name()));
            if let Some(s) = folder.folder_summary() {
                obj.set_unread(s.unread_count() as i32 - this.hidden_unread(&folder));
            }
            *this.open.borrow_mut() = Some(OpenFolder { account: ctl, obj, folder: folder.clone(), handler });
            this.update_pause_banner();
            this.update_empty_banner();
            this.update_outbox_notes();
            // A search of the folder follows it to the new one.
            if this.showing_results() && this.search.scope() == search::Scope::Folder {
                this.run_search(this.search.query(), search::Scope::Folder);
            }
            let _ = folder.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT).await;
        });
        *self.opening.borrow_mut() = Some(handle);
    }

    fn folder_changed(&self, folder: &camel::Folder, changes: &camel::FolderChanges) {
        if self.open.borrow().as_ref().is_some_and(|o| &o.folder == folder) {
            self.update_empty_banner();
        }
        let open = self.open.borrow();
        let Some(o) = open.as_ref().filter(|o| &o.folder == folder) else { return };
        if let Some(s) = folder.folder_summary() {
            o.obj.set_unread(s.unread_count() as i32 - self.hidden_unread(folder));
        }
        if !changes.added.is_empty() || !changes.removed.is_empty() {
            let keep = self.list.selected_uid();
            drop(open);
            self.list.reload(keep);
            if let Some((_, uid)) = self.viewer.current()
                && changes.removed.contains(&uid)
            {
                self.viewer.clear();
            }
        } else if changes.changed.iter().any(|u| folder.message_flags(u) & DELETED != 0) {
            // Deleted messages stay in the summary until expunged.
            drop(open);
            self.list.reload(self.list.selected_uid());
        } else {
            drop(open);
            self.list.refresh_flags(&changes.changed);
        }
        self.checked_changed();
        self.update_viewer_read();
    }

    fn cancel_read_timer(&self) {
        if let Some(id) = self.read_timer.borrow_mut().take() {
            id.remove();
        }
    }

    /// The only automatic path to marking mail read.
    fn start_read_timer(self: &Rc<Self>, folder: &camel::Folder, uid: &str) {
        self.cancel_read_timer();
        let settings = Settings::get();
        if !settings.mark_read || settings.read_on_response || folder.message_flags(uid) & SEEN != 0 {
            return;
        }
        if settings.mark_read_delay == 0 {
            folder.set_message_flags(uid, SEEN, SEEN);
            return;
        }
        let weak = Rc::downgrade(self);
        let (folder, uid) = (folder.clone(), uid.to_string());
        let id = glib::timeout_add_seconds_local_once(settings.mark_read_delay, move || {
            let Some(this) = weak.upgrade() else { return };
            this.read_timer.borrow_mut().take();
            // Still the same message; the timer is cancelled on every change.
            if this.viewer.current().is_some_and(|(f, u)| f == folder && u == uid) {
                folder.set_message_flags(&uid, SEEN, SEEN);
                this.update_viewer_read();
            }
        });
        *self.read_timer.borrow_mut() = Some(id);
    }

    fn toggle_read(&self) {
        if self.list.select_mode() {
            // Everything read if any checked message is unread, otherwise
            // everything unread.
            let checked = self.checked();
            let read = checked.iter().any(|(f, u)| f.message_flags(u) & SEEN == 0);
            for (folder, uid) in &checked {
                folder.set_message_flags(uid, SEEN, if read { SEEN } else { 0 });
            }
            if self.showing_results() {
                self.search.refresh_flags();
            }
            self.checked_changed();
            return;
        }
        let Some((folder, uid)) = self.viewer.target() else { return };
        let seen = folder.message_flags(&uid) & SEEN != 0;
        self.cancel_read_timer();
        folder.set_message_flags(&uid, SEEN, if seen { 0 } else { SEEN });
        self.update_viewer_read();
    }

    /// The message view's buttons follow the shown message.
    fn update_viewer_read(&self) {
        let current = self.viewer.target();
        let seen = current.as_ref().map(|(folder, uid)| folder.message_flags(uid) & SEEN != 0);
        let starred = current.as_ref().map(|(folder, uid)| folder.message_flags(uid) & FLAGGED != 0);
        self.viewer_buttons.set_flags(seen != Some(true), starred == Some(true));
        let msgs: Vec<(camel::Folder, String)> = current.into_iter().collect();
        let (place, has_junk) = self.place_of(&msgs);
        self.viewer_buttons.set_place(place, has_junk);
        self.viewer_buttons.set_visible(!msgs.is_empty() && !self.list.select_mode());
        if self.showing_results() {
            self.search.refresh_flags();
        }
        self.update_targets();
        self.update_reply_bar();
    }

    /// Enables the message actions that fit every message acted on, and
    /// says in the selection toolbar's tooltips why the others do not.
    fn update_targets(&self) {
        let targets = self.current_targets();
        let has = !targets.is_empty();
        let place = |t: &Target| folder_place(&t.account, &t.folder);
        // Whether every target fits; the reason when one does not.
        let check = |f: &dyn Fn(&Target) -> bool, why: &'static str| -> (bool, Option<&'static str>) {
            match (has, targets.iter().all(f)) {
                (false, _) => (false, None),
                (true, true) => (true, None),
                (true, false) => (false, Some(why)),
            }
        };
        let has_junk = |t: &Target| t.account.special.borrow().as_ref().is_some_and(|s| s.junk.is_some());
        let rules = [
            ("delete", check(&|t| place(t) != Place::Trash, "not for messages in Trash")),
            (
                "trash",
                check(
                    &|t| place(t) != Place::Trash && t.account.special.borrow().as_ref().is_some_and(|s| s.trash.is_some()),
                    "not for messages in Trash",
                ),
            ),
            (
                "delete-permanently",
                check(&|t| matches!(place(t), Place::Trash | Place::Junk), "only for messages in Trash or Junk"),
            ),
            ("restore", check(&|t| place(t) == Place::Trash, "only for messages in Trash")),
            ("not-junk", check(&|t| place(t) == Place::Junk, "only for messages in Junk")),
            ("junk", check(&|t| place(t) != Place::Junk && has_junk(t), "not for messages in Junk")),
            (
                "archive",
                check(&|t| place(t) != Place::Trash && !is_archive(&t.account, &t.folder), "not for messages in Trash or Archive"),
            ),
            ("move", (has, None)),
            ("toggle-read", (has, None)),
            ("toggle-star", (has, None)),
        ];
        // Delete (the key) still deletes permanently in Trash.
        for (name, (on, why)) in rules {
            let on = if name == "delete" { has } else { on };
            if let Some(a) = self.window.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_enabled(on);
            }
            self.select_buttons.set_reason(name, why);
        }
    }

    /// The checked messages in selection mode, one target per folder;
    /// otherwise the shown message.
    fn current_targets(&self) -> Vec<Target> {
        if self.list.select_mode() {
            let mut out: Vec<Target> = Vec::new();
            for (folder, uid) in self.checked() {
                match out.iter_mut().find(|t| t.folder == folder) {
                    Some(t) => t.uids.push(uid),
                    None => {
                        let Some(account) = self.account_of(&folder) else { continue };
                        out.push(Target { account, folder, uids: vec![uid], thread: false });
                    }
                }
            }
            return out;
        }
        // A search result may be in another folder or account.
        let Some((folder, uid)) = self.viewer.target() else { return Vec::new() };
        let Some(account) = self.account_of(&folder) else { return Vec::new() };
        vec![Target { account, folder, uids: vec![uid], thread: false }]
    }

    /// Starts watching the account's INBOX for new mail, once per session,
    /// and notifies about mail that arrived since the last run.
    async fn watch_inbox(self: &Rc<Self>, ctl: &Rc<AccountCtl>, store: &camel::Store) {
        if ctl.inbox.borrow().is_some() {
            return;
        }
        let Ok(folder) = self.session.folder_future(store, "INBOX").await else { return };
        if ctl.inbox.borrow().is_some() {
            return;
        }
        let uids: Vec<String> = folder.uids().iter().map(|u| u.to_string()).collect();
        if ctl.last_uid.get().is_none() {
            // The first run ever starts from what is there, without
            // notifying about all of it.
            let saved = load_last_uid(&ctl.account.uid);
            let start = saved.unwrap_or_else(|| uids.iter().filter_map(|u| u.parse().ok()).max().unwrap_or(0));
            ctl.last_uid.set(Some(start));
            if saved.is_none() {
                save_last_uid(&ctl.account.uid, start);
            }
        }
        let weak = glib::thread_guard::ThreadGuard::new(Rc::downgrade(self));
        let c = Rc::downgrade(ctl);
        let c = glib::thread_guard::ThreadGuard::new(c);
        let handler = folder.connect_changed(move |folder, changes| {
            if let (Some(this), Some(ctl)) = (weak.get_ref().upgrade(), c.get_ref().upgrade()) {
                this.inbox_changed(&ctl, folder, changes);
            }
        });
        *ctl.inbox.borrow_mut() = Some((folder.clone(), handler));
        self.notify_new(ctl, &folder, &uids);
        self.update_badge();
    }

    fn inbox_changed(&self, ctl: &Rc<AccountCtl>, folder: &camel::Folder, changes: &camel::FolderChanges) {
        if !changes.added.is_empty() {
            self.download_arrivals(ctl, folder);
        }
        // Read, moved or deleted anywhere: its notification goes.
        let gone = |uid: &String| {
            changes.removed.contains(uid) || folder.message_info(uid).is_none() || folder.message_flags(uid) & (SEEN | DELETED) != 0
        };
        let withdrawn: Vec<String> = ctl.noted.borrow().iter().filter(|u| gone(u)).cloned().collect();
        for uid in &withdrawn {
            self.notifier.withdraw(&notify::Subject::Message { account: ctl.account.uid.clone(), uid: uid.clone() });
        }
        ctl.noted.borrow_mut().retain(|u| !withdrawn.contains(u));
        let before = ctl.combined.borrow().len();
        ctl.combined.borrow_mut().retain(|u| !gone(u));
        let after = ctl.combined.borrow().len();
        if after != before {
            if after == 0 {
                self.notifier.withdraw(&notify::Subject::Combined { account: ctl.account.uid.clone() });
            } else {
                self.show_combined(ctl, folder, false);
            }
        }
        self.notify_new(ctl, folder, &changes.added);
        self.update_badge();
    }

    /// Notifies about unread messages among `uids` that are newer than any
    /// considered before.
    fn notify_new(&self, ctl: &Rc<AccountCtl>, folder: &camel::Folder, uids: &[String]) {
        let last = ctl.last_uid.get().unwrap_or(0);
        let mut fresh: Vec<(u64, String)> =
            uids.iter().filter_map(|u| u.parse::<u64>().ok().map(|n| (n, u.clone()))).filter(|(n, _)| *n > last).collect();
        if fresh.is_empty() {
            return;
        }
        fresh.sort();
        let top = fresh.last().map_or(last, |(n, _)| *n);
        ctl.last_uid.set(Some(top));
        save_last_uid(&ctl.account.uid, top);
        const JUNK: u32 = camel::MessageFlags::JUNK.bits();
        let mut expected = ctl.expected.borrow_mut();
        let fresh: Vec<String> = fresh
            .into_iter()
            .map(|(_, u)| u)
            .filter(|u| folder.message_flags(u) & (SEEN | DELETED | JUNK) == 0)
            .filter(|u| {
                // A message Salutations moved here is not new mail.
                let Some(k) = message_key(folder, u) else { return false };
                match expected.iter().position(|e| *e == k) {
                    Some(i) => {
                        expected.swap_remove(i);
                        false
                    }
                    None => true,
                }
            })
            .collect();
        drop(expected);
        if fresh.is_empty() {
            return;
        }
        // Nothing while the user is looking at this INBOX.
        let watching = self.window.is_active() && self.open.borrow().as_ref().is_some_and(|o| &o.folder == folder);
        if watching {
            return;
        }
        let sound = !Settings::get().silent_accounts.contains(&ctl.account.uid);
        let total = ctl.noted.borrow().len() + fresh.len();
        let limit = Settings::get().notification_limit;
        if !ctl.combined.borrow().is_empty() || limit.is_some_and(|n| total > n as usize) {
            let noted: Vec<String> = ctl.noted.borrow_mut().drain(..).collect();
            for uid in &noted {
                self.notifier.withdraw(&notify::Subject::Message { account: ctl.account.uid.clone(), uid: uid.clone() });
            }
            let mut combined = ctl.combined.borrow_mut();
            combined.extend(noted);
            combined.extend(fresh);
            drop(combined);
            self.show_combined(ctl, folder, sound);
            return;
        }
        let junk = ctl.special.borrow().as_ref().is_some_and(|s| s.junk.is_some());
        for uid in fresh {
            let Some(info) = folder.message_info(&uid) else { continue };
            let from = crate::message_list::parse_from(&info.from().unwrap_or_default());
            let subject = info.subject().map(|s| s.to_string()).unwrap_or_default();
            let preview = info.preview().map(|p| salutations_core::render::clean_preview(&p)).unwrap_or_default();
            let verified = info
                .user_header("Authentication-Results")
                .and_then(|h| salutations_core::verify::parse(&h).verified_domain(&from.email));
            ctl.noted.borrow_mut().push(uid.clone());
            // The contact's photo can take a moment to fetch.
            let (notifier, account) = (self.notifier.clone(), ctl.account.uid.clone());
            glib::spawn_future_local(async move {
                let (contact_name, photo) = crate::people::lookup(&from.email, verified.as_deref()).await;
                notifier.message(notify::MessageNote {
                    account: &account,
                    uid: &uid,
                    sender: from.display(),
                    contact_name,
                    photo,
                    subject: &subject,
                    preview: &preview,
                    sound,
                    junk,
                });
            });
        }
    }

    fn show_combined(&self, ctl: &Rc<AccountCtl>, folder: &camel::Folder, sound: bool) {
        let combined = ctl.combined.borrow().clone();
        let senders: Vec<String> = combined
            .iter()
            .rev()
            .filter_map(|u| folder.message_info(u))
            .map(|i| crate::message_list::parse_from(&i.from().unwrap_or_default()).display().to_string())
            .collect();
        self.notifier.combined(&ctl.account.uid, &ctl.account.display_name(), combined.len(), &senders, sound);
    }

    /// The unread count of every account's INBOX. An account whose INBOX
    /// is unavailable (not yet opened, or reconnecting) counts with its
    /// last known number rather than dropping out of the total.
    fn update_badge(&self) {
        let mut total = 0;
        for c in self.accounts.borrow().iter() {
            if let Some(n) = c.inbox.borrow().as_ref().and_then(|(f, _)| f.folder_summary()).map(|s| s.unread_count() as i64) {
                c.unread.set(Some(n));
            }
            total += c.unread.get().unwrap_or(0);
        }
        self.notifier.set_badge(total);
    }

    fn notification_action(self: &Rc<Self>, action: notify::Action) {
        use notify::Action as A;
        let account = match &action {
            A::Open { account, .. }
            | A::Delete { account, .. }
            | A::Junk { account, .. }
            | A::Reply { account, .. }
            | A::OpenInbox { account } => account.clone(),
        };
        let Some(ctl) = self.accounts.borrow().iter().find(|c| c.account.uid == account).cloned() else { return };
        let Some(inbox) = ctl.inbox.borrow().as_ref().map(|(f, _)| f.clone()) else { return };
        let reply = matches!(action, A::Reply { .. });
        match action {
            A::Open { uid, .. } | A::Reply { uid, .. } => {
                self.window.present();
                let this = self.clone();
                glib::spawn_future_local(async move {
                    this.show_inbox(&ctl).await;
                    // The reply starts once the viewer has the message.
                    if reply {
                        *this.reply_when_shown.borrow_mut() = Some((inbox.clone(), uid.clone()));
                    }
                    if this.list.folder().as_ref() == Some(&inbox) {
                        this.list.open_uid(&uid);
                        if this.inner.is_collapsed() {
                            this.inner.set_show_content(true);
                        }
                    }
                });
            }
            A::OpenInbox { .. } => {
                self.window.present();
                let this = self.clone();
                glib::spawn_future_local(async move { this.show_inbox(&ctl).await });
            }
            A::Delete { uid, .. } => {
                // Deleting for good asks first, which needs the window.
                if ctl.special.borrow().as_ref().and_then(|s| s.trash.as_ref()).is_none() {
                    self.window.present();
                }
                self.request(Op::Delete, Target { account: ctl, folder: inbox, uids: vec![uid], thread: false });
            }
            A::Junk { uid, .. } => {
                self.request(Op::Junk, Target { account: ctl, folder: inbox, uids: vec![uid], thread: false });
            }
        }
    }

    /// Selects the account's INBOX in the folder pane and waits until its
    /// list is shown.
    async fn show_inbox(self: &Rc<Self>, ctl: &Rc<AccountCtl>) {
        if self.search.bar.is_search_mode() {
            self.search.bar.set_search_mode(false);
        }
        if self.list.select_mode() {
            self.window.change_action_state("select-mode", &false.to_variant());
        }
        let Some(obj) = ctl.obj.find("INBOX") else { return };
        self.folders.select(&obj);
        let opening = self.opening.borrow_mut().take();
        if let Some(h) = opening {
            let _ = h.await;
        }
    }

    /// The floating New Message button shows while the folder pane, which
    /// has its own, is hidden; not in selection mode.
    fn update_compose_float(&self) {
        let selecting = self.window.action_state("select-mode").and_then(|s| s.get::<bool>()).unwrap_or(false);
        let shown = !self.outer.shows_sidebar() && !selecting;
        self.compose_float.set_visible(shown);
        // Room under the last message, so the button never covers it.
        if let Some(overlay) = self.compose_float.parent() {
            if shown {
                overlay.add_css_class("with-float");
            } else {
                overlay.remove_css_class("with-float");
            }
        }
    }

    /// Whether `folder` is its account's Drafts folder.
    fn is_drafts(&self, folder: &camel::Folder) -> bool {
        let Some(account) = self.account_of(folder) else { return false };
        let drafts = account.special.borrow().as_ref().and_then(|s| s.drafts.clone());
        drafts.is_some() && folder.full_name().map(|n| n.to_string()) == drafts
    }

    fn account_of(&self, folder: &camel::Folder) -> Option<Rc<AccountCtl>> {
        let store = folder.parent_store()?;
        self.accounts.borrow().iter().find(|c| c.store.borrow().as_ref() == Some(&store)).cloned()
    }

    /// The filter button's look and the empty-list notice follow the filter.
    fn update_filter(&self) {
        use crate::message_list::Filter;
        let filter = self.list.filter();
        let (tip, empty) = match filter {
            Filter::All => ("Show", ""),
            Filter::Unread => ("Showing unread messages", "No unread messages"),
            Filter::Starred => ("Showing starred messages", "No starred messages"),
            Filter::Attachments => ("Showing messages with attachments", "No messages with attachments"),
        };
        self.filter_button.set_tooltip_text(Some(tip));
        if filter == Filter::All {
            self.filter_button.remove_css_class("filter-on");
        } else {
            self.filter_button.add_css_class("filter-on");
        }
        self.filter_empty.set_title(empty);
        self.update_filter_empty();
    }

    fn update_filter_empty(&self) {
        let filtered = self.list.filter() != crate::message_list::Filter::All;
        self.filter_empty.set_visible(filtered && self.list.is_empty());
    }

    /// The folder's name over the list, or "Search" while a search spans
    /// more than the open folder.
    fn update_title(&self) {
        // The filter applies to the folder's list, not to search results.
        self.filter_button.set_sensitive(!self.showing_results());
        let open = self.open.borrow();
        let Some(o) = open.as_ref() else { return };
        let (title, subtitle) = match (self.showing_results(), self.search.scope()) {
            (true, search::Scope::Account) => ("Search".to_string(), o.account.account.display_name()),
            (true, search::Scope::All) => ("Search".to_string(), "All accounts".to_string()),
            _ => (o.obj.title(), o.account.account.display_name()),
        };
        self.title.set_title(&title);
        self.title.set_subtitle(&subtitle);
    }

    /// A pull on the list: the search runs again, or the open folder is
    /// refreshed. An account that is not connected, or whose refresh
    /// fails, syncs in full instead, which also restores its connection.
    fn pull_refreshed(self: &Rc<Self>) {
        let this = self.clone();
        if self.showing_results() {
            self.run_search(self.search.query(), self.search.scope());
            glib::spawn_future_local(async move {
                while this.searching.get() > 0 {
                    glib::timeout_future(std::time::Duration::from_millis(250)).await;
                }
                this.pull.finish();
            });
            return;
        }
        let open = self.open.borrow().as_ref().map(|o| (o.account.clone(), o.folder.clone()));
        let Some((ctl, folder)) = open else {
            self.pull.finish();
            return;
        };
        glib::spawn_future_local(async move {
            let connected = ctl.store.borrow().as_ref().is_some_and(|s| s.connection_status() == camel::ServiceConnectionStatus::Connected)
                && !matches!(ctl.obj.sync_state(), SyncState::Offline | SyncState::Error);
            let refreshed = connected && match within(STEP_LIMIT, folder.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT)).await {
                Ok(()) => true,
                Err(e) => {
                    crate::debug::log(&format!("{}: pull refresh failed: {}", ctl.account.uid, e.message()));
                    false
                }
            };
            if !refreshed {
                this.resync(&ctl, true);
                // The sync starts after the connection closes.
                glib::timeout_future(std::time::Duration::from_millis(500)).await;
                while ctl.obj.sync_state() == SyncState::Syncing {
                    glib::timeout_future(std::time::Duration::from_millis(250)).await;
                }
            }
            this.pull.finish();
        });
    }

    /// Shows the list instead of an empty message pane when the panes are
    /// one at a time. Checked after the current event, since a message is
    /// often cleared just before the next one loads.
    fn leave_empty_viewer(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(this) = weak.upgrade() else { return };
            let empty = this.content_stack.visible_child_name().as_deref() == Some("viewer")
                && this.viewer.widget.visible_child_name().as_deref() == Some("empty");
            if empty && this.inner.is_collapsed() && this.inner.shows_content() && !this.list.select_mode() {
                crate::debug::log("empty message pane: back to the list");
                this.inner.set_show_content(false);
            }
        });
    }

    /// "You replied to this message on <date>" and "You forwarded this
    /// message on <date>" under the shown message's headers, from its
    /// Answered flag and `$Forwarded` keyword. The dates are those of the
    /// newest matching message in the account's Sent ("last" when there
    /// are several): replies name it in In-Reply-To, forwards in
    /// X-Forwarded-Message-Id. Without one, the line has no date.
    pub(super) fn show_responses(self: &Rc<Self>, folder: &camel::Folder, uid: &str) {
        const ANSWERED: u32 = camel::MessageFlags::ANSWERED.bits();
        let answered = folder.message_flags(uid) & ANSWERED != 0;
        let forwarded = folder
            .message_info(uid)
            .is_some_and(|i| camel::message_info_user_flag(&i, crate::message_list::FORWARDED));
        let kinds: Vec<(&'static str, &'static str)> = [(answered, ("replied to", "In-Reply-To")), (forwarded, ("forwarded", salutations_core::compose::FORWARDED_HEADER))]
            .into_iter()
            .filter_map(|(on, k)| on.then_some(k))
            .collect();
        let undated: Vec<String> = kinds.iter().map(|(verb, _)| format!("You {verb} this message")).collect();
        self.viewer.set_responses(&undated);
        if kinds.is_empty() {
            return;
        }
        let id = self.viewer.message().and_then(|m| m.message_id()).map(|i| i.trim_matches(['<', '>', ' ']).to_string());
        let ctl = self.account_of(folder);
        let sent = ctl.as_ref().and_then(|c| c.special.borrow().as_ref().and_then(|s| s.sent.clone()));
        let store = ctl.as_ref().and_then(|c| c.store.borrow().clone());
        let (Some(id), Some(sent), Some(store)) = (id.filter(|i| !i.is_empty()), sent, store) else { return };
        let (this, folder, uid) = (self.clone(), folder.clone(), uid.to_string());
        glib::spawn_future_local(async move {
            let Ok(sent) = this.session.folder_future(&store, &sent).await else { return };
            // Camel searches only the messages its summary lists; replies
            // sent from other devices arrive in it with a refresh.
            let _ = within(STEP_LIMIT, sent.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT)).await;
            let mut lines = Vec::new();
            for (verb, header) in kinds {
                let (f, expr) = (sent.clone(), format!("(match-all (header-contains \"{header}\" \"{id}\"))"));
                let uids = gio::spawn_blocking(move || f.search_sync(&expr, None::<&gio::Cancellable>).unwrap_or_default())
                    .await
                    .unwrap_or_default();
                let dates: Vec<i64> = uids.iter().filter_map(|u| sent.message_info(u)).map(|i| i.date_sent()).filter(|d| *d > 0).collect();
                lines.push(match dates.iter().max() {
                    Some(d) => format!(
                        "You {}{verb} this message on {}",
                        if dates.len() > 1 { "last " } else { "" },
                        compose::date_text(Some(*d))
                    ),
                    None => format!("You {verb} this message"),
                });
            }
            // The message may have changed meanwhile.
            if this.viewer.current().is_some_and(|(f, u)| f == folder && u == uid) {
                this.viewer.set_responses(&lines);
            }
        });
    }

    fn showing_results(&self) -> bool {
        self.list_stack.visible_child_name().as_deref() == Some("results")
    }

    fn cancel_search(&self) {
        if let Some(c) = self.search_cancel.borrow_mut().take() {
            c.cancel();
        }
        self.set_searching(0);
    }

    fn set_searching(&self, n: usize) {
        self.searching.set(n);
        self.search_icons.set_visible_child_name(if n > 0 { "spinner" } else { "icon" });
    }

    /// Searches From, To, Cc, Subject and body. Matches in the summaries
    /// show at once; then each folder is refreshed and searched on the
    /// server, one folder at a time per account, and its matches added.
    fn run_search(self: &Rc<Self>, query: String, scope: search::Scope) {
        self.cancel_search();
        self.search.clear();
        let generation = self.search_gen.get() + 1;
        self.search_gen.set(generation);
        if query.is_empty() {
            self.list_stack.set_visible_child_name("list");
            self.update_title();
            return;
        }
        self.list_stack.set_visible_child_name("results");
        self.update_title();
        let Some((open_account, open_folder)) =
            self.open.borrow().as_ref().map(|o| (o.account.clone(), o.folder.full_name().map(|n| n.to_string()).unwrap_or_default()))
        else {
            return;
        };
        let accounts: Vec<Rc<AccountCtl>> = match scope {
            search::Scope::All => self.accounts.borrow().clone(),
            _ => vec![open_account.clone()],
        };
        let several = accounts.len() > 1;
        let targets: Vec<(Rc<AccountCtl>, Vec<(String, Option<String>)>)> = accounts
            .into_iter()
            .map(|ctl| {
                let folders = match scope {
                    search::Scope::Folder => vec![(open_folder.clone(), None)],
                    _ => folder_choices(&ctl, &[])
                        .into_iter()
                        .map(|c| {
                            let loc = if several { format!("{} · {}", ctl.account.display_name(), c.path) } else { c.path };
                            (c.full_name, Some(loc))
                        })
                        .collect(),
                };
                (ctl, folders)
            })
            .collect();
        let cancel = gio::Cancellable::new();
        *self.search_cancel.borrow_mut() = Some(cancel.clone());
        let header = search_expression(&query, false);
        let body = search_expression(&query, true);
        let this = self.clone();
        glib::spawn_future_local(async move {
            let current = |this: &Rc<Window>| this.search_gen.get() == generation && !cancel.is_cancelled();
            // Summary matches first, for every folder in scope.
            let mut opened: Vec<(Rc<AccountCtl>, Vec<(camel::Folder, Option<String>)>)> = Vec::new();
            for (ctl, folders) in targets {
                let Ok(store) = this.store_for(&ctl) else { continue };
                let mut list = Vec::new();
                for (name, loc) in folders {
                    if !current(&this) {
                        return;
                    }
                    let folder = match this.session.folder_future(&store, &name).await {
                        Ok(f) => f,
                        Err(e) => {
                            crate::debug::log(&format!("search: cannot open {name}: {}", e.message()));
                            continue;
                        }
                    };
                    let (f, e) = (folder.clone(), header.clone());
                    let uids = match gio::spawn_blocking(move || f.search_sync(&e, None::<&gio::Cancellable>)).await {
                        Ok(Ok(u)) => u,
                        Ok(Err(e)) => {
                            crate::debug::log(&format!("search: {name}: {}", e.message()));
                            Vec::new()
                        }
                        Err(_) => Vec::new(),
                    };
                    crate::debug::log(&format!("search: {} {name}: {} summary matches", ctl.account.uid, uids.len()));
                    if !current(&this) {
                        return;
                    }
                    this.search.add(&ctl.account.uid, &folder, &uids, loc.as_deref());
                    list.push((folder, loc));
                }
                opened.push((ctl, list));
            }
            // Body matches from the servers (or cached bodies when offline).
            this.set_searching(opened.len());
            for (ctl, folders) in opened {
                let (this, body, header, cancel) = (this.clone(), body.clone(), header.clone(), cancel.clone());
                glib::spawn_future_local(async move {
                    for (folder, loc) in folders {
                        if this.search_gen.get() != generation || cancel.is_cancelled() {
                            return;
                        }
                        // A folder never opened or kept offline has an empty or
                        // stale summary, and Camel searches only what it lists.
                        let (f, e, h, c) = (folder.clone(), body.clone(), header.clone(), cancel.clone());
                        let uids = gio::spawn_blocking(move || {
                            let _ = f.refresh_info_sync(Some(&c));
                            let mut uids = f.search_sync(&h, Some(&c)).unwrap_or_default();
                            uids.extend(f.search_sync(&e, Some(&c)).unwrap_or_default());
                            uids
                        })
                        .await
                        .unwrap_or_default();
                        crate::debug::log(&format!(
                            "search: {} {}: {} matches after refresh",
                            ctl.account.uid,
                            folder.full_name().unwrap_or_default(),
                            uids.len()
                        ));
                        if this.search_gen.get() != generation || cancel.is_cancelled() {
                            return;
                        }
                        this.search.add(&ctl.account.uid, &folder, &uids, loc.as_deref());
                    }
                    if this.search_gen.get() == generation {
                        this.set_searching(this.searching.get().saturating_sub(1));
                    }
                });
            }
        });
    }

    /// Messages dropped on a folder: the same move as the picker's.
    fn dropped(self: &Rc<Self>, obj: &FolderObject, m: DraggedMessages) {
        let accounts = self.accounts.borrow().clone();
        let Some(account) = accounts.iter().find(|a| a.obj.account_uid() == m.account_uid).cloned() else { return };
        let Some(dest_account) = accounts.iter().find(|a| a.obj.account_uid() == obj.account_uid()).cloned() else { return };
        let full_name = obj.full_name();
        let Some(mut dest) = folder_choices(&dest_account, &[]).into_iter().find(|c| c.full_name == full_name) else { return };
        if dest_account.account.uid != account.account.uid {
            dest.path = format!("{} / {}", dest_account.account.display_name(), dest.path);
        }
        let t = Target { account, folder: m.folder, uids: m.uids, thread: m.thread };
        let this = self.clone();
        glib::spawn_future_local(async move {
            this.transfer(&t, &dest_account, &dest.full_name, &format!("moved to {}", dest.path)).await;
        });
    }

    fn request(self: &Rc<Self>, op: Op, t: Target) {
        self.request_all(op, vec![t]);
    }

    /// `op` on messages from one or more folders. Move asks for the folder
    /// once; flags are set across all of them at once.
    fn request_all(self: &Rc<Self>, op: Op, targets: Vec<Target>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            match op {
                Op::Move if targets.len() > 1 => {
                    let n: usize = targets.iter().map(|t| t.uids.len()).sum();
                    let accounts = this.accounts.borrow().clone();
                    let choices = all_folder_choices(&accounts, None);
                    let title = if n == 1 { "Move message to" } else { "Move messages to" };
                    let Some(dest) = manage::pick_folder(&this.window, title, choices).await else { return };
                    let Some(dest_account) = accounts.iter().find(|a| a.account.uid == dest.account).cloned() else { return };
                    for t in targets {
                        if t.account.account.uid == dest.account && t.folder.full_name().as_deref() == Some(dest.full_name.as_str()) {
                            continue;
                        }
                        this.transfer(&t, &dest_account, &dest.full_name, &format!("moved to {}", dest.path)).await;
                    }
                }
                Op::Read | Op::Star if targets.len() > 1 => {
                    let all: Vec<(camel::Folder, String)> =
                        targets.iter().flat_map(|t| t.uids.iter().map(|u| (t.folder.clone(), u.clone()))).collect();
                    let (mask, set) = if op == Op::Read {
                        (SEEN, if all.iter().any(|(f, u)| f.message_flags(u) & SEEN == 0) { SEEN } else { 0 })
                    } else {
                        (FLAGGED, if all.iter().all(|(f, u)| f.message_flags(u) & FLAGGED != 0) { 0 } else { FLAGGED })
                    };
                    for (f, u) in &all {
                        f.set_message_flags(u, mask, set);
                    }
                    this.update_viewer_read();
                    this.checked_changed();
                }
                _ => {
                    for t in targets {
                        this.perform(op, t).await;
                    }
                }
            }
        });
    }

    async fn perform(self: &Rc<Self>, op: Op, t: Target) {
        let n = t.uids.len();
        let flags: Vec<u32> = t.uids.iter().map(|u| t.folder.message_flags(u)).collect();
        match op {
            Op::Read => {
                let read = flags.iter().any(|f| f & SEEN == 0);
                if self.shows_any(&t) {
                    self.cancel_read_timer();
                }
                set_flags(&t, SEEN, if read { SEEN } else { 0 });
                self.update_viewer_read();
                self.checked_changed();
            }
            Op::Star => {
                let star = !flags.iter().all(|f| f & FLAGGED != 0);
                set_flags(&t, FLAGGED, if star { FLAGGED } else { 0 });
                self.update_viewer_read();
                self.checked_changed();
            }
            Op::Move => {
                let current = t.folder.full_name().map(|n| n.to_string()).unwrap_or_default();
                let accounts = self.accounts.borrow().clone();
                let choices = all_folder_choices(&accounts, Some((t.account.as_ref(), current.as_str())));
                let title = match (t.thread, n) {
                    (true, _) => "Move thread to",
                    (false, 1) => "Move message to",
                    _ => "Move messages to",
                };
                let Some(dest) = manage::pick_folder(&self.window, title, choices).await else { return };
                let Some(dest_account) = accounts.iter().find(|a| a.account.uid == dest.account).cloned() else { return };
                self.transfer(&t, &dest_account, &dest.full_name, &format!("moved to {}", dest.path)).await;
            }
            Op::Archive => {
                let archive = t.account.special.borrow().as_ref().and_then(|s| s.archive.clone());
                let archive = match archive {
                    Some(a) => a,
                    None => match self.set_up_archive(&t.account).await {
                        Some(a) => a,
                        None => return,
                    },
                };
                if t.folder.full_name().as_deref() == Some(archive.as_str()) {
                    return;
                }
                self.transfer(&t, &t.account, &archive, "archived").await;
            }
            Op::Junk => {
                mark_junk(&t, true);
                // In Trash it is only marked, for the server's filter.
                if folder_place(&t.account, &t.folder) == Place::Trash {
                    self.update_targets();
                    return;
                }
                let Some(junk) = t.account.special.borrow().as_ref().and_then(|s| s.junk.clone()) else { return };
                if Settings::get().read_on_junk {
                    self.mark_read_moving(&t);
                }
                self.transfer(&t, &t.account, &junk, "marked as junk").await;
            }
            Op::NotJunk => {
                mark_junk(&t, false);
                self.transfer(&t, &t.account, "INBOX", "moved to Inbox").await;
            }
            Op::Restore => {
                // Back where Salutations moved them from, else to Inbox.
                let mut groups: Vec<(String, Vec<String>)> = Vec::new();
                let mut ids = Vec::new();
                for uid in &t.uids {
                    let id = t.folder.message_info(uid).map_or(0, |i| i.message_id());
                    ids.push(id);
                    let dest = salutations_core::origins::lookup(&t.account.account.uid, id)
                        .filter(|f| t.account.obj.find(f).is_some())
                        .unwrap_or_else(|| "INBOX".to_string());
                    match groups.iter_mut().find(|(d, _)| *d == dest) {
                        Some((_, u)) => u.push(uid.clone()),
                        None => groups.push((dest, vec![uid.clone()])),
                    }
                }
                for (dest, uids) in groups {
                    let path = folder_choices(&t.account, &[])
                        .into_iter()
                        .find(|c| c.full_name == dest)
                        .map_or_else(|| "Inbox".to_string(), |c| c.path);
                    let part = Target { account: t.account.clone(), folder: t.folder.clone(), uids, thread: false };
                    self.transfer(&part, &t.account, &dest, &format!("restored to {path}")).await;
                }
                salutations_core::origins::forget(&ids);
            }
            Op::Delete => {
                let trash = t.account.special.borrow().as_ref().and_then(|s| s.trash.clone());
                let here = t.folder.full_name().map(|n| n.to_string());
                match trash {
                    Some(trash) if here.as_deref() != Some(trash.as_str()) => {
                        if Settings::get().read_on_delete {
                            self.mark_read_moving(&t);
                        }
                        self.transfer(&t, &t.account, &trash, "moved to Trash").await;
                    }
                    trash => {
                        if manage::confirm_permanent(&self.window, n, trash.is_none()).await {
                            self.delete_permanently(&t).await;
                        }
                    }
                }
            }
            Op::Trash => {
                let trash = t.account.special.borrow().as_ref().and_then(|s| s.trash.clone());
                let Some(trash) = trash else { return };
                if t.folder.full_name().as_deref() == Some(trash.as_str()) {
                    return;
                }
                if Settings::get().read_on_delete {
                    self.mark_read_moving(&t);
                }
                self.transfer(&t, &t.account, &trash, "moved to Trash").await;
            }
            Op::DeletePermanently => {
                if manage::confirm_permanent(&self.window, n, false).await {
                    self.delete_permanently(&t).await;
                }
            }
        }
    }

    /// Asks for the account's Archive folder, creating it if asked, and
    /// saves the choice. None when cancelled or on error.
    async fn set_up_archive(self: &Rc<Self>, ctl: &Rc<AccountCtl>) -> Option<String> {
        let store = self.store_for(ctl).ok()?;
        let name = match manage::ask_archive_setup(&self.window).await? {
            manage::ArchiveSetup::Choose => {
                let special = ctl.special.borrow().clone().unwrap_or_default();
                let skip: Vec<String> = ["INBOX".to_string()]
                    .into_iter()
                    .chain([special.trash, special.junk, special.drafts].into_iter().flatten())
                    .collect();
                let skip: Vec<&str> = skip.iter().map(|s| s.as_str()).collect();
                let choices: Vec<FolderChoice> = folder_choices(ctl, &[])
                    .into_iter()
                    .filter(|c| !skip.iter().any(|s| s.eq_ignore_ascii_case(&c.full_name)))
                    .collect();
                manage::pick_folder(&self.window, "Archive folder", choices).await?.full_name
            }
            manage::ArchiveSetup::Create => {
                const NAME: &str = "Archive";
                if ctl.obj.find(NAME).is_none() {
                    let s = store.clone();
                    let created = gio::spawn_blocking(move || s.create_folder_sync(None, NAME, None::<&gio::Cancellable>))
                        .await
                        .unwrap_or_else(|_| Err(glib::Error::new(gio::IOErrorEnum::Failed, "Folder creation failed")));
                    if let Err(e) = created {
                        self.toast(&format!("Cannot create the Archive folder: {}", e.message()), false);
                        return None;
                    }
                }
                NAME.to_string()
            }
        };
        let mut special = ctl.special.borrow().clone().unwrap_or_default();
        special.archive = Some(name.clone());
        special.archive_chosen = true;
        if let Err(e) = special.save(&ctl.account.uid) {
            glib::g_warning!("salutations", "Cannot save special folders: {}", e);
        }
        *ctl.special.borrow_mut() = Some(special);
        // The tree gets the new folder, or the chosen one's archive icon.
        if let Ok(nodes) = self.session.folders_future(&store, true).await {
            self.set_folders(ctl, &nodes, true);
        }
        Some(name)
    }

    /// Whether the shown message is among the target's.
    fn shows_any(&self, t: &Target) -> bool {
        self.viewer.target().is_some_and(|(f, u)| f == t.folder && t.uids.contains(&u))
    }

    /// Marks the target read before it moves to Trash or Junk; its undo
    /// makes the ones that were unread unread again.
    fn mark_read_moving(&self, t: &Target) {
        let unread: Vec<String> = t.uids.iter().filter(|u| t.folder.message_flags(u) & SEEN == 0).cloned().collect();
        set_flags(t, SEEN, SEEN);
        *self.unread_before.borrow_mut() = unread;
    }

    /// Moves the target to `dest` in `dest_account`, offering undo. `verb`
    /// completes the toast: "moved to Trash" or "moved to <path>". Within
    /// an account the move is queued and the toast shows at once.
    async fn transfer(self: &Rc<Self>, t: &Target, dest_account: &Rc<AccountCtl>, dest: &str, verb: &str) {
        let count = t.uids.len();
        if Rc::ptr_eq(dest_account, &t.account) {
            let job = self.enqueue(t, dest, false);
            self.push_undo(dest, verb, count, Undo::Queued(job));
            return;
        }
        let unread_before = self.unread_before.take();
        let Some((dest_folder, new_uids)) = self.transfer_raw(t, dest_account, dest).await else { return };
        if new_uids.len() != count || new_uids.iter().any(|u| u.is_none()) {
            let b = Batch { dest: dest.to_string(), verb: verb.to_string(), count, moves: Vec::new() };
            self.toast(&b.text(), false);
            return;
        }
        let unread: Vec<String> = t
            .uids
            .iter()
            .zip(&new_uids)
            .filter(|(old, _)| unread_before.contains(old))
            .filter_map(|(_, new)| new.clone())
            .collect();
        let undo = Undo::Done { from: dest_folder, uids: new_uids.into_iter().flatten().collect(), to: t.folder.clone(), unread };
        self.push_undo(dest, verb, count, undo);
    }

    /// Adds `undo` to the last batch while its toast is shown and it is
    /// the same action, else starts a batch; shows the toast.
    fn push_undo(self: &Rc<Self>, dest: &str, verb: &str, count: usize, undo: Undo) {
        let merged = {
            let mut stack = self.undo.borrow_mut();
            match stack.last_mut() {
                Some(b) if self.toast.borrow().is_some() && b.dest == dest && b.verb == verb => {
                    b.count += count;
                    b.moves.push(undo);
                    b.text()
                }
                _ => {
                    let b = Batch { dest: dest.to_string(), verb: verb.to_string(), count, moves: vec![undo] };
                    let text = b.text();
                    stack.push(b);
                    text
                }
            }
        };
        self.toast(&merged, true);
    }

    /// Takes the target out of the list and queues its move to `dest` in
    /// the same account. Opens the next message if the shown one moved.
    fn enqueue(self: &Rc<Self>, t: &Target, dest: &str, permanent: bool) -> Rc<Job> {
        let unread = self.unread_before.take();
        let shown = self.shows_any(t);
        let neighbor = if shown && !self.showing_results() { self.list.neighbor(&t.uids) } else { None };
        let trash = t.account.special.borrow().as_ref().and_then(|s| s.trash.clone());
        if trash.as_deref() == Some(dest) {
            let ids: Vec<u64> = t.uids.iter().filter_map(|u| t.folder.message_info(u)).map(|i| i.message_id()).collect();
            if let Some(from) = t.folder.full_name() {
                salutations_core::origins::record(&t.account.account.uid, &from, &ids);
            }
        }
        let job = Rc::new(Job {
            account: t.account.clone(),
            folder: t.folder.clone(),
            uids: t.uids.clone(),
            dest: dest.to_string(),
            permanent,
            unread,
            state: RefCell::new(JobState::Waiting),
        });
        self.queue.borrow_mut().push(job.clone());
        crate::debug::log(&format!("queue: {:?} to {dest} queued", t.uids));
        if shown {
            self.cancel_read_timer();
        }
        self.search.remove(&t.folder, &t.uids);
        self.relist(&t.account, &t.folder);
        if shown {
            self.after_removal(neighbor);
        }
        self.run_queue(&t.account);
        job
    }

    /// Rebuilds the list if it shows `folder`, and its count.
    fn relist(&self, ctl: &Rc<AccountCtl>, folder: &camel::Folder) {
        if self.open.borrow().as_ref().is_some_and(|o| &o.folder == folder) {
            self.list.reload(self.list.selected_uid());
            self.update_empty_banner();
            self.checked_changed();
            self.update_viewer_read();
        }
        self.show_count(ctl, folder);
    }

    /// Works through `account`'s jobs unless already doing so.
    fn run_queue(self: &Rc<Self>, account: &Rc<AccountCtl>) {
        let id = account.account.uid.clone();
        if !self.working.borrow_mut().insert(id.clone()) {
            return;
        }
        let (this, account) = (self.clone(), account.clone());
        glib::spawn_future_local(async move {
            loop {
                let batch = this.next_batch(&account);
                if batch.is_empty() {
                    break;
                }
                this.run_jobs(&account, &batch).await;
            }
            this.working.borrow_mut().remove(&id);
        });
    }

    /// The account's first waiting job and those after it with the same
    /// folder and destination, which go to the server as one move.
    fn next_batch(&self, account: &Rc<AccountCtl>) -> Vec<Rc<Job>> {
        let queue = self.queue.borrow();
        let mut mine = queue.iter().filter(|j| Rc::ptr_eq(&j.account, account) && j.waiting());
        let Some(first) = mine.next() else { return Vec::new() };
        let mut batch = vec![first.clone()];
        for j in mine {
            if j.folder != first.folder || j.dest != first.dest || j.permanent != first.permanent {
                break;
            }
            batch.push(j.clone());
        }
        for j in &batch {
            *j.state.borrow_mut() = JobState::Running;
        }
        crate::debug::log(&format!("queue: {} jobs to {} started", batch.len(), first.dest));
        batch
    }

    async fn run_jobs(self: &Rc<Self>, account: &Rc<AccountCtl>, batch: &[Rc<Job>]) {
        let first = &batch[0];
        let uids: Vec<String> = batch.iter().flat_map(|j| j.uids.iter().cloned()).collect();
        match self.move_on_server(&first.folder, &uids, account, &first.dest).await {
            Ok((dest_folder, new_uids)) => {
                let mut rest = new_uids.as_slice();
                for j in batch {
                    let (mine, after) = rest.split_at(j.uids.len());
                    rest = after;
                    *j.state.borrow_mut() = if mine.iter().all(Option::is_some) {
                        JobState::Moved(dest_folder.clone(), mine.iter().flatten().cloned().collect())
                    } else {
                        JobState::Lost
                    };
                }
                if first.permanent {
                    self.expunge(&dest_folder, new_uids).await;
                }
                self.refresh_count(account, &dest_folder);
            }
            Err(e) => {
                for j in batch {
                    *j.state.borrow_mut() = JobState::Failed;
                    mark_unread(&j.folder, &j.unread);
                }
                let n = uids.len();
                let what = if first.permanent { "delete" } else { "move" };
                self.toast(&format!("Cannot {what} {n} message{}: {e}", if n == 1 { "" } else { "s" }), false);
            }
        }
        self.queue.borrow_mut().retain(|j| !batch.iter().any(|b| Rc::ptr_eq(b, j)));
        crate::debug::log(&format!("queue: {} jobs to {} ended", batch.len(), first.dest));
        // Failed messages come back; moved ones have left the summary.
        self.relist(account, &first.folder);
    }

    /// Deletes for good messages just moved to Trash.
    async fn expunge(self: &Rc<Self>, trash: &camel::Folder, uids: Vec<Option<String>>) {
        if uids.iter().any(|u| u.is_none()) {
            self.toast("Moved to Trash; the server did not say where, so it was not deleted", false);
            return;
        }
        trash.freeze();
        for uid in uids.iter().flatten() {
            trash.set_message_flags(uid, DELETED | SEEN, DELETED | SEEN);
        }
        trash.thaw();
        if let Err(e) = trash.synchronize_future(true, glib::ffi::G_PRIORITY_DEFAULT).await {
            self.toast(&format!("Cannot delete messages: {}", e.message()), false);
        }
    }

    /// Drops a job that has not started; its messages come back.
    fn cancel_job(&self, job: &Rc<Job>) {
        *job.state.borrow_mut() = JobState::Cancelled;
        crate::debug::log(&format!("queue: {:?} cancelled", job.uids));
        self.queue.borrow_mut().retain(|j| !Rc::ptr_eq(j, job));
        mark_unread(&job.folder, &job.unread);
        self.relist(&job.account, &job.folder);
    }

    /// Before the process ends: waits for queued moves, showing how many
    /// are left, until they are done or the user quits anyway. Moves not
    /// started by then are dropped and their messages stay where they were.
    pub(crate) async fn finish_queue(self: &Rc<Self>) {
        if self.queue.borrow().is_empty() {
            return;
        }
        let heading = |n: usize| format!("Finishing {n} action{}…", if n == 1 { "" } else { "s" });
        let quit_now = Rc::new(Cell::new(false));
        let dialog = self.window.is_visible().then(|| {
            let d = adw::AlertDialog::builder()
                .heading(heading(self.queue.borrow().len()))
                .body("Messages not moved yet stay where they are if you quit now.")
                .build();
            d.add_response("quit", "_Quit now");
            d.set_response_appearance("quit", adw::ResponseAppearance::Destructive);
            d.set_close_response("quit");
            let q = quit_now.clone();
            d.connect_response(None, move |_, _| q.set(true));
            d.present(Some(&self.window));
            d
        });
        // Without a window to ask in, the queue gets a limited time.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let n = self.queue.borrow().len();
            if n == 0 || quit_now.get() || (dialog.is_none() && std::time::Instant::now() > deadline) {
                break;
            }
            if let Some(d) = &dialog {
                d.set_heading(Some(&heading(n)));
            }
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
        }
        if let Some(d) = &dialog {
            d.force_close();
        }
        let left: Vec<Rc<Job>> = self.queue.borrow().clone();
        for job in left {
            if job.waiting() {
                self.cancel_job(&job);
            }
        }
        // A running move may still finish before the process ends.
        self.queue.borrow_mut().clear();
    }

    /// Moves between accounts: each message is downloaded and uploaded
    /// again, and the list waits for it. Returns the destination folder and
    /// the messages' UIDs there; opens the next message if the shown one
    /// moved.
    async fn transfer_raw(
        self: &Rc<Self>,
        t: &Target,
        dest_account: &Rc<AccountCtl>,
        dest: &str,
    ) -> Option<(camel::Folder, Vec<Option<String>>)> {
        let shown = self.shows_any(t);
        let neighbor = if shown && !self.showing_results() { self.list.neighbor(&t.uids) } else { None };
        let n = t.uids.len();
        self.toast(&format!("Moving {n} message{} to {}…", if n == 1 { "" } else { "s" }, dest_account.account.display_name()), false);
        if shown {
            self.cancel_read_timer();
        }
        match self.move_on_server(&t.folder, &t.uids, dest_account, dest).await {
            Ok((dest_folder, new_uids)) => {
                self.search.remove(&t.folder, &t.uids);
                if shown {
                    self.after_removal(neighbor);
                }
                self.refresh_count(dest_account, &dest_folder);
                Some((dest_folder, new_uids))
            }
            Err(e) => {
                self.toast(&format!("Cannot move messages: {e}"), false);
                None
            }
        }
    }

    /// Moves `uids` from `folder` to `dest` in `dest_account`; returns the
    /// destination folder and the messages' UIDs there, None for any the
    /// server did not report.
    async fn move_on_server(
        &self,
        folder: &camel::Folder,
        uids: &[String],
        dest_account: &Rc<AccountCtl>,
        dest: &str,
    ) -> Result<(camel::Folder, Vec<Option<String>>), String> {
        let store = self.store_for(dest_account).map_err(|e| e.message().to_string())?;
        let dest_folder = self
            .session
            .folder_future(&store, dest)
            .await
            .map_err(|e| format!("cannot open {dest}: {}", e.message()))?;
        // IMAPX does not report the new UIDs; it adds the messages to the
        // destination's summary, where they are found by comparing.
        let before: HashSet<String> = dest_folder.uids().iter().map(|u| u.to_string()).collect();
        let keys: Vec<MessageKey> = uids.iter().filter_map(|u| message_key(folder, u)).collect();
        if dest.eq_ignore_ascii_case("INBOX") {
            dest_account.expected.borrow_mut().extend(keys.iter().cloned());
        }
        let (src, to, list) = (folder.clone(), dest_folder.clone(), uids.to_vec());
        let mut new_uids = gio::spawn_blocking(move || src.transfer_messages_to_sync(&list, &to, true, None::<&gio::Cancellable>))
            .await
            .unwrap_or_else(|_| Err(glib::Error::new(gio::IOErrorEnum::Failed, "Transfer failed")))
            .map_err(|e| e.message().to_string())?;
        if new_uids.len() != uids.len() || new_uids.iter().any(|u| u.is_none()) {
            let mut keys = keys;
            let found: Vec<Option<String>> = dest_folder
                .uids()
                .iter()
                .map(|u| u.to_string())
                .filter(|u| !before.contains(u))
                .filter(|u| {
                    let Some(k) = message_key(&dest_folder, u) else { return false };
                    let Some(i) = keys.iter().position(|x| *x == k) else { return false };
                    keys.swap_remove(i);
                    true
                })
                .map(Some)
                .collect();
            new_uids = if found.len() == uids.len() { found } else { vec![None; uids.len()] };
        }
        Ok((dest_folder, new_uids))
    }

    /// Deletes for good. Outside Trash the messages go to Trash first
    /// (queued): IMAPX moves messages flagged deleted in other folders to
    /// Trash instead of expunging them when the account has a Trash folder.
    async fn delete_permanently(self: &Rc<Self>, t: &Target) {
        let trash = t.account.special.borrow().as_ref().and_then(|s| s.trash.clone());
        let here = t.folder.full_name().map(|n| n.to_string());
        if let Some(trash) = trash.filter(|trash| here.as_deref() != Some(trash.as_str())) {
            self.enqueue(t, &trash, true);
            return;
        }
        if self.shows_any(t) {
            let neighbor = self.list.neighbor(&t.uids);
            self.cancel_read_timer();
            self.after_removal(neighbor);
        }
        self.search.remove(&t.folder, &t.uids);
        self.expunge(&t.folder, t.uids.iter().cloned().map(Some).collect()).await;
    }

    /// After the shown message left the folder: on a phone, back to the
    /// list; otherwise the next message opens.
    fn after_removal(&self, neighbor: Option<String>) {
        if self.showing_results() {
            self.search.unselect();
            self.viewer.clear();
            if self.inner.is_collapsed() {
                self.inner.set_show_content(false);
            }
        } else if self.inner.is_collapsed() {
            self.inner.set_show_content(false);
        } else if let Some(uid) = neighbor {
            self.list.open_uid(&uid);
        } else {
            self.list.unselect();
            self.viewer.clear();
        }
        self.update_viewer_read();
    }

    /// Refreshes `folder` and shows its unread count in the folder tree.
    fn refresh_count(self: &Rc<Self>, ctl: &Rc<AccountCtl>, folder: &camel::Folder) {
        let (this, ctl, folder) = (self.clone(), ctl.clone(), folder.clone());
        glib::spawn_future_local(async move {
            let _ = folder.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT).await;
            this.show_count(&ctl, &folder);
        });
    }

    /// Shows `folder`'s unread count in the folder tree, leaving out
    /// messages whose move is waiting.
    fn show_count(&self, ctl: &Rc<AccountCtl>, folder: &camel::Folder) {
        if let Some(name) = folder.full_name()
            && let Some(obj) = ctl.obj.find(&name)
            && let Some(s) = folder.folder_summary()
        {
            obj.set_unread(s.unread_count() as i32 - self.hidden_unread(folder));
        }
    }

    fn is_hidden(&self, folder: &camel::Folder, uid: &str) -> bool {
        self.queue.borrow().iter().any(|j| &j.folder == folder && j.pending() && j.uids.iter().any(|u| u == uid))
    }

    fn hidden_unread(&self, folder: &camel::Folder) -> i32 {
        self.queue
            .borrow()
            .iter()
            .filter(|j| &j.folder == folder && j.pending())
            .flat_map(|j| j.uids.iter())
            .filter(|u| folder.message_flags(u) & SEEN == 0)
            .count() as i32
    }

    /// Shows `text`, replacing the current toast. With `undo`, the toast
    /// offers to undo the last batch.
    fn toast(self: &Rc<Self>, text: &str, undo: bool) {
        // dismiss() emits "dismissed" at once, whose handler borrows the cell.
        let old = self.toast.borrow_mut().take();
        if let Some(old) = old {
            old.dismiss();
        }
        let toast = adw::Toast::new(text);
        if undo {
            toast.set_button_label(Some("_Undo"));
            toast.set_action_name(Some("win.undo"));
            *self.toast.borrow_mut() = Some(toast.clone());
            // Merging stops once the toast is gone; Ctrl+Z still works.
            let weak = Rc::downgrade(self);
            toast.connect_dismissed(move |t| {
                if let Some(this) = weak.upgrade() {
                    let mut cur = this.toast.borrow_mut();
                    if cur.as_ref() == Some(t) {
                        *cur = None;
                    }
                }
            });
        }
        self.toasts.add_toast(toast);
    }

    /// Reverses the last batch, its toast's or not.
    fn undo(self: &Rc<Self>) {
        let Some(batch) = self.undo.borrow_mut().pop() else { return };
        let shown = self.toast.borrow_mut().take();
        if let Some(t) = shown {
            t.dismiss();
        }
        // Moves still waiting are dropped before the queue can start them.
        for u in &batch.moves {
            if let Undo::Queued(job) = u
                && job.waiting()
            {
                self.cancel_job(job);
            }
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            // The batch's first message, back where it was, is opened again.
            let mut reopen: Option<(camel::Folder, String)> = None;
            for u in batch.moves.into_iter().rev() {
                let (from, uids, to, unread) = match u {
                    Undo::Done { from, uids, to, unread } => (from, uids, to, unread),
                    Undo::Queued(job) => {
                        while matches!(*job.state.borrow(), JobState::Running) {
                            glib::timeout_future(std::time::Duration::from_millis(100)).await;
                        }
                        if matches!(*job.state.borrow(), JobState::Cancelled) {
                            reopen = job.uids.first().map(|u| (job.folder.clone(), u.clone()));
                            continue;
                        }
                        let JobState::Moved(from, uids) = &*job.state.borrow() else { continue };
                        let unread = job
                            .uids
                            .iter()
                            .zip(uids)
                            .filter(|(old, _)| job.unread.contains(old))
                            .map(|(_, new)| new.clone())
                            .collect();
                        (from.clone(), uids.clone(), job.folder.clone(), unread)
                    }
                };
                let (Some(ctl), Some(dest)) = (this.account_of(&to), to.full_name()) else { continue };
                // Unread again before moving back; the flag travels along.
                mark_unread(&from, &unread);
                let back = match this.move_on_server(&from, &uids, &ctl, &dest).await {
                    Ok((_, back)) => back,
                    Err(e) => {
                        this.toast(&format!("Cannot undo: {e}"), false);
                        return;
                    }
                };
                reopen = back.into_iter().next().flatten().map(|u| (to.clone(), u));
                for f in [&from, &to] {
                    if let Some(ctl) = this.account_of(f) {
                        this.refresh_count(&ctl, f);
                    }
                }
            }
            if let Some((folder, uid)) = reopen
                && this.open.borrow().as_ref().is_some_and(|o| o.folder == folder)
                && !this.list.select_mode()
                && !this.showing_results()
            {
                this.list.open_uid(&uid);
            }
        });
    }
}

/// Clears the read flag of `uids` in `folder`.
fn mark_unread(folder: &camel::Folder, uids: &[String]) {
    if uids.is_empty() {
        return;
    }
    folder.freeze();
    for uid in uids {
        folder.set_message_flags(uid, SEEN, 0);
    }
    folder.thaw();
}

fn notify_state_path() -> std::path::PathBuf {
    salutations_core::data_dir().join("notify-state.ini")
}

fn load_last_uid(account: &str) -> Option<u64> {
    let kf = glib::KeyFile::new();
    kf.load_from_file(notify_state_path(), glib::KeyFileFlags::NONE).ok()?;
    kf.uint64(account, "last-uid").ok()
}

fn save_last_uid(account: &str, uid: u64) {
    let kf = glib::KeyFile::new();
    let path = notify_state_path();
    let _ = kf.load_from_file(&path, glib::KeyFileFlags::NONE);
    kf.set_uint64(account, "last-uid", uid);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = kf.save_to_file(&path) {
        glib::g_warning!("salutations", "Cannot save notification state: {}", e);
    }
}

/// What identifies a message across folders when its new UID is unknown.
type MessageKey = (String, i64, String);

fn message_key(folder: &camel::Folder, uid: &str) -> Option<MessageKey> {
    let info = folder.message_info(uid)?;
    Some((
        info.subject().map(|s| s.to_string()).unwrap_or_default(),
        info.date_sent(),
        info.from().map(|s| s.to_string()).unwrap_or_default(),
    ))
}

/// A Camel search over From, To, Cc and Subject in the summary, or over
/// the body (`body`), which IMAPX sends to the server when online.
fn search_expression(query: &str, body: bool) -> String {
    let q = query.replace('\\', "\\\\").replace('"', "\\\"");
    if body {
        format!("(match-all (body-contains \"{q}\"))")
    } else {
        let any: Vec<String> = ["from", "to", "cc", "subject"]
            .iter()
            .map(|h| format!("(header-contains \"{h}\" \"{q}\")"))
            .collect();
        format!("(match-all (or {}))", any.join(" "))
    }
}

/// Whether `folder` is its account's Trash or Junk.
fn folder_place(ctl: &AccountCtl, folder: &camel::Folder) -> Place {
    let name = folder.full_name().map(|n| n.to_string());
    let special = ctl.special.borrow();
    let is = |f: Option<&String>| f.is_some() && f == name.as_ref();
    match special.as_ref() {
        Some(s) if is(s.trash.as_ref()) => Place::Trash,
        Some(s) if is(s.junk.as_ref()) => Place::Junk,
        _ => Place::Ordinary,
    }
}

/// Folders of every account for the move picker, each account's under a
/// heading when there are several; `exclude` is one account's folder.
fn all_folder_choices(accounts: &[Rc<AccountCtl>], exclude: Option<(&AccountCtl, &str)>) -> Vec<FolderChoice> {
    let mut out = Vec::new();
    for ctl in accounts {
        let skip: Vec<&str> = match exclude {
            Some((a, name)) if std::ptr::eq(a, ctl.as_ref()) => vec![name],
            _ => Vec::new(),
        };
        let mut choices = folder_choices(ctl, &skip);
        if accounts.len() > 1 {
            out.push(FolderChoice {
                account: ctl.account.uid.clone(),
                full_name: String::new(),
                path: ctl.account.display_name(),
                title: ctl.account.display_name(),
                icon: String::new(),
                depth: 0,
                heading: true,
            });
            for c in &mut choices {
                c.path = format!("{} / {}", ctl.account.display_name(), c.path);
            }
        }
        out.extend(choices);
    }
    out
}

fn is_archive(ctl: &AccountCtl, folder: &camel::Folder) -> bool {
    let archive = ctl.special.borrow().as_ref().and_then(|s| s.archive.clone());
    archive.is_some() && folder.full_name().map(|n| n.to_string()) == archive
}

/// Junk or not junk, for the server's filter: Camel's flags (sent as the
/// `Junk` and `NotJunk` keywords) and the `$Junk` and `$NotJunk` keywords.
fn mark_junk(t: &Target, junk: bool) {
    const JUNK: u32 = camel::MessageFlags::JUNK.bits();
    const NOTJUNK: u32 = camel::MessageFlags::NOTJUNK.bits();
    set_flags(t, JUNK | NOTJUNK, if junk { JUNK } else { NOTJUNK });
    for uid in &t.uids {
        if let Some(info) = t.folder.message_info(uid) {
            camel::message_info_set_user_flag(&info, "$Junk", junk);
            camel::message_info_set_user_flag(&info, "$NotJunk", !junk);
        }
    }
}

fn set_flags(t: &Target, mask: u32, set: u32) {
    t.folder.freeze();
    for uid in &t.uids {
        t.folder.set_message_flags(uid, mask, set);
    }
    t.folder.thaw();
}

/// The account's folders for the picker, tree order, without `exclude`.
fn folder_choices(ctl: &AccountCtl, exclude: &[&str]) -> Vec<FolderChoice> {
    fn walk(store: &gio::ListStore, prefix: &str, depth: usize, exclude: &[&str], out: &mut Vec<FolderChoice>) {
        for i in 0..store.n_items() {
            let Some(o) = store.item(i).and_downcast::<FolderObject>() else { continue };
            let path = if prefix.is_empty() { o.title() } else { format!("{prefix} / {}", o.title()) };
            if !exclude.contains(&o.full_name().as_str()) {
                out.push(FolderChoice {
                    account: o.account_uid(),
                    full_name: o.full_name(),
                    path: path.clone(),
                    title: o.title(),
                    icon: o.icon_name(),
                    depth,
                    heading: false,
                });
            }
            walk(o.children(), &path, depth + 1, exclude, out);
        }
    }
    let mut out = Vec::new();
    walk(ctl.obj.children(), "", 0, exclude, &mut out);
    out
}

fn special_icons(s: &SpecialFolders) -> Vec<(&str, &'static str)> {
    [
        (&s.drafts, "document-edit-symbolic"),
        (&s.sent, "mail-send-symbolic"),
        (&s.archive, "salutations-archive-symbolic"),
        (&s.junk, "mail-mark-junk-symbolic"),
        (&s.trash, "user-trash-symbolic"),
    ]
    .into_iter()
    .filter_map(|(name, icon)| name.as_deref().map(|n| (n, icon)))
    .collect()
}

/// Top level: INBOX, Drafts, Sent, Junk, Trash, Archive, then the rest by
/// name. Subfolders keep the server's order.
fn sort_folders(nodes: &[camel::FolderNode], s: &SpecialFolders) -> Vec<camel::FolderNode> {
    let rank = |n: &camel::FolderNode| -> usize {
        let is = |f: &Option<String>| f.as_deref() == Some(n.full_name.as_str());
        match () {
            _ if n.full_name.eq_ignore_ascii_case("INBOX") => 0,
            _ if is(&s.drafts) => 1,
            _ if is(&s.sent) => 2,
            _ if is(&s.junk) => 3,
            _ if is(&s.trash) => 4,
            _ if is(&s.archive) => 5,
            _ => 6,
        }
    };
    let mut out = nodes.to_vec();
    out.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()))
    });
    out
}

fn metered() -> bool {
    gio::NetworkMonitor::default().is_network_metered()
}

fn power_saver() -> bool {
    gio::PowerProfileMonitor::get_default().is_power_saver_enabled()
}

enum Either<A, B> {
    Left(A),
    Right(B),
}

/// Whichever of `a` and `b` finishes first; the other is dropped.
async fn race<A: std::future::Future, B: std::future::Future>(a: A, b: B) -> Either<A::Output, B::Output> {
    let (mut a, mut b) = (std::pin::pin!(a), std::pin::pin!(b));
    std::future::poll_fn(move |cx| {
        if let std::task::Poll::Ready(v) = a.as_mut().poll(cx) {
            return std::task::Poll::Ready(Either::Left(v));
        }
        if let std::task::Poll::Ready(v) = b.as_mut().poll(cx) {
            return std::task::Poll::Ready(Either::Right(v));
        }
        std::task::Poll::Pending
    })
    .await
}

/// `step`, failing as timed out after `limit`.
async fn within<T>(limit: std::time::Duration, step: impl std::future::Future<Output = Result<T, glib::Error>>) -> Result<T, glib::Error> {
    glib::future_with_timeout(limit, step)
        .await
        .unwrap_or_else(|_| Err(glib::Error::new(gio::IOErrorEnum::TimedOut, "The server did not answer in time")))
}

fn is_offline(e: &glib::Error) -> bool {
    use gio::IOErrorEnum as E;
    [E::NetworkUnreachable, E::HostUnreachable, E::HostNotFound, E::TimedOut, E::ConnectionRefused]
        .iter()
        .any(|k| e.matches(*k))
        || e.matches(camel::ServiceError::Unavailable)
}

/// The message list may take at most this share of the space beside the
/// folders, so the message stays readable as the window narrows.
const LIST_FRACTION: f64 = 0.6;
/// The narrowest the list is dragged to, in sp.
const LIST_MIN: f64 = 260.0;

impl Window {
    fn connect_layout(self: &Rc<Self>) {
        self.apply_layout();
        let weak = Rc::downgrade(self);
        self.inner.connect_collapsed_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.apply_layout();
            }
        });
        let weak = Rc::downgrade(self);
        crate::preferences::connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.apply_layout();
            }
        });
        let weak = Rc::downgrade(self);
        self.content_stack.connect_visible_child_name_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.apply_layout();
            }
        });
        let weak = Rc::downgrade(self);
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
        self.paned.connect_position_notify(move |p| {
            let Some(this) = weak.upgrade() else { return };
            if this.layout.visible_child_name().as_deref() != Some("below") || !this.list_side.is_visible() {
                return;
            }
            if this.divider_set.get().is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(1)) {
                return;
            }
            // Saved once the drag settles, not on every step.
            let height = p.position();
            if let Some(id) = pending.take() {
                id.remove();
            }
            let pend = pending.clone();
            let id = glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                pend.take();
                let mut s = Settings::get();
                s.list_height = Some(height);
                let _ = s.save();
            });
            *pending.borrow_mut() = Some(id);
        });
    }

    /// Puts the message beside or below the list, as the setting and the
    /// window's width allow. While composing below, the composer takes the
    /// whole height.
    fn apply_layout(&self) {
        let below = Settings::get().message_below && !self.inner.is_collapsed();
        let now_below = self.layout.visible_child_name().as_deref() == Some("below");
        if below && !now_below {
            self.list_page.set_child(None::<&gtk::Widget>);
            self.viewer_page.set_child(None::<&gtk::Widget>);
            self.paned.set_start_child(Some(&self.list_side));
            self.paned.set_end_child(Some(&self.viewer_side));
            // Before the window is shown its height is 0.
            let window = match self.window.height() {
                0 => self.window.default_height(),
                h => h,
            };
            let height = Settings::get().list_height.unwrap_or(window * 2 / 5).max(120);
            self.divider_set.set(Some(std::time::Instant::now()));
            self.paned.set_position(height);
            self.layout.set_visible_child_name("below");
        } else if !below && now_below {
            self.paned.set_start_child(None::<&gtk::Widget>);
            self.paned.set_end_child(None::<&gtk::Widget>);
            self.list_page.set_child(Some(&self.list_side));
            self.viewer_page.set_child(Some(&self.viewer_side));
            self.layout.set_visible_child_name("beside");
        }
        // Below, the list's header bar is the top one and holds the window
        // buttons; the split view decides them beside.
        self.viewer_header.set_show_end_title_buttons(!below);
        self.width_handle.set_visible(!below && !self.inner.is_collapsed());
        let composing = self.content_stack.visible_child_name().as_deref() == Some("composer");
        self.list_side.set_visible(!(below && composing));
    }
}

/// Makes `width` sp the list's width, or less when the window is too narrow
/// for it.
fn set_list_width(inner: &adw::NavigationSplitView, width: f64) {
    let width = width.max(LIST_MIN);
    inner.set_min_sidebar_width(LIST_MIN);
    inner.set_max_sidebar_width(width);
    inner.set_sidebar_width_fraction(LIST_FRACTION);
}
