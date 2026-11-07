//! The account and folder tree.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use salutations_core::camel::{self, prelude::*};

use crate::folder_object::{FolderObject, SyncState};

/// What a message row drag carries; it never leaves the process.
#[derive(Clone, glib::Boxed)]
#[boxed_type(name = "SalutationsDraggedMessages")]
pub struct DraggedMessages {
    pub account_uid: String,
    pub folder: camel::Folder,
    pub uids: Vec<String>,
    /// From a thread's summary row: all of its messages.
    pub thread: bool,
}

/// How long a drag must rest on a collapsed row before it expands.
const EXPAND_DELAY_MS: u64 = 700;

pub struct FolderPane {
    pub widget: gtk::ScrolledWindow,
    pub accounts: gio::ListStore,
    tree: gtk::TreeListModel,
    selection: gtk::SingleSelection,
}

impl FolderPane {
    /// `on_folder` gets each newly selected folder; `on_pick` runs when the
    /// user clicks or activates a folder row, even the selected one;
    /// `on_sync` gets an account row's sync button presses; `on_drop` gets
    /// messages dropped on a folder; `on_files` gets files dropped on one.
    pub fn new(
        on_folder: impl Fn(&FolderObject) + 'static,
        on_pick: impl Fn() + 'static,
        on_sync: impl Fn(&FolderObject, &gtk::Button) + 'static,
        on_drop: impl Fn(&FolderObject, DraggedMessages) + 'static,
        on_files: impl Fn(&FolderObject, Vec<gio::File>) + 'static,
        on_menu: impl Fn(&FolderObject, &gtk::Widget, f64, f64) + 'static,
    ) -> Rc<Self> {
        let on_menu: Rc<dyn Fn(&FolderObject, &gtk::Widget, f64, f64)> = Rc::new(on_menu);
        let on_drop: Rc<dyn Fn(&FolderObject, DraggedMessages)> = Rc::new(on_drop);
        let on_files: Rc<dyn Fn(&FolderObject, Vec<gio::File>)> = Rc::new(on_files);
        let on_pick = Rc::new(on_pick);
        let accounts = gio::ListStore::new::<FolderObject>();
        let tree = gtk::TreeListModel::new(accounts.clone(), false, true, |item| {
            // An account's folders arrive after its row exists, so accounts
            // always get their (possibly empty) child model. Folder rows are
            // rebuilt whenever the tree changes.
            let obj = item.downcast_ref::<FolderObject>()?;
            (obj.is_account() || obj.children().n_items() > 0).then(|| obj.children().clone().upcast())
        });
        let selection = gtk::SingleSelection::builder()
            .model(&tree)
            .autoselect(false)
            .can_unselect(true)
            .build();
        let factory = gtk::SignalListItemFactory::new();
        let pick = on_pick.clone();
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
            let expander = gtk::TreeExpander::builder().indent_for_icon(true).build();
            let click = gtk::GestureClick::new();
            let weak = item.downgrade();
            let pick = pick.clone();
            click.connect_released(move |_, _, _, _| {
                if weak.upgrade().is_some_and(|i| is_folder_row(&i)) {
                    pick();
                }
            });
            expander.add_controller(click);
            // A right click or long press asks for the folder's menu.
            let menu_click = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_SECONDARY).build();
            let long = gtk::GestureLongPress::builder().touch_only(true).build();
            for gesture in [menu_click.upcast_ref::<gtk::Gesture>().clone(), long.upcast_ref::<gtk::Gesture>().clone()] {
                let (weak, on_menu) = (item.downgrade(), on_menu.clone());
                let fire = move |g: &gtk::Gesture, x: f64, y: f64| {
                    let Some(item) = weak.upgrade() else { return };
                    let Some(obj) = item.item().and_downcast::<gtk::TreeListRow>().and_then(|r| r.item()).and_downcast::<FolderObject>()
                    else {
                        return;
                    };
                    let Some(w) = g.widget() else { return };
                    if !obj.is_account() {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        on_menu(&obj, &w, x, y);
                    }
                };
                if let Some(c) = gesture.downcast_ref::<gtk::GestureClick>() {
                    c.connect_pressed(move |g, _, x, y| fire(g.upcast_ref(), x, y));
                } else if let Some(l) = gesture.downcast_ref::<gtk::GestureLongPress>() {
                    l.connect_pressed(move |g, x, y| fire(g.upcast_ref(), x, y));
                }
                expander.add_controller(gesture);
            }
            expander.add_controller(drop_target(item, on_drop.clone()));
            expander.add_controller(file_drop_target(item, on_files.clone()));
            item.set_child(Some(&expander));
        });
        let on_sync = Rc::new(on_sync);
        factory.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
            let Some(row) = item.item().and_downcast::<gtk::TreeListRow>() else { return };
            let Some(obj) = row.item().and_downcast::<FolderObject>() else { return };
            let expander = item.child().and_downcast::<gtk::TreeExpander>().expect("expander");
            expander.set_list_row(Some(&row));
            expander.set_child(Some(&row_widget(&obj, on_sync.clone())));
            item.set_selectable(!obj.is_account());
            item.set_activatable(true);
        });
        let view = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .css_classes(["navigation-sidebar"])
            .build();
        view.connect_activate(move |view, pos| {
            let Some(row) = view.model().and_then(|m| m.item(pos)).and_downcast::<gtk::TreeListRow>() else {
                return;
            };
            match row.item().and_downcast::<FolderObject>() {
                Some(o) if o.is_account() => row.set_expanded(!row.is_expanded()),
                Some(_) => on_pick(),
                None => {}
            }
        });
        let on_folder = Rc::new(on_folder);
        selection.connect_selected_item_notify(move |sel| {
            if let Some(obj) = sel
                .selected_item()
                .and_downcast::<gtk::TreeListRow>()
                .and_then(|r| r.item().and_downcast::<FolderObject>())
                .filter(|o| !o.is_account())
            {
                on_folder(&obj);
            }
        });
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&view)
            .build();
        Rc::new(Self { widget, accounts, tree, selection })
    }

    /// Selects a folder row without the user's involvement.
    pub fn select(&self, obj: &FolderObject) {
        for i in 0..self.tree.n_items() {
            if self
                .tree
                .item(i)
                .and_downcast::<gtk::TreeListRow>()
                .and_then(|r| r.item())
                .is_some_and(|o| &o == obj.upcast_ref::<glib::Object>())
            {
                self.selection.set_selected(i);
                return;
            }
        }
    }

    pub fn selected(&self) -> Option<FolderObject> {
        self.selection
            .selected_item()
            .and_downcast::<gtk::TreeListRow>()
            .and_then(|r| r.item().and_downcast())
    }
}

fn row_of(item: &gtk::ListItem) -> Option<(gtk::TreeListRow, FolderObject)> {
    let row = item.item().and_downcast::<gtk::TreeListRow>()?;
    let obj = row.item().and_downcast::<FolderObject>()?;
    Some((row, obj))
}

fn dragged(target: &gtk::DropTarget) -> Option<DraggedMessages> {
    target.value()?.get::<DraggedMessages>().ok()
}

/// Whether `m` can be moved to the folder `obj`: any folder but the one
/// the messages are in, in any account.
fn accepts(obj: &FolderObject, m: &DraggedMessages) -> bool {
    !obj.is_account()
        && !(obj.account_uid() == m.account_uid && m.folder.full_name().as_deref() == Some(obj.full_name().as_str()))
}

/// Takes message drags; a collapsed row expands after the drag rests on
/// it.
fn drop_target(item: &gtk::ListItem, on_drop: Rc<dyn Fn(&FolderObject, DraggedMessages)>) -> gtk::DropTarget {
    let target = gtk::DropTarget::new(DraggedMessages::static_type(), gdk::DragAction::MOVE);
    // The value is needed before the drop to refuse the messages' own folder.
    target.set_preload(true);
    let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let stop = {
        let timer = timer.clone();
        move || {
            if let Some(id) = timer.borrow_mut().take() {
                id.remove();
            }
        }
    };
    let action = {
        let item = item.downgrade();
        move |t: &gtk::DropTarget| {
            let ok = item
                .upgrade()
                .and_then(|i| row_of(&i))
                .zip(dragged(t))
                .is_some_and(|((_, obj), m)| accepts(&obj, &m));
            if ok { gdk::DragAction::MOVE } else { gdk::DragAction::empty() }
        }
    };
    let (item_weak, t2, a) = (item.downgrade(), timer.clone(), action.clone());
    target.connect_enter(move |t, _, _| {
        if let Some(id) = t2.borrow_mut().take() {
            id.remove();
        }
        let (item, tw, timer) = (item_weak.clone(), t.downgrade(), t2.clone());
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(EXPAND_DELAY_MS), move || {
            timer.borrow_mut().take();
            let (Some(item), Some(t)) = (item.upgrade(), tw.upgrade()) else { return };
            let (Some((row, _)), Some(_)) = (row_of(&item), dragged(&t)) else { return };
            if row.is_expandable() && !row.is_expanded() {
                row.set_expanded(true);
            }
        });
        *t2.borrow_mut() = Some(id);
        a(t)
    });
    target.connect_motion(move |t, _, _| action(t));
    let s = stop.clone();
    target.connect_leave(move |_| s());
    let item = item.downgrade();
    target.connect_drop(move |_, value, _, _| {
        stop();
        let (Some(item), Ok(m)) = (item.upgrade(), value.get::<DraggedMessages>()) else { return false };
        let Some((_, obj)) = row_of(&item) else { return false };
        if !accepts(&obj, &m) {
            return false;
        }
        on_drop(&obj, m);
        true
    });
    target
}

/// Takes files from outside, to import into the row's folder.
fn file_drop_target(item: &gtk::ListItem, on_files: Rc<dyn Fn(&FolderObject, Vec<gio::File>)>) -> gtk::DropTarget {
    let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    let weak = item.downgrade();
    target.connect_enter(move |_, _, _| {
        let folder = weak.upgrade().and_then(|i| row_of(&i)).is_some_and(|(_, obj)| !obj.is_account());
        if folder { gdk::DragAction::COPY } else { gdk::DragAction::empty() }
    });
    let item = item.downgrade();
    target.connect_drop(move |_, value, _, _| {
        let (Some(item), Ok(list)) = (item.upgrade(), value.get::<gdk::FileList>()) else { return false };
        let Some((_, obj)) = row_of(&item).filter(|(_, o)| !o.is_account()) else { return false };
        on_files(&obj, list.files());
        true
    });
    target
}

fn is_folder_row(item: &gtk::ListItem) -> bool {
    item.item()
        .and_downcast::<gtk::TreeListRow>()
        .and_then(|r| r.item().and_downcast::<FolderObject>())
        .is_some_and(|o| !o.is_account())
}

type SyncHandler = Rc<dyn Fn(&FolderObject, &gtk::Button)>;

fn row_widget(obj: &FolderObject, on_sync: SyncHandler) -> gtk::Widget {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    if !obj.is_account() {
        let icon = gtk::Image::new();
        obj.bind_property("icon-name", &icon, "icon-name").sync_create().build();
        b.append(&icon);
    }
    let title = gtk::Label::new(None);
    obj.bind_property("title", &title, "label").sync_create().build();
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    b.append(&title);
    if obj.is_account() {
        title.add_css_class("account-name");
        b.append(&sync_button(obj, on_sync));
    } else {
        let count = gtk::Label::new(None);
        count.add_css_class("folder-count");
        count.add_css_class("dimmed");
        obj.bind_property("unread", &count, "label")
            .transform_to(|_, n: i32| Some(if n > 0 { n.to_string() } else { String::new() }))
            .sync_create()
            .build();
        b.append(&count);
    }
    b.upcast()
}

fn sync_button(obj: &FolderObject, on_sync: SyncHandler) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.set_valign(gtk::Align::Center);
    let stack = gtk::Stack::new();
    stack.add_named(&gtk::Image::from_icon_name("view-refresh-symbolic"), Some("idle"));
    stack.add_named(&adw::Spinner::new(), Some("syncing"));
    stack.add_named(&gtk::Image::from_icon_name("network-offline-symbolic"), Some("offline"));
    stack.add_named(&gtk::Image::from_icon_name("dialog-warning-symbolic"), Some("error"));
    button.set_child(Some(&stack));
    let update = {
        let button = button.downgrade();
        let stack = stack.downgrade();
        move |obj: &FolderObject| {
            let (Some(button), Some(stack)) = (button.upgrade(), stack.upgrade()) else { return };
            let title = obj.title();
            let (page, tip) = match obj.sync_state() {
                SyncState::Idle => ("idle", format!("Sync {title} now")),
                SyncState::Syncing => ("syncing", format!("Syncing {title}; click to cancel")),
                SyncState::Offline => ("offline", format!("{title} is offline; click to retry")),
                SyncState::Error => ("error", obj.sync_message()),
            };
            stack.set_visible_child_name(page);
            button.set_tooltip_text(Some(&tip));
            if obj.sync_state() == SyncState::Error {
                button.add_css_class("warning");
            } else {
                button.remove_css_class("warning");
            }
        }
    };
    update(obj);
    let handler = std::cell::Cell::new(Some(obj.connect_sync_state_notify(update)));
    let weak = obj.downgrade();
    button.connect_destroy(move |_| {
        if let (Some(o), Some(h)) = (weak.upgrade(), handler.take()) {
            o.disconnect(h);
        }
    });
    let weak = obj.downgrade();
    button.connect_clicked(move |b| {
        if let Some(o) = weak.upgrade() {
            on_sync(&o, b);
        }
    });
    button
}
