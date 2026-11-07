use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use salutations_core::camel::{self, prelude::*};
use salutations_core::edataserver::SourceRegistry;
use salutations_core::offline::{self, Download, OfflineSync, Refresh, TimeUnit, Window as AgeWindow};
use salutations_core::{Account, Settings};

/// What the Accounts page needs about one account.
pub struct AccountEntry {
    pub account: Account,
    /// None until the account has been opened.
    pub store: Option<camel::Store>,
    /// Folder full names with their displayed paths, in tree order.
    pub folders: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountChange {
    /// What is kept offline, or how far back.
    Offline,
    /// The check interval.
    Refresh,
    /// The accounts' order, saved in Settings.
    Order,
    /// Server push on or off; applies on reconnecting.
    Push,
    /// Metered, Power Saver or Never download settings, kept by Salutations.
    Sync,
}

/// Called with an account UID after one of its settings changes.
pub type AccountChanged = Rc<dyn Fn(&str, AccountChange)>;

thread_local! {
    static LISTENERS: RefCell<Vec<Box<dyn Fn()>>> = const { RefCell::new(Vec::new()) };
}

/// `f` runs after every saved change made in the dialog.
pub fn connect_changed(f: impl Fn() + 'static) {
    LISTENERS.with(|l| l.borrow_mut().push(Box::new(f)));
}

/// How long a shown message waits before it is marked read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadDelay {
    Seconds(u32),
    /// When a reply or forward is sent.
    OnResponse,
    Never,
}

impl ReadDelay {
    const PRESETS: [ReadDelay; 7] = [
        ReadDelay::Seconds(0),
        ReadDelay::Seconds(2),
        ReadDelay::Seconds(5),
        ReadDelay::Seconds(10),
        ReadDelay::Seconds(30),
        ReadDelay::OnResponse,
        ReadDelay::Never,
    ];

    fn label(self) -> String {
        match self {
            ReadDelay::Never => "Never".into(),
            ReadDelay::OnResponse => "On reply or forward".into(),
            ReadDelay::Seconds(0) => "Immediately".into(),
            ReadDelay::Seconds(1) => "After 1 second".into(),
            ReadDelay::Seconds(n) => format!("After {n} seconds"),
        }
    }
}

/// How many new messages per account get notifications of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotifyLimit {
    Count(u32),
    All,
}

impl NotifyLimit {
    const PRESETS: [NotifyLimit; 5] =
        [NotifyLimit::Count(0), NotifyLimit::Count(4), NotifyLimit::Count(8), NotifyLimit::Count(12), NotifyLimit::All];

    fn label(self) -> String {
        match self {
            NotifyLimit::Count(0) => "None".into(),
            NotifyLimit::Count(n) => n.to_string(),
            NotifyLimit::All => "All".into(),
        }
    }
}

/// A row of presets whose last entry is the "off" choice (Never, All
/// Time), with "Custom" inserted before it, or at `custom_at`. A spin row
/// under it, shown only while Custom is selected, holds the custom number.
/// A current value that is not a preset opens as Custom with its number, so
/// the dialog never shows a value other than the one in use.
struct PresetRow<T> {
    combo: adw::ComboRow,
    spin: adw::SpinRow,
    presets: RefCell<Vec<T>>,
    /// Where "Custom" is in the combo's list; None without Custom.
    custom_at: std::cell::Cell<Option<usize>>,
    label: Box<dyn Fn(T) -> String>,
    /// Set while `reset` rebuilds the list, which is not a user's choice.
    resetting: std::cell::Cell<bool>,
}

impl<T: Copy + PartialEq + 'static> PresetRow<T> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        title: &str,
        subtitle: Option<&str>,
        presets: &[T],
        label: impl Fn(T) -> String + 'static,
        current: T,
        custom_title: &str,
        custom_value: Option<f64>,
        range: (f64, f64),
    ) -> Self {
        Self::with_custom_at(title, subtitle, presets, label, current, custom_title, custom_value, range, presets.len() - 1)
    }

    /// Like `new`, with "Custom" at index `custom_at` of the list.
    #[allow(clippy::too_many_arguments)]
    fn with_custom_at(
        title: &str,
        subtitle: Option<&str>,
        presets: &[T],
        label: impl Fn(T) -> String + 'static,
        current: T,
        custom_title: &str,
        custom_value: Option<f64>,
        range: (f64, f64),
        custom_at: usize,
    ) -> Self {
        let combo = adw::ComboRow::builder().title(title).build();
        if let Some(s) = subtitle {
            combo.set_subtitle(s);
        }
        let spin = adw::SpinRow::builder()
            .title(custom_title)
            .adjustment(&gtk::Adjustment::new(range.0, range.0, range.1, 1.0, 10.0, 0.0))
            .build();
        // Styled as a child of the row above, like an expander row's.
        spin.add_css_class("preset-custom");
        let row = PresetRow {
            combo,
            spin,
            presets: RefCell::new(Vec::new()),
            custom_at: Default::default(),
            label: Box::new(label),
            resetting: Default::default(),
        };
        row.fill(presets, Some(custom_at), current, custom_value);
        row
    }

    /// Lists `presets` with "Custom" at `custom_at`, showing `current`.
    fn fill(&self, presets: &[T], custom_at: Option<usize>, current: T, custom_value: Option<f64>) {
        let mut labels: Vec<String> = presets.iter().map(|p| (self.label)(*p)).collect();
        let custom_at = custom_at.map(|c| c.min(labels.len()));
        if let Some(c) = custom_at {
            labels.insert(c, "Custom".into());
        }
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let selected = match (presets.iter().position(|p| *p == current), custom_at) {
            (Some(i), Some(c)) if i >= c => i + 1,
            (Some(i), _) => i,
            (None, Some(c)) => c,
            (None, None) => 0,
        };
        *self.presets.borrow_mut() = presets.to_vec();
        self.custom_at.set(custom_at);
        self.combo.set_model(Some(&gtk::StringList::new(&labels)));
        self.combo.set_selected(selected as u32);
        if let Some(v) = custom_value {
            let adj = self.spin.adjustment();
            self.spin.set_value(v.clamp(adj.lower(), adj.upper()));
        }
        self.spin.set_visible(custom_at == Some(selected));
    }

    /// Replaces the choices, without reporting it as a change. `max` is
    /// the largest custom number.
    fn reset(&self, presets: &[T], custom_at: Option<usize>, current: T, custom_value: Option<f64>, max: f64) {
        self.resetting.set(true);
        self.spin.adjustment().set_upper(max);
        self.fill(presets, custom_at, current, custom_value);
        self.resetting.set(false);
    }

    fn is_custom(&self) -> bool {
        self.custom_at.get() == Some(self.combo.selected() as usize)
    }

    /// The chosen value, with `from_number` making a custom one.
    fn value(&self, from_number: &impl Fn(f64) -> T) -> T {
        let i = self.combo.selected() as usize;
        let presets = self.presets.borrow();
        match self.custom_at.get() {
            Some(c) if i == c => from_number(self.spin.value()),
            Some(c) if i > c => presets[i - 1],
            _ => presets[i.min(presets.len() - 1)],
        }
    }

    /// Styled as a child of the row above, its custom row a level deeper.
    fn nested(&self) -> &Self {
        self.combo.add_css_class("preset-custom");
        self.spin.add_css_class("preset-nested");
        self
    }

    fn add_to(&self, group: &adw::PreferencesGroup) {
        group.add(&self.combo);
        group.add(&self.spin);
    }

    /// `on_change` gets each new value; connected after setup, so opening
    /// the dialog saves nothing.
    fn connect(self: Rc<Self>, from_number: impl Fn(f64) -> T + 'static, on_change: impl Fn(T) + 'static) {
        let from_number = Rc::new(from_number);
        let on_change = Rc::new(on_change);
        let (this, f, cb) = (self.clone(), from_number.clone(), on_change.clone());
        self.combo.connect_selected_notify(move |_| {
            if this.resetting.get() {
                return;
            }
            this.spin.set_visible(this.is_custom());
            cb(this.value(&*f));
        });
        let this = self.clone();
        self.spin.connect_value_notify(move |_| {
            if !this.resetting.get() && this.is_custom() {
                on_change(this.value(&*from_number));
            }
        });
    }
}

/// The dialog's height from the main window's: all of it up to 685 px,
/// 685 px up to 720 px, and 95% above that. Fixed, so it never changes
/// between pages or as rows appear.
fn dialog_height(window_height: i32) -> i32 {
    match window_height {
        ..=685 => window_height,
        686..=720 => 685,
        h => (h as f64 * 0.95).round() as i32,
    }
}

pub fn show(
    parent: &impl IsA<gtk::Widget>,
    registry: &SourceRegistry,
    accounts: Vec<AccountEntry>,
    on_account: AccountChanged,
    open_account: Option<&str>,
) {
    let settings = Settings::get();
    let current = match (settings.mark_read, settings.read_on_response) {
        (false, _) => ReadDelay::Never,
        (true, true) => ReadDelay::OnResponse,
        (true, false) => ReadDelay::Seconds(settings.mark_read_delay),
    };
    // Custom goes after the delays, before On reply or forward and Never.
    let read = Rc::new(PresetRow::with_custom_at(
        "Mark messages as read",
        None,
        &ReadDelay::PRESETS,
        ReadDelay::label,
        current,
        "Seconds",
        Some(settings.mark_read_delay as f64),
        (0.0, 86_400.0),
        ReadDelay::PRESETS.len() - 2,
    ));
    let read_delete = adw::SwitchRow::builder()
        .title("Mark as read when deleting")
        .active(settings.read_on_delete)
        .build();
    let read_junk = adw::SwitchRow::builder()
        .title("Mark as read when moving to Junk")
        .active(settings.read_on_junk)
        .build();
    let strip = adw::SwitchRow::builder()
        .title("Remove some basic tracking from remote content")
        .subtitle("Removes known tracking codes and your address from the addresses of remote content. Some content may not load.")
        .active(settings.remote_strip)
        .build();
    let mono = adw::SwitchRow::builder()
        .title("Use monospace font for plain-text messages")
        .active(settings.plain_monospace)
        .build();
    let confirm = adw::SwitchRow::builder()
        .title("Confirm before opening links")
        .subtitle("Links misrepresenting their destinations will always require confirmation")
        .active(settings.confirm_links)
        .build();
    let position = adw::ComboRow::builder()
        .title("Message position")
        .subtitle("In wide windows; narrow windows show one at a time")
        .model(&gtk::StringList::new(&["Beside the list", "Below the list"]))
        .selected(settings.message_below as u32)
        .build();
    let group = adw::PreferencesGroup::builder().title("Reading").build();
    group.add(&position);
    read.add_to(&group);
    group.add(&read_delete);
    group.add(&read_junk);
    group.add(&mono);
    group.add(&confirm);
    group.add(&strip);
    group.add(&remote_allowed_row());
    let background = adw::SwitchRow::builder()
        .title("Run in background")
        .subtitle("Keep checking for new mail after the window is closed")
        .active(settings.run_in_background)
        .build();
    let autostart = adw::SwitchRow::builder()
        .title("Start at login")
        .subtitle("Start in the background when you log in")
        .active(settings.autostart && settings.run_in_background)
        .sensitive(settings.run_in_background)
        .build();
    let limit_now = settings.notification_limit.map_or(NotifyLimit::All, NotifyLimit::Count);
    let limit = Rc::new(PresetRow::with_custom_at(
        "Concurrent notifications",
        Some("New messages beyond this are combined into one notification"),
        &NotifyLimit::PRESETS,
        NotifyLimit::label,
        limit_now,
        "Maximum number",
        settings.notification_limit.map(|n| n.max(1) as f64),
        (1.0, 999.0),
        NotifyLimit::PRESETS.len(),
    ));
    let notify_group = adw::PreferencesGroup::builder().title("Notifications").build();
    limit.add_to(&notify_group);
    let import = adw::ComboRow::builder()
        .title("Import keys from messages")
        .subtitle("Only from messages verified as coming from their sender")
        .model(&gtk::StringList::new(&["Always", "Prompt", "Never"]))
        .selected(match settings.import_keys {
            salutations_core::ImportKeys::Always => 0,
            salutations_core::ImportKeys::Prompt => 1,
            salutations_core::ImportKeys::Never => 2,
        })
        .build();
    let keyserver = adw::EntryRow::builder()
        .title("Keyserver")
        .text(&settings.keyserver)
        .show_apply_button(true)
        .build();
    let crypto_group = adw::PreferencesGroup::builder().title("Encryption").build();
    crypto_group.add(&import);
    crypto_group.add(&keyserver);
    let bimi = adw::SwitchRow::builder()
        .title("Brand logos (BIMI)")
        .subtitle("For senders that pass verification; the logo comes from the sender's own server")
        .active(settings.avatar_bimi)
        .build();
    let libravatar = adw::SwitchRow::builder()
        .title("Libravatar")
        .subtitle("Sends a hash of each sender's address to Libravatar or the sender's own avatar server")
        .active(settings.avatar_libravatar)
        .build();
    let gravatar = adw::SwitchRow::builder()
        .title("Gravatar")
        .subtitle("Sends a hash of each sender's address to Gravatar")
        .active(settings.avatar_gravatar)
        .build();
    let avatar_group = adw::PreferencesGroup::builder()
        .title("Online avatars")
        .description("Used only for senders without a photo in the address books, an OpenPGP photo or a picture in their mail, and never in Junk")
        .build();
    avatar_group.add(&bimi);
    avatar_group.add(&libravatar);
    avatar_group.add(&gravatar);
    let background_group = adw::PreferencesGroup::builder().title("System").build();
    background_group.add(&background);
    background_group.add(&autostart);
    let page = adw::PreferencesPage::builder().title("General").icon_name("emblem-system-symbolic").build();
    page.add(&group);
    page.add(&avatar_group);
    page.add(&notify_group);
    page.add(&crypto_group);
    page.add(&background_group);
    let dialog = adw::PreferencesDialog::new();
    dialog.add(&page);
    // The account's row, to open its settings at once.
    let mut open_row = None;
    if !accounts.is_empty() {
        let (page, rows) = accounts_page(&dialog, registry, accounts, on_account);
        dialog.add(&page);
        if let Some(uid) = open_account {
            dialog.set_visible_page(&page);
            open_row = rows.borrow().iter().find(|(u, ..)| u == uid).map(|(_, row, ..)| row.clone());
        }
    }
    dialog.set_content_height(dialog_height(parent.as_ref().height()));

    let save = Rc::new(move |f: &dyn Fn(&mut Settings)| {
        let mut s = Settings::get();
        f(&mut s);
        if let Err(e) = s.save() {
            glib::g_warning!("salutations", "Could not save settings: {}", e);
        }
        LISTENERS.with(|l| l.borrow().iter().for_each(|f| f()));
    });
    let s = save.clone();
    strip.connect_active_notify(move |row| {
        let on = row.is_active();
        s(&|st: &mut Settings| st.remote_strip = on);
    });
    for (row, junk) in [(&read_delete, false), (&read_junk, true)] {
        let s = save.clone();
        row.connect_active_notify(move |row| {
            let on = row.is_active();
            s(&|st: &mut Settings| if junk { st.read_on_junk = on } else { st.read_on_delete = on });
        });
    }
    for (row, which) in [(&bimi, 0), (&libravatar, 1), (&gravatar, 2)] {
        let s = save.clone();
        row.connect_active_notify(move |row| {
            let on = row.is_active();
            s(&|st: &mut Settings| match which {
                0 => st.avatar_bimi = on,
                1 => st.avatar_libravatar = on,
                _ => st.avatar_gravatar = on,
            });
        });
    }
    let s = save.clone();
    position.connect_selected_notify(move |row| {
        let below = row.selected() == 1;
        s(&|st: &mut Settings| st.message_below = below);
    });
    let s = save.clone();
    read.connect(
        |n| ReadDelay::Seconds(n as u32),
        move |d| {
            s(&|st: &mut Settings| match d {
                ReadDelay::Never => st.mark_read = false,
                ReadDelay::OnResponse => {
                    st.mark_read = true;
                    st.read_on_response = true;
                }
                ReadDelay::Seconds(n) => {
                    st.mark_read = true;
                    st.read_on_response = false;
                    st.mark_read_delay = n;
                }
            })
        },
    );
    let s = save.clone();
    import.connect_selected_notify(move |row| {
        let v = match row.selected() {
            0 => salutations_core::ImportKeys::Always,
            1 => salutations_core::ImportKeys::Prompt,
            _ => salutations_core::ImportKeys::Never,
        };
        s(&|st: &mut Settings| st.import_keys = v);
    });
    let s = save.clone();
    keyserver.connect_apply(move |row| {
        let text = row.text().trim().to_string();
        let v = if text.is_empty() { salutations_core::DEFAULT_KEYSERVER.to_string() } else { text };
        row.set_text(&v);
        s(&|st: &mut Settings| st.keyserver = v.clone());
    });
    let s = save.clone();
    limit.connect(
        |n| NotifyLimit::Count(n as u32),
        move |l| {
            s(&|st: &mut Settings| {
                st.notification_limit = match l {
                    NotifyLimit::Count(n) => Some(n),
                    NotifyLimit::All => None,
                }
            })
        },
    );
    let s = save.clone();
    let a = autostart.clone();
    background.connect_active_notify(move |row| {
        let on = row.is_active();
        a.set_sensitive(on);
        if !on {
            a.set_active(false);
        }
        s(&|st: &mut Settings| st.run_in_background = on);
        crate::background::apply();
        crate::background::apply_autostart();
    });
    let s = save.clone();
    autostart.connect_active_notify(move |row| {
        let on = row.is_active();
        s(&|st: &mut Settings| st.autostart = on);
        crate::background::apply_autostart();
    });
    let s = save.clone();
    mono.connect_active_notify(move |row| {
        let on = row.is_active();
        s(&|st: &mut Settings| st.plain_monospace = on);
    });
    confirm.connect_active_notify(move |row| {
        let on = row.is_active();
        save(&|st: &mut Settings| st.confirm_links = on);
    });
    dialog.present(Some(parent));
    if let Some(row) = open_row {
        row.emit_activate();
    }
}

fn window_days(w: AgeWindow) -> Option<f64> {
    match w {
        AgeWindow::Last(TimeUnit::Days, n) => Some(n as f64),
        AgeWindow::Last(TimeUnit::Weeks, n) => Some((n * 7) as f64),
        _ => None,
    }
}

/// The Accounts page. With one account its settings are on the page;
/// with more, each account is a row that opens a page of its own, and the
/// rows' arrows set the order.
fn accounts_page(
    dialog: &adw::PreferencesDialog,
    registry: &SourceRegistry,
    accounts: Vec<AccountEntry>,
    on_account: AccountChanged,
) -> (adw::PreferencesPage, Rc<RefCell<Vec<AccountRow>>>) {
    let page = adw::PreferencesPage::builder()
        .title("Accounts")
        .icon_name("system-users-symbolic")
        .build();
    let notice = adw::PreferencesGroup::builder()
        .description("Availability of mail accounts is managed in Online Accounts")
        .build();
    notice.add(
        &adw::ButtonRow::builder()
            .title("Open _Online Accounts")
            .use_underline(true)
            .end_icon_name("adw-external-link-symbolic")
            .action_name("app.online-accounts")
            .build(),
    );
    page.add(&notice);
    if accounts.len() == 1 {
        let entry = accounts.into_iter().next().expect("one account");
        let title = entry.account.display_name();
        let account = entry.account.clone();
        page.add(&receiving_group(registry, entry, &title, on_account));
        for g in sending_groups(&account) {
            page.add(&g);
        }
        return (page, Rc::default());
    }
    // Where mailto: links and other messages started outside a folder
    // come from.
    let names: Vec<String> = accounts.iter().map(|e| e.account.display_name()).collect();
    let uids: Vec<String> = accounts.iter().map(|e| e.account.uid.clone()).collect();
    let current = Settings::get().default_account;
    let default = adw::ComboRow::builder()
        .title("Default account")
        .subtitle("Used for email links and new messages started outside Salutations")
        .model(&gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(current.and_then(|c| uids.iter().position(|u| *u == c)).unwrap_or(0) as u32)
        .build();
    default.connect_selected_notify(move |row| {
        let mut st = Settings::get();
        st.default_account = uids.get(row.selected() as usize).cloned();
        if let Err(e) = st.save() {
            glib::g_warning!("salutations", "Could not save settings: {}", e);
        }
    });
    let defaults = adw::PreferencesGroup::new();
    defaults.add(&default);
    page.add(&defaults);
    let list = adw::PreferencesGroup::new();
    page.add(&list);
    // Each account's row in display order, for the up and down buttons.
    let rows: Rc<RefCell<Vec<AccountRow>>> = Rc::default();
    let reorder = {
        let (rows, list, on_account) = (rows.clone(), list.clone(), on_account.clone());
        Rc::new(move |uid: &str, delta: isize| {
            let mut r = rows.borrow_mut();
            let Some(i) = r.iter().position(|(u, ..)| u == uid) else { return };
            let j = i as isize + delta;
            if j < 0 || j as usize >= r.len() {
                return;
            }
            r.swap(i, j as usize);
            // A group only appends, so the rows are added again in order.
            for (_, row, ..) in r.iter() {
                list.remove(row);
            }
            for (k, (_, row, up, down)) in r.iter().enumerate() {
                list.add(row);
                up.set_sensitive(k > 0);
                down.set_sensitive(k + 1 < r.len());
            }
            let mut st = Settings::get();
            st.account_order = r.iter().map(|(u, ..)| u.clone()).collect();
            if let Err(e) = st.save() {
                glib::g_warning!("salutations", "Could not save settings: {}", e);
            }
            drop(r);
            on_account(uid, AccountChange::Order);
        })
    };
    let count = accounts.len();
    for (index, entry) in accounts.into_iter().enumerate() {
        let name = entry.account.display_name();
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&name))
            .subtitle(glib::markup_escape_text(entry.account.address.as_deref().unwrap_or("")))
            .activatable(true)
            .build();
        let up = gtk::Button::builder()
            .icon_name("go-up-symbolic")
            .tooltip_text("Move up")
            .sensitive(index > 0)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let down = gtk::Button::builder()
            .icon_name("go-down-symbolic")
            .tooltip_text("Move down")
            .sensitive(index + 1 < count)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        for (button, delta) in [(&up, -1), (&down, 1)] {
            let (reorder, uid) = (reorder.clone(), entry.account.uid.clone());
            button.connect_clicked(move |_| reorder(&uid, delta));
        }
        row.add_suffix(&up);
        row.add_suffix(&down);
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        list.add(&row);
        rows.borrow_mut().push((entry.account.uid.clone(), row.clone(), up, down));
        // The subpage is built when first opened and kept.
        let subpage: RefCell<Option<adw::NavigationPage>> = RefCell::new(None);
        let entry = RefCell::new(Some(entry));
        let (dialog, registry, on_account) = (dialog.downgrade(), registry.clone(), on_account.clone());
        row.connect_activated(move |_| {
            let Some(dialog) = dialog.upgrade() else { return };
            let page = subpage
                .borrow_mut()
                .get_or_insert_with(|| {
                    let entry = entry.borrow_mut().take().expect("built once");
                    account_subpage(&registry, entry, on_account.clone())
                })
                .clone();
            dialog.push_subpage(&page);
        });
    }
    (page, rows)
}

/// An account UID, its row, and the row's up and down buttons.
type AccountRow = (String, adw::ActionRow, gtk::Button, gtk::Button);

fn account_subpage(registry: &SourceRegistry, entry: AccountEntry, on_account: AccountChanged) -> adw::NavigationPage {
    let name = entry.account.display_name();
    let account = entry.account.clone();
    let page = adw::PreferencesPage::new();
    page.add(&receiving_group(registry, entry, "Receiving", on_account));
    for g in sending_groups(&account) {
        page.add(&g);
    }
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&page));
    adw::NavigationPage::builder().title(&name).child(&view).build()
}

fn save_compose(uid: &str, f: impl FnOnce(&mut salutations_core::ComposePrefs)) {
    let mut st = Settings::get();
    let mut prefs = st.compose_for(uid);
    f(&mut prefs);
    st.compose.insert(uid.to_string(), prefs);
    if let Err(e) = st.save() {
        glib::g_warning!("salutations", "Could not save settings: {}", e);
    }
}

/// Default format, From aliases, and the signature.
fn sending_groups(account: &Account) -> Vec<adw::PreferencesGroup> {
    use salutations_core::compose::{self, Format};
    let prefs = Settings::get().compose_for(&account.uid);
    let uid = account.uid.clone();

    let sending = adw::PreferencesGroup::builder().title("Sending").build();
    let format = adw::ComboRow::builder()
        .title("Default format")
        .model(&gtk::StringList::new(&["Markdown", "Plain text"]))
        .selected(if prefs.format == Format::Plain { 1 } else { 0 })
        .build();
    sending.add(&format);
    {
        let uid = uid.clone();
        format.connect_selected_notify(move |row| {
            let f = if row.selected() == 1 { Format::Plain } else { Format::Markdown };
            save_compose(&uid, |p| p.format = f);
        });
    }
    let aliases = adw::ExpanderRow::builder()
        .title("Other from addresses")
        .build();
    sending.add(&aliases);
    let add = adw::EntryRow::builder().title("Add an address").show_apply_button(true).build();
    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::default();
    let refill: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::default();
    let fill = {
        let (account, aliases, add, rows, refill) = (account.clone(), aliases.clone(), add.clone(), rows.clone(), refill.clone());
        Rc::new(move || {
            for r in rows.borrow_mut().drain(..) {
                aliases.remove(&r);
            }
            aliases.remove(&add);
            let list = account.aliases();
            for (i, a) in list.iter().enumerate() {
                let row = adw::ActionRow::builder().title(glib::markup_escape_text(&compose::format_address(a))).build();
                let remove = gtk::Button::builder()
                    .icon_name("edit-delete-symbolic")
                    .tooltip_text("Remove")
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                let (account, refill) = (account.clone(), refill.clone());
                remove.connect_clicked(move |_| {
                    let mut list = account.aliases();
                    if i < list.len() {
                        list.remove(i);
                    }
                    account.set_aliases(&list);
                    let f = refill.borrow().clone();
                    if let Some(f) = f {
                        glib::idle_add_local_once(move || f());
                    }
                });
                row.add_suffix(&remove);
                aliases.add_row(&row);
                rows.borrow_mut().push(row);
            }
            aliases.add_row(&add);
        })
    };
    *refill.borrow_mut() = Some(fill.clone());
    fill();
    {
        let (account, fill) = (account.clone(), fill.clone());
        add.connect_apply(move |row| {
            let text = row.text().to_string();
            match compose::parse_addresses(&text) {
                Ok(new) if !new.is_empty() => {
                    let mut list = account.aliases();
                    for a in new {
                        if !list.iter().any(|x| x.email.eq_ignore_ascii_case(&a.email)) {
                            list.push(a);
                        }
                    }
                    account.set_aliases(&list);
                    row.set_text("");
                    row.remove_css_class("error");
                    fill();
                }
                _ => row.add_css_class("error"),
            }
        });
    }

    let signature = adw::PreferencesGroup::builder().title("Signature").build();
    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&prefs.signature);
    let text = gtk::TextView::builder()
        .buffer(&buffer)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(10)
        .bottom_margin(10)
        .left_margin(12)
        .right_margin(12)
        .height_request(96)
        .accepts_tab(false)
        .css_classes(["card", "signature-text"])
        .build();
    signature.add(&text);
    {
        let uid = uid.clone();
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
        buffer.connect_changed(move |b| {
            if let Some(id) = pending.borrow_mut().take() {
                id.remove();
            }
            let (uid, b, p) = (uid.clone(), b.clone(), pending.clone());
            *pending.borrow_mut() = Some(glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                p.borrow_mut().take();
                let (s, e) = b.bounds();
                let text = b.text(&s, &e, false).to_string();
                save_compose(&uid, |p| p.signature = text);
            }));
        });
    }
    let options = adw::PreferencesGroup::new();
    let include = adw::SwitchRow::builder()
        .title("Include signature by default")
        .active(prefs.include_signature)
        .build();
    options.add(&include);
    {
        let uid = uid.clone();
        include.connect_active_notify(move |row| {
            let on = row.is_active();
            save_compose(&uid, |p| p.include_signature = on);
        });
    }
    let position = adw::ComboRow::builder()
        .title("Signature position")
        .model(&gtk::StringList::new(&["Below quotes", "Above quotes"]))
        .selected(if prefs.signature_above { 1 } else { 0 })
        .build();
    options.add(&position);
    position.connect_selected_notify(move |row| {
        let above = row.selected() == 1;
        save_compose(&uid, |p| p.signature_above = above);
    });
    vec![sending, signature, options, encryption_group(account)]
}

/// The account's OpenPGP key, when it encrypts automatically, and how its
/// public key is shared.
fn encryption_group(account: &Account) -> adw::PreferencesGroup {
    use salutations_core::AttachKey;
    let prefs = Settings::get().compose_for(&account.uid);
    let uid = account.uid.clone();
    let group = adw::PreferencesGroup::builder().title("Encryption").build();
    let key = adw::ComboRow::builder()
        .title("Signing key")
        .model(&gtk::StringList::new(&["None"]))
        .build();
    group.add(&key);
    let encrypt_new = adw::SwitchRow::builder()
        .title("Encrypt new messages automatically")
        .active(prefs.encrypt_new)
        .build();
    group.add(&encrypt_new);
    let encrypt_replies = adw::SwitchRow::builder()
        .title("Encrypt replies to encrypted messages")
        .active(prefs.encrypt_replies)
        // Replies are encrypted anyway while new messages are.
        .visible(!prefs.encrypt_new)
        .build();
    group.add(&encrypt_replies);
    let autocrypt = adw::SwitchRow::builder()
        .title("Include public key in messages")
        .subtitle("via Autocrypt")
        .active(prefs.autocrypt)
        .build();
    group.add(&autocrypt);
    let attach = adw::ComboRow::builder()
        .title("Also include as an attachment")
        .subtitle("Supports older clients")
        .model(&gtk::StringList::new(&["Always", "When signed", "Never"]))
        .selected(match prefs.attach_key {
            AttachKey::Always => 0,
            AttachKey::WhenProtected => 1,
            AttachKey::Never => 2,
        })
        .visible(prefs.autocrypt)
        .build();
    // Styled as a child of the switch above.
    attach.add_css_class("preset-custom");
    group.add(&attach);
    let dependents: Vec<gtk::Widget> =
        vec![encrypt_new.clone().upcast(), encrypt_replies.clone().upcast(), autocrypt.clone().upcast(), attach.clone().upcast()];
    let has_key = prefs.signing_key.is_some();
    for d in &dependents {
        d.set_sensitive(has_key);
    }

    // The keys are listed by gpg, so the row fills in once it answers.
    let fingerprints: Rc<RefCell<Vec<String>>> = Rc::default();
    {
        let (key, fps, current, address) = (key.downgrade(), fingerprints.clone(), prefs.signing_key.clone(), account.address.clone());
        let (uid, deps) = (uid.clone(), dependents.clone());
        glib::spawn_future_local(async move {
            let listed = gio::spawn_blocking(salutations_core::crypto::list_secret_keys_sync)
                .await
                .unwrap_or_else(|_| Err("gpg failed".into()));
            let Some(key) = key.upgrade() else { return };
            // Without gpg's reason a failed listing looks like a missing key.
            let mut keys = match listed {
                Ok(keys) => keys,
                Err(why) => {
                    key.set_subtitle(&format!("Cannot list keys: {why}"));
                    Vec::new()
                }
            };
            // Keys for the account's own address first.
            keys.sort_by_key(|k| salutations_core::crypto::user_id_email(&k.user_id) != address);
            let mut labels = vec!["None".to_string()];
            labels.extend(keys.iter().map(|k| {
                let tail = &k.fingerprint[k.fingerprint.len().saturating_sub(8)..];
                format!("{} · {}", k.user_id, tail)
            }));
            let mut list: Vec<String> = keys.iter().map(|k| k.fingerprint.clone()).collect();
            // A chosen key no longer in the keyring stays listed, so the
            // row never shows a value other than the one in use.
            if let Some(c) = &current
                && !list.contains(c)
            {
                labels.push(format!("Missing key · {}", &c[c.len().saturating_sub(8)..]));
                list.push(c.clone());
            }
            let model = gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>());
            key.set_model(Some(&model));
            let selected = current.as_ref().and_then(|c| list.iter().position(|f| f == c)).map_or(0, |i| i + 1);
            key.set_selected(selected as u32);
            *fps.borrow_mut() = list;
            let fps = fps.clone();
            key.connect_selected_notify(move |row| {
                let chosen = (row.selected() as usize).checked_sub(1).and_then(|i| fps.borrow().get(i).cloned());
                for d in &deps {
                    d.set_sensitive(chosen.is_some());
                }
                save_compose(&uid, |p| p.signing_key = chosen);
            });
        });
    }
    {
        let (uid, replies) = (uid.clone(), encrypt_replies.clone());
        encrypt_new.connect_active_notify(move |row| {
            let on = row.is_active();
            save_compose(&uid, |p| p.encrypt_new = on);
            replies.set_visible(!on);
        });
    }
    {
        let uid = uid.clone();
        encrypt_replies.connect_active_notify(move |row| {
            let on = row.is_active();
            save_compose(&uid, |p| p.encrypt_replies = on);
        });
    }
    {
        let (uid, attach) = (uid.clone(), attach.clone());
        autocrypt.connect_active_notify(move |row| {
            let on = row.is_active();
            attach.set_visible(on);
            save_compose(&uid, |p| p.autocrypt = on);
        });
    }
    attach.connect_selected_notify(move |row| {
        let v = match row.selected() {
            0 => AttachKey::Always,
            1 => AttachKey::WhenProtected,
            _ => AttachKey::Never,
        };
        save_compose(&uid, |p| p.attach_key = v);
    });
    group
}

/// Changes the account's Salutations-kept sync settings and saves them.
fn update_sync(uid: &str, on_change: &AccountChanged, f: impl FnOnce(&mut salutations_core::SyncPrefs)) {
    let mut st = Settings::get();
    f(st.sync.entry(uid.to_string()).or_default());
    if let Err(e) = st.save() {
        glib::g_warning!("salutations", "Could not save settings: {}", e);
        return;
    }
    on_change(uid, AccountChange::Sync);
}

/// How often to check, push, sound, and what to keep offline.
fn receiving_group(registry: &SourceRegistry, entry: AccountEntry, title: &str, on_account: AccountChanged) -> adw::PreferencesGroup {
    let current = offline::read(registry, &entry.account).unwrap_or(offline::DEFAULT);
    let current_refresh = offline::read_refresh(registry, &entry.account).unwrap_or(offline::DEFAULT_REFRESH);
    let group = adw::PreferencesGroup::builder().title(glib::markup_escape_text(title)).build();
    let check = Rc::new(PresetRow::new(
        "Check for new mail",
        None,
        &Refresh::PRESETS,
        Refresh::label,
        current_refresh,
        "Minutes",
        match current_refresh {
            Refresh::Minutes(m) => Some(m as f64),
            Refresh::Never => None,
        },
        (1.0, 1440.0),
    ));
    check.add_to(&group);
    {
        let (registry, account, on_change) = (registry.clone(), entry.account.clone(), on_account.clone());
        check.connect(
            |n| Refresh::Minutes(n as u32),
            move |r| match offline::write_refresh(&registry, &account, r) {
                Ok(()) => on_change(&account.uid, AccountChange::Refresh),
                Err(e) => glib::g_warning!("salutations", "Cannot save the check interval: {}", e),
            },
        );
    }
    let prefs = Settings::get().sync_for(&entry.account.uid);
    let mut metered_checks = vec![None];
    metered_checks.extend(Refresh::PRESETS.iter().map(|r| Some(*r)));
    let metered_check = Rc::new(PresetRow::new(
        "On metered connections",
        None,
        &metered_checks,
        |r: Option<Refresh>| r.map_or_else(|| "Same as above".into(), Refresh::label),
        prefs.metered_refresh,
        "Minutes",
        // Custom starts from the interval above.
        match prefs.metered_refresh.unwrap_or(current_refresh) {
            Refresh::Minutes(m) => Some(m as f64),
            Refresh::Never => None,
        },
        (1.0, 1440.0),
    ));
    metered_check.nested().add_to(&group);
    {
        let (uid, on_change) = (entry.account.uid.clone(), on_account.clone());
        metered_check.connect(
            |n| Some(Refresh::Minutes(n as u32)),
            move |r| update_sync(&uid, &on_change, |p| p.metered_refresh = r),
        );
    }
    let pause = adw::SwitchRow::builder().title("Pause in power save mode").active(prefs.pause_power_save).build();
    pause.add_css_class("preset-custom");
    group.add(&pause);
    {
        let (uid, on_change) = (entry.account.uid.clone(), on_account.clone());
        pause.connect_active_notify(move |row| {
            let on = row.is_active();
            update_sync(&uid, &on_change, |p| p.pause_power_save = on);
        });
    }

    let push = adw::SwitchRow::builder()
        .title("Receive server push messages")
        .active(offline::read_push(registry, &entry.account))
        .build();
    group.add(&push);
    {
        let (registry, account, on_change) = (registry.clone(), entry.account.clone(), on_account.clone());
        push.connect_active_notify(move |row| {
            if !row.is_sensitive() {
                return;
            }
            match offline::write_push(&registry, &account, row.is_active()) {
                Ok(()) => on_change(&account.uid, AccountChange::Push),
                Err(e) => glib::g_warning!("salutations", "Cannot save the push setting: {}", e),
            }
        });
    }
    // Greyed out when the server answers that it cannot push.
    if let Some((host, port, tls)) = offline::server(registry, &entry.account) {
        let row = push.downgrade();
        glib::spawn_future_local(async move {
            if salutations_core::push::supported(&host, port, tls).await == Some(false)
                && let Some(row) = row.upgrade()
            {
                row.set_sensitive(false);
                row.set_active(false);
                row.set_subtitle("Not supported by the server");
            }
        });
    }
    let sound = adw::SwitchRow::builder()
        .title("Notification sound")
        .active(!Settings::get().silent_accounts.contains(&entry.account.uid))
        .build();
    group.add(&sound);
    {
        let uid = entry.account.uid.clone();
        sound.connect_active_notify(move |row| {
            let mut st = Settings::get();
            st.silent_accounts.retain(|u| *u != uid);
            if !row.is_active() {
                st.silent_accounts.push(uid.clone());
            }
            if let Err(e) = st.save() {
                glib::g_warning!("salutations", "Could not save settings: {}", e);
            }
        });
    }

    // A window set elsewhere in months or years has no exact number of
    // days; it is listed as an entry of its own.
    let mut windows = AgeWindow::PRESETS.to_vec();
    if !windows.contains(&current.window) && window_days(current.window).is_none() {
        windows.insert(windows.len() - 1, current.window);
    }
    let mut downloads: Vec<Download> = windows.iter().map(|w| Download::Within(*w)).collect();
    downloads.push(Download::Never);
    let current_download = if prefs.never_download { Download::Never } else { Download::Within(current.window) };
    // Custom goes before All Time and Never.
    let custom_at = downloads.len() - 2;
    let age = Rc::new(PresetRow::with_custom_at(
        "Download from",
        None,
        &downloads,
        Download::label,
        current_download,
        "Days",
        window_days(current.window),
        (1.0, 3650.0),
        custom_at,
    ));
    age.add_to(&group);
    let mut metered_downloads = vec![None];
    metered_downloads.extend(downloads.iter().map(|d| Some(*d)));
    let metered_window = match prefs.metered_download {
        Some(Download::Within(w)) => window_days(w),
        _ => window_days(current.window),
    };
    let metered_age = Rc::new(PresetRow::with_custom_at(
        "On metered connections",
        None,
        &metered_downloads,
        |d: Option<Download>| d.map_or_else(|| "Same as above".into(), Download::label),
        prefs.metered_download,
        "Days",
        metered_window,
        (1.0, 3650.0),
        custom_at + 1,
    ));
    metered_age.nested().add_to(&group);
    {
        let (uid, on_change) = (entry.account.uid.clone(), on_account.clone());
        metered_age.clone().connect(
            |n| Some(Download::Within(AgeWindow::Last(TimeUnit::Days, n as i32))),
            move |d| update_sync(&uid, &on_change, |p| p.metered_download = d),
        );
    }
    // The metered row offers nothing wider than the row above; narrowing
    // that one narrows a wider metered choice to match.
    let constrain = {
        let (metered_age, downloads) = (metered_age.clone(), downloads.clone());
        let (uid, on_change) = (entry.account.uid.clone(), on_account.clone());
        Rc::new(move |normal: Download| {
            let Ok(now) = glib::DateTime::now_utc() else { return };
            let limit = normal.reach(&now);
            let mut list = vec![None];
            list.extend(downloads.iter().filter(|d| d.reach(&now) <= limit).map(|d| Some(*d)));
            // Custom goes before All Time, or before Never when All Time is
            // too wide; there is no Custom when only Never fits.
            let custom_at = (limit > 0)
                .then(|| {
                    list.iter()
                        .position(|d| *d == Some(Download::Within(AgeWindow::AllTime)))
                        .or_else(|| list.iter().position(|d| *d == Some(Download::Never)))
                })
                .flatten();
            let max_days = match normal {
                Download::Within(AgeWindow::AllTime) => 3650.0,
                _ => (limit / 86_400).clamp(1, 3650) as f64,
            };
            let current = Settings::get().sync_for(&uid).metered_download;
            let new = match current {
                Some(d) if d.reach(&now) > limit => Some(normal),
                c => c,
            };
            let days = match new {
                Some(Download::Within(w)) => window_days(w),
                _ => None,
            };
            metered_age.reset(&list, custom_at, new, days, max_days);
            if new != current {
                update_sync(&uid, &on_change, |p| p.metered_download = new);
            }
        })
    };
    constrain(current_download);
    let scope = adw::ComboRow::builder()
        .title("Download messages in")
        .model(&gtk::StringList::new(&["All folders", "Specific folders"]))
        .selected(if current.all_folders { 0 } else { 1 })
        .build();
    group.add(&scope);
    let folders = adw::ExpanderRow::builder()
        .title("Folders")
        .visible(!current.all_folders)
        .build();
    group.add(&folders);

    let save = {
        let registry = registry.clone();
        let account = entry.account.clone();
        let scope = scope.clone();
        let age = age.clone();
        let folders = folders.clone();
        let on_account = on_account.clone();
        Rc::new(move || {
            let download = age.value(&|n| Download::Within(AgeWindow::Last(TimeUnit::Days, n as i32)));
            // Never keeps the window in use for when it is set again.
            let window = match download {
                Download::Within(w) => w,
                Download::Never => offline::read(&registry, &account).map_or(offline::DEFAULT.window, |s| s.window),
            };
            let never = download == Download::Never;
            let mut st = Settings::get();
            if st.sync_for(&account.uid).never_download != never {
                st.sync.entry(account.uid.clone()).or_default().never_download = never;
                if let Err(e) = st.save() {
                    glib::g_warning!("salutations", "Could not save settings: {}", e);
                }
            }
            let sync = OfflineSync { all_folders: scope.selected() == 0, window };
            folders.set_visible(!sync.all_folders);
            if let Err(e) = offline::write(&registry, &account, &sync) {
                glib::g_warning!("salutations", "Cannot save offline settings: {}", e);
                return;
            }
            on_account(&account.uid, AccountChange::Offline);
        })
    };
    let s = save.clone();
    scope.connect_selected_notify(move |_| s());
    age.connect(
        |n| Download::Within(AgeWindow::Last(TimeUnit::Days, n as i32)),
        move |d| {
            save();
            constrain(d);
        },
    );

    // Folders are opened only when the list is shown; each switch
    // reads and writes the folder's offline state.
    let Some(store) = entry.store else {
        folders.set_sensitive(false);
        folders.set_subtitle("Available once the account has been opened");
        return group;
    };
    let filled = std::cell::Cell::new(false);
    let list = entry.folders;
    let uid = entry.account.uid.clone();
    let on_account = on_account.clone();
    folders.connect_expanded_notify(move |row| {
        if !row.is_expanded() || filled.replace(true) {
            return;
        }
        for (full_name, path) in &list {
            let check = gtk::CheckButton::builder().valign(gtk::Align::Center).sensitive(false).build();
            let folder_row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(path))
                .activatable_widget(&check)
                .build();
            folder_row.add_prefix(&check);
            row.add_row(&folder_row);
            let (store, full_name) = (store.clone(), full_name.clone());
            let (uid, on_account) = (uid.clone(), on_account.clone());
            let inbox = full_name.eq_ignore_ascii_case("INBOX");
            glib::spawn_future_local(async move {
                let Ok(Some(folder)) = store
                    .folder_future(&full_name, camel::StoreGetFolderFlags::empty(), glib::ffi::G_PRIORITY_DEFAULT)
                    .await
                else {
                    folder_row.set_subtitle("Cannot be opened");
                    return;
                };
                check.set_active(inbox || offline::folder_synced(&folder));
                check.set_sensitive(!inbox);
                if inbox {
                    folder_row.set_subtitle("Required");
                }
                check.connect_active_notify(move |c| {
                    offline::set_folder_synced(&folder, c.is_active());
                    on_account(&uid, AccountChange::Offline);
                });
            });
        }
    });
    group
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heights() {
        assert_eq!(dialog_height(600), 600);
        assert_eq!(dialog_height(685), 685);
        assert_eq!(dialog_height(700), 685);
        assert_eq!(dialog_height(720), 685);
        assert_eq!(dialog_height(800), 760);
    }
}

/// Senders and domains whose remote content is always shown, each with a
/// remove button, and Remove All, which also forgets the messages allowed
/// one at a time.
fn remote_allowed_row() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("Remote content allowed for").build();
    let rows: Rc<RefCell<Vec<gtk::Widget>>> = Rc::default();
    let refill: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::default();
    let fill = {
        let (expander, rows, refill) = (expander.clone(), rows.clone(), refill.clone());
        Rc::new(move || {
            for r in rows.borrow_mut().drain(..) {
                expander.remove(&r);
            }
            let settings = Settings::get();
            let mut entries: Vec<(String, bool)> = settings.remote_domains.iter().map(|d| (d.clone(), true)).collect();
            entries.extend(settings.remote_senders.iter().map(|e| (e.clone(), false)));
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let count = |n: usize, one: &str, many: &str| match n {
                0 => None,
                1 => Some(format!("1 {one}")),
                n => Some(format!("{n} {many}")),
            };
            let parts: Vec<String> = [
                count(settings.remote_senders.len(), "sender", "senders"),
                count(settings.remote_domains.len(), "domain", "domains"),
            ]
            .into_iter()
            .flatten()
            .collect();
            expander.set_subtitle(&if parts.is_empty() { "No senders or domains".to_string() } else { parts.join(", ") });
            for (value, domain) in entries {
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&value))
                    .subtitle(if domain { "Domain" } else { "Sender" })
                    .build();
                let remove = gtk::Button::builder()
                    .icon_name("edit-delete-symbolic")
                    .tooltip_text("Remove")
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                let refill = refill.clone();
                remove.connect_clicked(move |_| {
                    let mut settings = Settings::get();
                    if domain {
                        settings.remote_domains.retain(|d| *d != value);
                    } else {
                        settings.remote_senders.retain(|e| *e != value);
                    }
                    if let Err(e) = settings.save() {
                        glib::g_warning!("salutations", "Could not save settings: {}", e);
                    }
                    let f = refill.borrow().clone();
                    // Removing the row from inside its handler is unsafe.
                    if let Some(f) = f {
                        glib::idle_add_local_once(move || f());
                    }
                });
                row.add_suffix(&remove);
                expander.add_row(&row);
                rows.borrow_mut().push(row.upcast());
            }
        })
    };
    let all = gtk::Button::builder()
        .label("Remove all")
        .valign(gtk::Align::Center)
        .css_classes(["flat", "destructive-action"])
        .build();
    {
        let fill = fill.clone();
        all.connect_clicked(move |button| {
            let dialog = adw::AlertDialog::new(
                Some("Remove all?"),
                Some("Remote content will be blocked again for every sender, domain and message."),
            );
            dialog.add_responses(&[("cancel", "_Cancel"), ("remove", "_Remove all")]);
            dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            dialog.set_close_response("cancel");
            let fill = fill.clone();
            dialog.connect_response(Some("remove"), move |_, _| {
                let mut settings = Settings::get();
                settings.remote_domains.clear();
                settings.remote_senders.clear();
                if let Err(e) = settings.save() {
                    glib::g_warning!("salutations", "Could not save settings: {}", e);
                }
                salutations_core::allowed::clear();
                fill();
            });
            dialog.present(Some(button));
        });
    }
    expander.add_suffix(&all);
    *refill.borrow_mut() = Some(fill.clone());
    fill();
    expander
}
