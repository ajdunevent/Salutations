//! The message list: threads as summary rows that expand while one of their
//! messages is selected.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use salutations_core::camel::{self, prelude::*};

use crate::message_object::{MessageObject, Person, RowData, RowKind};

const SEEN: u32 = camel::MessageFlags::SEEN.bits();
const FLAGGED: u32 = camel::MessageFlags::FLAGGED.bits();
const DELETED: u32 = camel::MessageFlags::DELETED.bits();
const ATTACHMENTS: u32 = camel::MessageFlags::ATTACHMENTS.bits();
const ANSWERED: u32 = camel::MessageFlags::ANSWERED.bits();
/// The IMAP keyword for a forwarded message.
pub const FORWARDED: &str = "$Forwarded";

/// Space kept free of hover buttons at a row's right edge: a hovered
/// overlay scrollbar's width.
const SCROLLBAR_ROOM: i32 = 14;

/// Avatar layers drawn solid before the stack switches to fading.
const STACK_MAX: usize = 4;

#[derive(Debug, Clone)]
struct Msg {
    uid: String,
    from: Person,
    date: i64,
    subject: String,
    preview: String,
    flags: u32,
    important: bool,
    forwarded: bool,
    verified: Option<Option<String>>,
}

#[derive(Debug, Clone)]
struct Thread {
    key: String,
    /// In list order.
    msgs: Vec<Msg>,
}

impl Thread {
    fn newest(&self, newest_first: bool) -> &Msg {
        if newest_first { &self.msgs[0] } else { &self.msgs[self.msgs.len() - 1] }
    }
}

#[derive(Default)]
struct State {
    folder: Option<camel::Folder>,
    own: Option<String>,
    threads: Vec<Thread>,
    expanded: Option<String>,
    newest_first: bool,
    threaded: bool,
    /// Position and row of the previous selection, to tell which way the
    /// cursor moved onto a summary row.
    last: Option<(u32, MessageObject)>,
}

type OpenHandler = Rc<dyn Fn(Option<(camel::Folder, String)>)>;
type ModeHandler = Rc<dyn Fn(bool)>;
/// Gets the messages a drag carries (UIDs, whole thread) and returns its
/// content, or None to refuse the drag.
type DragHandler = Rc<dyn Fn(Vec<String>, bool) -> Option<gtk::gdk::ContentProvider>>;

pub struct MessageList {
    pub widget: gtk::ScrolledWindow,
    view: gtk::ListView,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    state: RefCell<State>,
    suppress: Cell<bool>,
    /// The current selection change comes from a navigation key, not a
    /// click or tap. Reset after the key event.
    key_nav: Cell<bool>,
    /// Phone layout: the list is the only pane shown.
    compact: Cell<bool>,
    /// Bound row widgets, for keeping a row's screen position while rows
    /// above it are removed.
    rows: RefCell<HashMap<MessageObject, (glib::WeakRef<gtk::Widget>, glib::SignalHandlerId)>>,
    on_open: RefCell<Option<OpenHandler>>,
    on_activate: RefCell<Option<Rc<dyn Fn()>>>,
    /// Selection mode: rows show checkboxes and clicks check messages.
    select_mode: Cell<bool>,
    /// Selection mode came from a right click, long press or menu key,
    /// so it ends when nothing is left checked.
    quick_select: Cell<bool>,
    /// UIDs of the checked messages in the current folder.
    checked: RefCell<HashSet<String>>,
    /// Verification results read from offline copies, by UID; cleared
    /// with the folder.
    verified_cache: RefCell<HashMap<String, Option<String>>>,
    /// Asks the window to enter or leave selection mode.
    on_mode: RefCell<Option<ModeHandler>>,
    /// Called after the checked set changes.
    on_checked: RefCell<Option<Rc<dyn Fn()>>>,
    /// Shift+↑/↓ is moving the cursor: the row it lands on gets checked.
    extend: Cell<bool>,
    on_drag: RefCell<Option<DragHandler>>,
    /// Status shown instead of the preview, by UID (waiting drafts).
    notes: RefCell<HashMap<String, String>>,
    /// Messages left out of the list: those whose move is waiting for the
    /// server.
    hidden: RefCell<Option<HiddenFilter>>,
    filter: Cell<Filter>,
    /// A row's quick action: the action, the row's messages, and whether
    /// they are a whole thread.
    on_quick: RefCell<Option<QuickHandler>>,
    /// The open folder is the archive, where Archive is not offered.
    in_archive: Cell<bool>,
    /// Messages that matched the filter since it was chosen. They stay
    /// listed after no longer matching, so reading a message in Unread
    /// does not take it out from under the user.
    kept: RefCell<HashSet<String>>,
}

type HiddenFilter = Rc<dyn Fn(&camel::Folder, &str) -> bool>;
type QuickHandler = Rc<dyn Fn(Quick, Vec<String>, bool)>;

pub fn parse_from(raw: &str) -> Person {
    let addr = camel::InternetAddress::new();
    if addr.unformat(raw) > 0
        && let Some((name, email)) = addr.get(0)
    {
        return Person {
            name: name.map(|n| n.to_string()).unwrap_or_default(),
            email: email.map(|e| e.to_string()).unwrap_or_default(),
        };
    }
    Person { name: raw.to_string(), email: String::new() }
}

/// Which messages the list shows; back to All when the folder changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Unread,
    Starred,
    Attachments,
}

impl Filter {
    pub fn from_name(name: &str) -> Self {
        match name {
            "unread" => Filter::Unread,
            "starred" => Filter::Starred,
            "attachments" => Filter::Attachments,
            _ => Filter::All,
        }
    }

    fn matches(self, flags: u32) -> bool {
        match self {
            Filter::All => true,
            Filter::Unread => flags & SEEN == 0,
            Filter::Starred => flags & FLAGGED != 0,
            Filter::Attachments => flags & ATTACHMENTS != 0,
        }
    }
}

/// The buttons a row shows under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quick {
    Read,
    Star,
    Archive,
    Delete,
}

/// What "Select read" and its siblings pick.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Read,
    Unread,
    Starred,
    Unstarred,
}

impl Kind {
    pub fn matches(self, unread: bool, starred: bool) -> bool {
        match self {
            Kind::Read => !unread,
            Kind::Unread => unread,
            Kind::Starred => starred,
            Kind::Unstarred => !starred,
        }
    }
}

fn msg_from_info(info: &camel::MessageInfo) -> Option<Msg> {
    let flags = info.flags();
    if flags & DELETED != 0 {
        return None;
    }
    let date = match info.date_received() {
        d if d > 0 => d,
        _ => info.date_sent(),
    };
    Some(Msg {
        uid: info.uid()?.to_string(),
        from: parse_from(&info.from().unwrap_or_default()),
        date,
        subject: info.subject().map(|s| s.to_string()).unwrap_or_default(),
        preview: info.preview().map(|p| salutations_core::render::clean_preview(&p)).unwrap_or_default(),
        flags,
        // Present only when Camel keeps these headers in the summary (see
        // salutations_core::summary_headers).
        important: salutations_core::compose::is_important(
            info.user_header("Importance").as_deref(),
            info.user_header("X-Priority").as_deref(),
        ),
        forwarded: camel::message_info_user_flag(info, FORWARDED),
        verified: info.user_header("Authentication-Results").map(|h| {
            let email = parse_from(&info.from().unwrap_or_default()).email;
            salutations_core::verify::parse(&h).verified_domain(&email)
        }),
    })
}

impl MessageList {
    pub fn new() -> Rc<Self> {
        let store = gio::ListStore::new::<MessageObject>();
        let selection = gtk::SingleSelection::builder()
            .model(&store)
            .autoselect(false)
            .can_unselect(true)
            .build();
        let view = gtk::ListView::builder().model(&selection).build();
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&view)
            .build();
        let this = Rc::new(Self {
            widget,
            view,
            store,
            selection,
            state: RefCell::new(State { newest_first: true, threaded: true, ..Default::default() }),
            suppress: Cell::new(false),
            key_nav: Cell::new(false),
            compact: Cell::new(false),
            rows: RefCell::new(HashMap::new()),
            on_open: RefCell::new(None),
            on_activate: RefCell::new(None),
            select_mode: Cell::new(false),
            quick_select: Cell::new(false),
            checked: RefCell::new(HashSet::new()),
            verified_cache: RefCell::new(HashMap::new()),
            on_mode: RefCell::new(None),
            on_checked: RefCell::new(None),
            extend: Cell::new(false),
            on_drag: RefCell::new(None),
            notes: RefCell::new(HashMap::new()),
            hidden: RefCell::new(None),
            filter: Cell::new(Filter::All),
            on_quick: RefCell::new(None),
            in_archive: Cell::new(false),
            kept: RefCell::new(HashSet::new()),
        });
        this.view.set_factory(Some(&this.factory()));
        // Rows are rebuilt so their times follow a 12/24-hour switch.
        let weak = Rc::downgrade(&this);
        crate::clock::connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                let keep = this.selected_uid();
                this.reload(keep);
            }
        });
        let weak = Rc::downgrade(&this);
        this.selection.connect_selected_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.selection_changed();
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            use gtk::gdk::Key as K;
            if [K::Up, K::Down, K::KP_Up, K::KP_Down, K::Page_Up, K::Page_Down, K::Home, K::End].contains(&key) {
                this.key_nav.set(true);
                let weak = Rc::downgrade(&this);
                glib::idle_add_local_once(move || {
                    if let Some(this) = weak.upgrade() {
                        this.key_nav.set(false);
                    }
                });
            }
            if let Some(p) = this.selection_key(key, mods) {
                return p;
            }
            // The context-menu keys start selection mode with the current
            // message checked, as a right click does.
            let menu_key = key == K::Menu || (key == K::F10 && mods == gtk::gdk::ModifierType::SHIFT_MASK);
            if menu_key {
                if let Some(row) = this.selection.selected_item().and_downcast::<MessageObject>() {
                    this.select_row(&row);
                }
                return glib::Propagation::Stop;
            }
            if !mods.is_empty() {
                return glib::Propagation::Proceed;
            }
            match key {
                gtk::gdk::Key::Left | gtk::gdk::Key::KP_Left => this.jump_in_thread(false),
                gtk::gdk::Key::Right | gtk::gdk::Key::KP_Right => this.jump_in_thread(true),
                _ => glib::Propagation::Proceed,
            }
        });
        this.view.add_controller(keys);
        let weak = Rc::downgrade(&this);
        this.view.connect_activate(move |_, pos| {
            let Some(this) = weak.upgrade() else { return };
            // A summary row is not a message; its double click only expands.
            let summary = this
                .store
                .item(pos)
                .and_downcast::<MessageObject>()
                .is_some_and(|o| o.data().kind == RowKind::Summary);
            if summary {
                return;
            }
            // Compact keyboard navigation only highlights; Enter opens.
            if this.compact.get() {
                this.key_nav.set(false);
                this.open_selected();
            }
            if let Some(cb) = this.on_activate.borrow().clone() {
                cb();
            }
        });
        this
    }

    pub fn connect_quick(&self, f: impl Fn(Quick, Vec<String>, bool) + 'static) {
        *self.on_quick.borrow_mut() = Some(Rc::new(f));
    }

    pub fn set_in_archive(&self, on: bool) {
        self.in_archive.set(on);
    }

    fn quick(&self, row: &MessageObject, action: Quick) {
        let d = row.data();
        let (uids, thread) = match &d.uid {
            Some(uid) => (vec![uid.clone()], false),
            None => (self.thread_uids(&d.thread), true),
        };
        let cb = self.on_quick.borrow().clone();
        if let Some(cb) = cb
            && !uids.is_empty()
        {
            cb(action, uids, thread);
        }
    }

    pub fn filter(&self) -> Filter {
        self.filter.get()
    }

    pub fn set_filter(&self, filter: Filter) {
        if self.filter.replace(filter) == filter {
            return;
        }
        self.kept.borrow_mut().clear();
        self.reload(self.selected_uid());
    }

    pub fn is_empty(&self) -> bool {
        self.store.n_items() == 0
    }

    /// Called with the number of rows whenever it changes.
    pub fn connect_rows(&self, f: impl Fn(u32) + 'static) {
        self.store.connect_items_changed(move |store, _, _, _| f(store.n_items()));
    }

    /// Leaves out the messages `f` returns true for, from the next reload.
    pub fn set_hidden_filter(&self, f: impl Fn(&camel::Folder, &str) -> bool + 'static) {
        *self.hidden.borrow_mut() = Some(Rc::new(f));
    }

    /// Called with the folder and UID of the message to show, or None when
    /// nothing is selected.
    pub fn connect_open(&self, f: impl Fn(Option<(camel::Folder, String)>) + 'static) {
        *self.on_open.borrow_mut() = Some(Rc::new(f));
    }

    /// Enter or double click on a message row.
    pub fn connect_activate(&self, f: impl Fn() + 'static) {
        *self.on_activate.borrow_mut() = Some(Rc::new(f));
    }

    /// Starts selection mode, if it is off, and checks `row` (a whole
    /// thread for a thread's summary row); unchecks it when it is checked.
    fn select_row(&self, row: &MessageObject) {
        let (uids, _, selecting) = self.row_targets(row);
        if selecting {
            // `uids` is then every checked message; only this row's go.
            let d = row.data();
            let own = match &d.uid {
                Some(uid) => vec![uid.clone()],
                None => self.thread_uids(&d.thread),
            };
            let mut c = self.checked.borrow_mut();
            for u in &own {
                c.remove(u);
            }
            drop(c);
            self.sync_checks();
            return;
        }
        if !self.select_mode.get() {
            self.request_mode(true);
            // After the request: entering clears it.
            self.quick_select.set(true);
        }
        self.check(&uids);
    }

    /// Shows `notes` in place of those messages' previews, in italics.
    pub fn set_notes(&self, notes: HashMap<String, String>) {
        if *self.notes.borrow() == notes {
            return;
        }
        self.notes.replace(notes);
        for (row, (holder, _)) in self.rows.borrow().iter() {
            if let Some(holder) = holder.upgrade() {
                self.apply_note(row, &holder);
            }
        }
    }

    fn apply_note(&self, row: &MessageObject, holder: &gtk::Widget) {
        let note = row.data().uid.as_ref().and_then(|u| self.notes.borrow().get(u).cloned());
        let Some(widget) = holder.first_child() else { return };
        let mut stack = vec![widget.clone()];
        while let Some(w) = stack.pop() {
            if w.has_css_class("preview")
                && let Some(label) = w.downcast_ref::<gtk::Label>()
            {
                match &note {
                    Some(n) => label.set_label(n),
                    None => label.set_label(&row.data().preview),
                }
            }
            let mut c = w.first_child();
            while let Some(child) = c {
                c = child.next_sibling();
                stack.push(child);
            }
        }
        if note.is_some() {
            widget.add_css_class("waiting");
        } else {
            widget.remove_css_class("waiting");
        }
    }

    /// A row is dragged with the mouse or touchpad.
    pub fn connect_drag(&self, f: impl Fn(Vec<String>, bool) -> Option<gtk::gdk::ContentProvider> + 'static) {
        *self.on_drag.borrow_mut() = Some(Rc::new(f));
    }

    /// The messages a menu or drag on `row` acts on, whether they are a
    /// whole thread, and whether they are the checked set.
    fn row_targets(&self, row: &MessageObject) -> (Vec<String>, bool, bool) {
        let d = row.data();
        let (uids, thread) = match &d.uid {
            Some(uid) => (vec![uid.clone()], false),
            None => (self.thread_uids(&d.thread), true),
        };
        let all_checked = !uids.is_empty() && {
            let checked = self.checked.borrow();
            uids.iter().all(|u| checked.contains(u))
        };
        // In selection mode a checked row stands for every checked message.
        if self.select_mode.get() && all_checked {
            return (self.checked_uids(), false, true);
        }
        (uids, thread, false)
    }

    /// The message to show after `uids` leave the list: the next one in
    /// list order, else the previous one.
    pub fn neighbor(&self, uids: &[String]) -> Option<String> {
        let st = self.state.borrow();
        let order: Vec<&str> = st.threads.iter().flat_map(|t| t.msgs.iter().map(|m| m.uid.as_str())).collect();
        let last = order.iter().rposition(|u| uids.iter().any(|x| x == u))?;
        let first = order.iter().position(|u| uids.iter().any(|x| x == u))?;
        order[last + 1..]
            .iter()
            .chain(order[..first].iter().rev())
            .find(|u| !uids.iter().any(|x| x == *u))
            .map(|u| u.to_string())
    }

    /// Selects and opens `uid`, expanding its thread.
    pub fn open_uid(&self, uid: &str) {
        let key = {
            let st = self.state.borrow();
            st.threads
                .iter()
                .find(|t| t.msgs.len() > 1 && t.msgs.iter().any(|m| m.uid == uid))
                .map(|t| t.key.clone())
        };
        let expanded = self.state.borrow().expanded.clone();
        if expanded.is_some() && expanded != key {
            self.collapse(None);
        }
        if let Some(k) = &key
            && let Some(summary) = self.summary_position(k)
        {
            self.expand_if_needed(k, summary);
        }
        let pos = (0..self.store.n_items()).find(|&i| {
            self.store
                .item(i)
                .and_downcast::<MessageObject>()
                .is_some_and(|o| o.data().uid.as_deref() == Some(uid))
        });
        if let Some(pos) = pos {
            self.select(pos);
            self.emit_open(self.folder().map(|f| (f, uid.to_string())));
        }
    }

    /// Phone layout: tapping a thread only expands it, and keyboard
    /// navigation highlights rows without opening them, so no message is
    /// loaded (and marked read) out of sight.
    pub fn set_compact(&self, compact: bool) {
        self.compact.set(compact);
    }

    /// Clears the selection without opening or clearing anything.
    pub fn unselect(&self) {
        self.suppress.set(true);
        self.selection.set_selected(gtk::INVALID_LIST_POSITION);
        self.suppress.set(false);
        self.state.borrow_mut().last = None;
    }

    /// Called with true or false when a key or click asks to enter or leave
    /// selection mode; the window then calls [`MessageList::set_select_mode`].
    pub fn connect_mode_request(&self, f: impl Fn(bool) + 'static) {
        *self.on_mode.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_checked(&self, f: impl Fn() + 'static) {
        *self.on_checked.borrow_mut() = Some(Rc::new(f));
    }

    fn request_mode(&self, on: bool) {
        let cb = self.on_mode.borrow().clone();
        match cb {
            Some(cb) => cb(on),
            None => self.set_select_mode(on),
        }
    }

    pub fn select_mode(&self) -> bool {
        self.select_mode.get()
    }

    /// Entering keeps the open message's row as the cursor and checks
    /// nothing; leaving clears the checks and the cursor.
    pub fn set_select_mode(&self, on: bool) {
        if self.select_mode.replace(on) == on {
            return;
        }
        self.quick_select.set(false);
        if on {
            self.view.add_css_class("selection-mode");
        } else {
            self.view.remove_css_class("selection-mode");
            self.checked.borrow_mut().clear();
            self.unselect();
        }
        // ListView keeps a row's widgets when its item is unchanged, so the
        // bound rows' checkboxes are switched here; new binds read the mode.
        for (holder, _) in self.rows.borrow().values() {
            if let Some(check) = holder.upgrade().and_then(|h| row_check(&h)) {
                check.set_visible(on);
            }
        }
        self.sync_checks();
    }


    pub fn checked_uids(&self) -> Vec<String> {
        self.checked.borrow().iter().cloned().collect()
    }

    /// Checks `uids` in addition to those already checked.
    pub fn check(&self, uids: &[String]) {
        self.checked.borrow_mut().extend(uids.iter().cloned());
        self.sync_checks();
    }

    /// Checks or unchecks every message in the folder, expanded or not.
    pub fn set_all_checked(&self, on: bool) {
        {
            let st = self.state.borrow();
            let mut checked = self.checked.borrow_mut();
            checked.clear();
            if on {
                checked.extend(st.threads.iter().flat_map(|t| t.msgs.iter().map(|m| m.uid.clone())));
            }
        }
        self.sync_checks();
    }

    /// Checks exactly the messages of `kind`, expanded or not.
    pub fn check_kind(&self, kind: Kind) {
        {
            let st = self.state.borrow();
            let mut checked = self.checked.borrow_mut();
            checked.clear();
            let all = st.threads.iter().flat_map(|t| t.msgs.iter());
            checked.extend(all.filter(|m| kind.matches(m.flags & SEEN == 0, m.flags & FLAGGED != 0)).map(|m| m.uid.clone()));
        }
        self.sync_checks();
    }

    /// Updates every row's checkbox from the checked set and notifies.
    fn sync_checks(&self) {
        {
            let st = self.state.borrow();
            let checked = self.checked.borrow();
            for i in 0..self.store.n_items() {
                let Some(row) = self.store.item(i).and_downcast::<MessageObject>() else { continue };
                let d = row.data();
                let (all, some) = match &d.uid {
                    Some(uid) => {
                        let c = checked.contains(uid);
                        (c, c)
                    }
                    None => {
                        let msgs = st.threads.iter().find(|t| t.key == d.thread).map(|t| &t.msgs);
                        let n = msgs.map_or(0, |m| m.iter().filter(|m| checked.contains(&m.uid)).count());
                        let total = msgs.map_or(0, |m| m.len());
                        (n == total && total > 0, n > 0)
                    }
                };
                if row.checked() != all {
                    row.set_checked(all);
                }
                if row.inconsistent() != (some && !all) {
                    row.set_inconsistent(some && !all);
                }
            }
        }
        let cb = self.on_checked.borrow().clone();
        if let Some(cb) = cb {
            cb();
        }
        if self.quick_select.get() && self.select_mode.get() && self.checked.borrow().is_empty() {
            self.request_mode(false);
        }
    }

    fn thread_uids(&self, key: &str) -> Vec<String> {
        self.state
            .borrow()
            .threads
            .iter()
            .find(|t| t.key == key)
            .map(|t| t.msgs.iter().map(|m| m.uid.clone()).collect())
            .unwrap_or_default()
    }

    fn position_of(&self, row: &MessageObject) -> Option<u32> {
        (0..self.store.n_items()).find(|&i| self.store.item(i).and_downcast::<MessageObject>().as_ref() == Some(row))
    }

    /// Moves the cursor (the list's selection, which opens nothing in
    /// selection mode) without running selection_changed().
    fn set_cursor(&self, row: &MessageObject) {
        if let Some(p) = self.position_of(row) {
            self.suppress.set(true);
            self.selection.set_selected(p);
            self.suppress.set(false);
            self.view.scroll_to(p, gtk::ListScrollFlags::FOCUS, None);
            self.state.borrow_mut().last = Some((p, row.clone()));
        }
    }

    /// Collapses the expanded thread when `row` is outside it, keeping
    /// `row` where it is on screen.
    fn collapse_unless_in(&self, row: &MessageObject) {
        let expanded = self.state.borrow().expanded.clone();
        if expanded.is_some_and(|k| k != row.data().thread) {
            self.collapse(Some(row));
        }
    }

    /// Selection mode: the cursor landed on `row` from the keyboard or a
    /// click the list handled itself. Summary rows behave as in reading
    /// mode, without opening anything: ↓ onto one enters the thread at its
    /// first message, ↑ from its first message leaves it.
    fn cursor_moved(&self, pos: u32, row: &MessageObject) {
        let d = row.data().clone();
        let last = self.state.borrow().last.clone();
        let mut target = row.clone();
        if d.kind == RowKind::Summary {
            let open_here = self.state.borrow().expanded.as_deref() == Some(d.thread.as_str());
            let came_up = last.as_ref().is_some_and(|(p, l)| *p == pos + 1 && l.data().thread == d.thread);
            if open_here && came_up {
                self.collapse(None);
                if pos == 0 {
                    self.set_cursor(row);
                    return;
                }
                match self.store.item(pos - 1).and_downcast::<MessageObject>() {
                    Some(above) => return self.land_cursor(&above),
                    None => return,
                }
            }
            if !open_here {
                self.collapse(None);
            }
            let Some(summary) = self.summary_position(&d.thread) else { return };
            let summary = self.expand_if_needed(&d.thread, summary).unwrap_or(summary);
            let from_below = last.as_ref().is_some_and(|(p, _)| *p > pos);
            let child = if from_below { summary + d.count as u32 } else { summary + 1 };
            match self.store.item(child).and_downcast::<MessageObject>() {
                Some(c) => target = c,
                None => return,
            }
        } else {
            self.collapse_unless_in(row);
        }
        self.set_cursor(&target);
        if self.extend.get()
            && let Some(uid) = target.data().uid.clone()
        {
            self.checked.borrow_mut().insert(uid);
            self.sync_checks();
        }
    }

    fn land_cursor(&self, row: &MessageObject) {
        let Some(p) = self.position_of(row) else { return };
        self.cursor_moved(p, row);
    }

    /// Selection keys, handled before the list's own: Space, Shift+↑/↓,
    /// Ctrl+A, Shift+Ctrl+A and Escape.
    fn selection_key(&self, key: gtk::gdk::Key, mods: gtk::gdk::ModifierType) -> Option<glib::Propagation> {
        use gtk::gdk::{Key as K, ModifierType as M};
        let ctrl = mods.contains(M::CONTROL_MASK);
        let shift = mods.contains(M::SHIFT_MASK);
        let stop = Some(glib::Propagation::Stop);
        match key {
            K::a | K::A if ctrl => {
                self.request_mode(true);
                self.set_all_checked(!shift);
                stop
            }
            K::Escape if self.select_mode.get() => {
                self.request_mode(false);
                stop
            }
            K::space | K::KP_Space if mods.is_empty() => {
                let row = self.selection.selected_item().and_downcast::<MessageObject>()?;
                let uid = row.data().uid.clone()?;
                self.request_mode(true);
                self.toggle(&uid);
                stop
            }
            K::Up | K::Down | K::KP_Up | K::KP_Down if shift && !ctrl => {
                // Checks the starting message, then lets the list move the
                // cursor; cursor_moved() checks where it lands.
                if let Some(uid) = self.selected_uid() {
                    self.request_mode(true);
                    self.checked.borrow_mut().insert(uid);
                    self.sync_checks();
                }
                if !self.select_mode.get() {
                    return None;
                }
                self.extend.set(true);
                let delta: i64 = if matches!(key, K::Up | K::KP_Up) { -1 } else { 1 };
                let pos = self.selection.selected() as i64 + delta;
                if pos >= 0 && (pos as u32) < self.store.n_items() {
                    self.selection.set_selected(pos as u32);
                }
                self.extend.set(false);
                stop
            }
            _ => None,
        }
    }

    fn toggle(&self, uid: &str) {
        {
            let mut c = self.checked.borrow_mut();
            if !c.remove(uid) {
                c.insert(uid.to_string());
            }
        }
        self.sync_checks();
    }

    /// Checks every message of `key`'s thread, or unchecks them all when
    /// all were already checked.
    fn toggle_thread(&self, key: &str) {
        let uids = self.thread_uids(key);
        {
            let mut c = self.checked.borrow_mut();
            if uids.iter().all(|u| c.contains(u)) {
                for u in &uids {
                    c.remove(u);
                }
            } else {
                c.extend(uids);
            }
        }
        self.sync_checks();
    }

    /// A click on `row` in selection mode, or with Ctrl or Shift held.
    /// `on_check` is a click on a summary row's checkbox.
    fn row_clicked(&self, row: &MessageObject, on_check: bool, ctrl: bool, shift: bool) {
        if !self.select_mode.get() {
            // Ctrl- or Shift-click from reading mode keeps the open message.
            let open = self.selected_uid();
            self.request_mode(true);
            if let Some(uid) = open {
                self.checked.borrow_mut().insert(uid);
            }
        }
        let d = row.data().clone();
        if shift {
            self.check_range(row);
            return;
        }
        let _ = ctrl;
        match d.kind {
            RowKind::Summary => {
                let open_here = self.state.borrow().expanded.as_deref() == Some(d.thread.as_str());
                if !open_here {
                    self.collapse(None);
                    if let Some(p) = self.summary_position(&d.thread) {
                        self.expand(&d.thread);
                        if let Some(r) = self.store.item(p).and_downcast::<MessageObject>() {
                            self.set_cursor(&r);
                        }
                    }
                    if !on_check {
                        self.sync_checks();
                        return;
                    }
                }
                self.toggle_thread(&d.thread);
            }
            RowKind::Single | RowKind::Child => {
                self.collapse_unless_in(row);
                self.set_cursor(row);
                if let Some(uid) = &d.uid {
                    self.toggle(uid);
                }
            }
        }
    }

    /// Shift-click: checks every message from the cursor to `row`,
    /// including all messages of collapsed threads in between.
    fn check_range(&self, row: &MessageObject) {
        let Some(to) = self.position_of(row) else { return };
        let from = self.state.borrow().last.as_ref().map_or(to, |(p, _)| *p);
        let (a, b) = (from.min(to), from.max(to));
        let expanded = self.state.borrow().expanded.clone();
        let mut add = Vec::new();
        for i in a..=b {
            let Some(r) = self.store.item(i).and_downcast::<MessageObject>() else { continue };
            let d = r.data();
            match (&d.uid, d.kind) {
                (Some(uid), _) => add.push(uid.clone()),
                (None, RowKind::Summary) if expanded.as_deref() != Some(d.thread.as_str()) => {
                    add.extend(self.thread_uids(&d.thread));
                }
                _ => {}
            }
        }
        self.checked.borrow_mut().extend(add);
        self.set_cursor(row);
        self.sync_checks();
    }

    pub fn folder(&self) -> Option<camel::Folder> {
        self.state.borrow().folder.clone()
    }

    pub fn set_folder(&self, folder: Option<&camel::Folder>, own: Option<String>) {
        {
            let mut st = self.state.borrow_mut();
            st.folder = folder.cloned();
            self.verified_cache.borrow_mut().clear();
            self.filter.set(Filter::All);
            self.kept.borrow_mut().clear();
            st.own = own;
            st.expanded = None;
            st.last = None;
        }
        self.checked.borrow_mut().clear();
        if self.select_mode.get() {
            self.request_mode(false);
        }
        self.reload(None);
        self.emit_open(None);
    }

    pub fn set_order(&self, newest_first: bool, threaded: bool) {
        let keep = self.selected_uid();
        {
            let mut st = self.state.borrow_mut();
            st.newest_first = newest_first;
            st.threaded = threaded;
        }
        self.reload(keep);
    }

    pub fn selected_uid(&self) -> Option<String> {
        self.selection
            .selected_item()
            .and_downcast::<MessageObject>()
            .and_then(|o| o.data().uid.clone())
    }

    /// Rebuilds the rows from the folder summary, keeping `keep` selected
    /// (and its thread expanded) when it still exists.
    pub fn reload(&self, keep: Option<String>) {
        self.load_threads();
        {
            let st = self.state.borrow();
            let all: HashSet<&str> = st.threads.iter().flat_map(|t| t.msgs.iter().map(|m| m.uid.as_str())).collect();
            self.checked.borrow_mut().retain(|u| all.contains(u.as_str()));
        }
        let keep_thread = keep.as_ref().and_then(|uid| {
            let st = self.state.borrow();
            st.threads
                .iter()
                .find(|t| t.msgs.len() > 1 && t.msgs.iter().any(|m| &m.uid == uid))
                .map(|t| t.key.clone())
        });
        self.state.borrow_mut().expanded = keep_thread;
        let rows = self.build_rows();
        self.suppress.set(true);
        self.store.splice(0, self.store.n_items(), &rows);
        let pos = keep.and_then(|uid| {
            rows.iter()
                .position(|r| r.data().uid.as_deref() == Some(uid.as_str()))
        });
        match pos {
            Some(p) => {
                self.selection.set_selected(p as u32);
                self.view.scroll_to(p as u32, gtk::ListScrollFlags::NONE, None);
                self.state.borrow_mut().last = Some((p as u32, rows[p].clone()));
            }
            None => {
                self.selection.set_selected(gtk::INVALID_LIST_POSITION);
                self.state.borrow_mut().last = None;
            }
        }
        self.suppress.set(false);
        self.sync_checks();
    }

    /// Applies new flags from the folder summary to existing rows.
    pub fn refresh_flags(&self, uids: &[String]) {
        let Some(folder) = self.folder() else { return };
        let mut touched_threads = Vec::new();
        {
            let mut st = self.state.borrow_mut();
            for t in st.threads.iter_mut() {
                for m in t.msgs.iter_mut() {
                    if uids.contains(&m.uid) {
                        m.flags = folder.message_flags(&m.uid);
                        m.forwarded = folder.message_info(&m.uid).is_some_and(|i| camel::message_info_user_flag(&i, FORWARDED));
                        touched_threads.push(t.key.clone());
                    }
                }
            }
        }
        let st = self.state.borrow();
        for i in 0..self.store.n_items() {
            let Some(row) = self.store.item(i).and_downcast::<MessageObject>() else { continue };
            let d = row.data();
            match d.kind {
                RowKind::Summary if touched_threads.contains(&d.thread) => {
                    if let Some(t) = st.threads.iter().find(|t| t.key == d.thread) {
                        row.set_unread(t.msgs.iter().any(|m| m.flags & SEEN == 0));
                        row.set_starred(t.msgs.iter().any(|m| m.flags & FLAGGED != 0));
                        row.set_answered(t.msgs.iter().any(|m| m.flags & ANSWERED != 0));
                        row.set_forwarded(t.msgs.iter().any(|m| m.forwarded));
                    }
                }
                RowKind::Single | RowKind::Child => {
                    if let Some(uid) = d.uid.as_ref().filter(|u| uids.contains(u)) {
                        let flags = folder.message_flags(uid);
                        row.set_unread(flags & SEEN == 0);
                        row.set_starred(flags & FLAGGED != 0);
                        row.set_answered(flags & ANSWERED != 0);
                        row.set_forwarded(folder.message_info(uid).is_some_and(|i| camel::message_info_user_flag(&i, FORWARDED)));
                    }
                }
                _ => {}
            }
        }
    }

    fn load_threads(&self) {
        let mut st = self.state.borrow_mut();
        let Some(folder) = st.folder.clone() else {
            st.threads.clear();
            return;
        };
        let newest_first = st.newest_first;
        let hidden = self.hidden.borrow().clone();
        let shown = |uid: &str| !hidden.as_ref().is_some_and(|h| h(&folder, uid));
        let mut threads: Vec<Thread> = if st.threaded {
            camel::thread_folder(&folder)
                .into_iter()
                .filter_map(|root| {
                    let msgs: Vec<Msg> = root.flatten().iter().filter_map(msg_from_info).filter(|m| shown(&m.uid)).collect();
                    let key = root.info.uid()?.to_string();
                    (!msgs.is_empty()).then_some(Thread { key, msgs })
                })
                .collect()
        } else {
            folder
                .uids()
                .iter()
                .filter_map(|uid| folder.message_info(uid))
                .filter_map(|info| msg_from_info(&info))
                .filter(|m| shown(&m.uid))
                .map(|m| Thread { key: m.uid.clone(), msgs: vec![m] })
                .collect()
        };
        // A thread is listed whole when any of its messages passes.
        let filter = self.filter.get();
        if filter != Filter::All {
            let mut kept = self.kept.borrow_mut();
            threads.retain(|t| {
                let hit = t.msgs.iter().any(|m| filter.matches(m.flags) || kept.contains(&m.uid));
                if hit {
                    kept.extend(t.msgs.iter().filter(|m| filter.matches(m.flags)).map(|m| m.uid.clone()));
                }
                hit
            });
        }
        for t in threads.iter_mut() {
            t.msgs.sort_by_key(|m| m.date);
            if newest_first {
                t.msgs.reverse();
            }
        }
        threads.sort_by_key(|t| t.newest(newest_first).date);
        if newest_first {
            threads.reverse();
        }
        st.threads = threads;
    }

    fn build_rows(&self) -> Vec<MessageObject> {
        let st = self.state.borrow();
        let mut rows = Vec::new();
        for t in &st.threads {
            if t.msgs.len() == 1 {
                rows.push(single_row(&t.msgs[0], RowKind::Single, &t.key));
                continue;
            }
            rows.push(summary_row(t, st.newest_first, st.own.as_deref()));
            if st.expanded.as_deref() == Some(t.key.as_str()) {
                rows.extend(t.msgs.iter().map(|m| single_row(m, RowKind::Child, &t.key)));
            }
        }
        rows
    }

    fn summary_position(&self, key: &str) -> Option<u32> {
        (0..self.store.n_items()).find(|&i| {
            self.store
                .item(i)
                .and_downcast::<MessageObject>()
                .is_some_and(|o| o.data().kind == RowKind::Summary && o.data().thread == key)
        })
    }

    /// Inserts the thread's messages under its summary row; returns the
    /// summary's position.
    fn expand(&self, key: &str) -> Option<u32> {
        let pos = self.summary_position(key)?;
        let children: Vec<MessageObject> = {
            let st = self.state.borrow();
            let t = st.threads.iter().find(|t| t.key == key)?;
            t.msgs.iter().map(|m| single_row(m, RowKind::Child, key)).collect()
        };
        self.store.splice(pos + 1, 0, &children);
        self.state.borrow_mut().expanded = Some(key.to_string());
        Some(pos)
    }

    /// Removes the expanded thread's message rows, keeping `anchor` at the
    /// same height on screen when rows above it disappear.
    fn collapse(&self, anchor: Option<&MessageObject>) {
        let Some(key) = self.state.borrow_mut().expanded.take() else { return };
        let Some(pos) = self.summary_position(&key) else { return };
        let mut n = 0;
        while let Some(o) = self.store.item(pos + 1 + n).and_downcast::<MessageObject>() {
            if o.data().kind != RowKind::Child || o.data().thread != key {
                break;
            }
            n += 1;
        }
        let before = anchor.and_then(|a| self.row_y(a));
        self.store.splice(pos + 1, n, &[] as &[MessageObject]);
        if let (Some(a), Some(y0)) = (anchor.cloned(), before) {
            let view = self.view.clone();
            let scrolled = self.widget.clone();
            let rows = self.rows.borrow().get(&a).map(|(w, _)| w.clone());
            glib::idle_add_local_once(move || {
                let Some(w) = rows.and_then(|w| w.upgrade()) else { return };
                if let Some(p) = w.compute_point(&view, &gtk::graphene::Point::new(0.0, 0.0)) {
                    let adj = scrolled.vadjustment();
                    adj.set_value(adj.value() + (p.y() as f64 - y0));
                }
            });
        }
    }

    fn row_y(&self, row: &MessageObject) -> Option<f64> {
        let w = self.rows.borrow().get(row)?.0.upgrade()?;
        w.compute_point(&self.view, &gtk::graphene::Point::new(0.0, 0.0))
            .map(|p| p.y() as f64)
    }

    fn select(&self, pos: u32) {
        self.suppress.set(true);
        self.selection.set_selected(pos);
        self.suppress.set(false);
        self.view.scroll_to(pos, gtk::ListScrollFlags::FOCUS, None);
        if let Some(row) = self.store.item(pos).and_downcast::<MessageObject>() {
            self.state.borrow_mut().last = Some((pos, row));
        }
    }

    fn selection_changed(&self) {
        if self.suppress.get() {
            return;
        }
        let pos = self.selection.selected();
        let Some(row) = self.selection.selected_item().and_downcast::<MessageObject>() else {
            self.state.borrow_mut().last = None;
            self.emit_open(None);
            return;
        };
        let d = row.data().clone();
        let expanded = self.state.borrow().expanded.clone();
        if self.select_mode.get() {
            self.cursor_moved(pos, &row);
            return;
        }
        match d.kind {
            RowKind::Summary => {
                let key_nav = self.key_nav.get();
                let last = self.state.borrow().last.clone();
                let open_here = expanded.as_deref() == Some(d.thread.as_str());
                let came_up = key_nav
                    && last.as_ref().is_some_and(|(p, last)| {
                        *p == pos + 1 && last.data().kind == RowKind::Child && last.data().thread == d.thread
                    });
                if open_here && !key_nav {
                    // A tap on an open thread's summary changes nothing.
                    match last {
                        Some((p, _)) => {
                            self.suppress.set(true);
                            self.selection.set_selected(p);
                            self.suppress.set(false);
                        }
                        None => self.unselect(),
                    }
                    return;
                }
                if self.compact.get() && !key_nav {
                    if expanded.is_some() {
                        self.collapse(None);
                    }
                    if let Some(summary) = self.summary_position(&d.thread) {
                        self.expand_if_needed(&d.thread, summary);
                    }
                    self.unselect();
                    return;
                }
                if came_up && open_here {
                    // ↑ from the thread's first message leaves the thread.
                    if pos == 0 {
                        self.select(1);
                        return;
                    }
                    self.collapse(None);
                    self.land_on(pos - 1);
                    return;
                }
                let mut summary = pos;
                if expanded.as_deref().is_some_and(|k| k != d.thread) {
                    self.collapse(None);
                    summary = self.summary_position(&d.thread).unwrap_or(pos);
                }
                let Some(summary) = self.expand_if_needed(&d.thread, summary) else { return };
                let target = if self.state.borrow().newest_first { summary + 1 } else { summary + d.count as u32 };
                self.select(target);
                self.open_selected();
            }
            RowKind::Single | RowKind::Child => {
                if expanded.is_some_and(|k| k != d.thread) {
                    self.collapse(Some(&row));
                    if let Some(p) = (0..self.store.n_items()).find(|&i| {
                        self.store.item(i).and_downcast::<MessageObject>().as_ref() == Some(&row)
                    }) {
                        self.select(p);
                    }
                } else {
                    self.state.borrow_mut().last = Some((pos, row.clone()));
                }
                self.open_selected();
            }
        }
    }

    /// Selects `pos` as if the user had moved there, so a summary row
    /// expands.
    fn land_on(&self, pos: u32) {
        self.state.borrow_mut().last = None;
        self.suppress.set(true);
        self.selection.set_selected(pos);
        self.suppress.set(false);
        self.selection_changed();
    }

    /// ← and →: the top or bottom message of the expanded thread.
    fn jump_in_thread(&self, bottom: bool) -> glib::Propagation {
        let Some(row) = self.selection.selected_item().and_downcast::<MessageObject>() else {
            return glib::Propagation::Proceed;
        };
        if row.data().kind != RowKind::Child {
            return glib::Propagation::Stop;
        }
        let Some(summary) = self.summary_position(&row.data().thread) else {
            return glib::Propagation::Stop;
        };
        let count = self
            .state
            .borrow()
            .threads
            .iter()
            .find(|t| t.key == row.data().thread)
            .map_or(0, |t| t.msgs.len() as u32);
        let target = if bottom { summary + count } else { summary + 1 };
        if target != self.selection.selected() && count > 0 {
            self.select(target);
            self.open_selected();
        }
        glib::Propagation::Stop
    }

    fn expand_if_needed(&self, key: &str, summary: u32) -> Option<u32> {
        if self.state.borrow().expanded.as_deref() == Some(key) {
            return Some(summary);
        }
        self.expand(key)
    }

    fn open_selected(&self) {
        if self.compact.get() && self.key_nav.get() {
            return;
        }
        let uid = self.selected_uid();
        let folder = self.folder();
        self.emit_open(folder.zip(uid));
    }

    fn emit_open(&self, target: Option<(camel::Folder, String)>) {
        let cb = self.on_open.borrow().clone();
        if let Some(cb) = cb {
            cb(target);
        }
    }

    fn factory(self: &Rc<Self>) -> gtk::SignalListItemFactory {
        let factory = gtk::SignalListItemFactory::new();
        let weak = Rc::downgrade(self);
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
            let holder = crate::row_holder::RowHolder::new();
            holder.add_overlay(&quick_actions(weak.clone(), item, &holder));
            // Capture phase, so a claimed press never reaches the list's handler
            // and moves the selection.
            let click = gtk::GestureClick::new();
            click.set_propagation_phase(gtk::PropagationPhase::Capture);
            let weak = weak.clone();
            let ctx_weak = weak.clone();
            let ctx_drag = weak.clone();
            let item_weak = item.downgrade();
            let h = holder.downgrade();
            // A claimed press acts on release, so a long press (which ends
            // the same touch) can replace it.
            let pending: Rc<Cell<Option<(bool, bool, bool)>>> = Rc::new(Cell::new(None));
            let long_fired = Rc::new(Cell::new(false));
            let (p, lf) = (pending.clone(), long_fired.clone());
            click.connect_pressed(move |g, n, x, y| {
                p.set(None);
                lf.set(false);
                let (Some(this), Some(holder)) = (weak.upgrade(), h.upgrade()) else { return };
                let mods = g.current_event_state();
                let ctrl = mods.contains(gtk::gdk::ModifierType::CONTROL_MASK);
                let shift = mods.contains(gtk::gdk::ModifierType::SHIFT_MASK);
                if !this.select_mode.get() && !ctrl && !shift {
                    return;
                }
                g.set_state(gtk::EventSequenceState::Claimed);
                if n > 1 {
                    return;
                }
                let on_check = row_check(holder.upcast_ref())
                    .and_then(|c| c.compute_bounds(&holder))
                    .is_some_and(|b| b.contains_point(&gtk::graphene::Point::new(x as f32, y as f32)));
                p.set(Some((on_check, ctrl, shift)));
            });
            let (p, lf, w2) = (pending.clone(), long_fired.clone(), ctx_weak.clone());
            click.connect_released(move |_, _, _, _| {
                let (Some(this), Some(item)) = (w2.upgrade(), item_weak.upgrade()) else { return };
                let Some(row) = item.item().and_downcast::<MessageObject>() else { return };
                if let Some((on_check, ctrl, shift)) = p.take()
                    && !lf.get()
                {
                    this.row_clicked(&row, on_check, ctrl, shift);
                }
            });
            holder.add_controller(click.clone());
            let menu_click = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_SECONDARY).build();
            let long_press = gtk::GestureLongPress::builder().touch_only(true).build();
            long_press.set_propagation_phase(gtk::PropagationPhase::Capture);
            let lf = long_fired.clone();
            long_press.connect_pressed(move |_, _, _| lf.set(true));
            for (gesture, w, item_weak, h) in [
                (menu_click.upcast_ref::<gtk::Gesture>().clone(), ctx_weak.clone(), item.downgrade(), holder.downgrade()),
                (long_press.upcast_ref::<gtk::Gesture>().clone(), ctx_weak, item.downgrade(), holder.downgrade()),
            ] {
                // There is no context menu: a right click or long press
                // starts selection mode with the row checked, or toggles
                // the row's check in selection mode.
                let fire = move |g: &gtk::Gesture| {
                    let (Some(this), Some(item), Some(_)) = (w.upgrade(), item_weak.upgrade(), h.upgrade()) else {
                        return;
                    };
                    let Some(row) = item.item().and_downcast::<MessageObject>() else { return };
                    g.set_state(gtk::EventSequenceState::Claimed);
                    this.select_row(&row);
                };
                if let Some(c) = gesture.downcast_ref::<gtk::GestureClick>() {
                    c.connect_pressed(move |g, _, _, _| fire(g.upcast_ref()));
                } else if let Some(l) = gesture.downcast_ref::<gtk::GestureLongPress>() {
                    l.connect_pressed(move |g, _, _| fire(g.upcast_ref()));
                }
                holder.add_controller(gesture);
            }
            // Grouped (both on the holder), so the long press sees the touch
            // that the claimed click holds.
            long_press.group_with(&click);
            let drag = drag_source(ctx_drag, item, long_fired);
            holder.add_controller(drag.clone());
            drag.group_with(&click);
            item.set_child(Some(&holder));
        });
        let weak = Rc::downgrade(self);
        factory.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
            let Some(row) = item.item().and_downcast::<MessageObject>() else { return };
            let holder = item.child().and_downcast::<crate::row_holder::RowHolder>().expect("row holder");
            let widget = row_widget(&row);
            if let Some(check) = widget.first_child().filter(|c| c.has_css_class("row-check")) {
                check.set_visible(weak.upgrade().is_some_and(|t| t.select_mode.get()));
            }
            holder.set_child(Some(&widget));
            if let Some(this) = weak.upgrade() {
                this.apply_note(&row, holder.upcast_ref());
                this.check_verification(&row, &widget);
            }
            let w = widget.downgrade();
            let handler = row.connect_unread_notify(move |r| {
                if let Some(w) = w.upgrade() {
                    set_unread_class(&w, r.unread());
                }
            });
            if let Some(this) = weak.upgrade() {
                let old = this
                    .rows
                    .borrow_mut()
                    .insert(row.clone(), (holder.upcast_ref::<gtk::Widget>().downgrade(), handler));
                if let Some((_, h)) = old {
                    row.disconnect(h);
                }
            }
        });
        let weak = Rc::downgrade(self);
        factory.connect_unbind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
            if let (Some(this), Some(row)) = (weak.upgrade(), item.item().and_downcast::<MessageObject>())
                && let Some((_, h)) = this.rows.borrow_mut().remove(&row)
            {
                row.disconnect(h);
            }
        });
        factory
    }
}

/// Read, Star, Archive and Delete at the top right of a row while the
/// pointer is on it; never for touch, and not in selection mode. They act
/// on the row's messages, whatever is selected.
fn quick_actions(list: std::rc::Weak<MessageList>, item: &gtk::ListItem, holder: &crate::row_holder::RowHolder) -> gtk::Box {
    let bar = gtk::Box::builder()
        .spacing(2)
        .halign(gtk::Align::End)
        // Clear of the overlay scrollbar, which widens under the pointer
        // and covered the rightmost button.
        .margin_end(SCROLLBAR_ROOM)
        .valign(gtk::Align::Start)
        .visible(false)
        .css_classes(["quick-actions"])
        .build();
    let button = |action: Quick| {
        let b = gtk::Button::builder().css_classes(["flat", "circular"]).can_focus(false).build();
        let (list, item) = (list.clone(), item.downgrade());
        b.connect_clicked(move |_| {
            let (Some(this), Some(item)) = (list.upgrade(), item.upgrade()) else { return };
            if let Some(row) = item.item().and_downcast::<MessageObject>() {
                this.quick(&row, action);
            }
        });
        bar.append(&b);
        b
    };
    let read = button(Quick::Read);
    let star = button(Quick::Star);
    let archive = button(Quick::Archive);
    archive.set_icon_name("salutations-archive-symbolic");
    archive.set_tooltip_text(Some("Archive"));
    let delete = button(Quick::Delete);
    delete.set_icon_name("user-trash-symbolic");
    delete.set_tooltip_text(Some("Trash"));
    let motion = gtk::EventControllerMotion::new();
    let (b, item, h) = (bar.clone(), item.downgrade(), holder.downgrade());
    let show = move |c: &gtk::EventControllerMotion| {
        let touch = c.current_event_device().is_some_and(|d| d.source() == gtk::gdk::InputSource::Touchscreen);
        let (Some(this), Some(item)) = (list.upgrade(), item.upgrade()) else { return };
        let Some(row) = item.item().and_downcast::<MessageObject>() else { return };
        let Some(holder) = h.upgrade() else { return };
        if touch || this.select_mode.get() {
            b.set_visible(false);
            holder.set_covered(None);
            return;
        }
        let (icon, tip) = if row.unread() {
            ("salutations-mark-read-symbolic", "Mark as read")
        } else {
            ("mail-unread-symbolic", "Mark as unread")
        };
        // Motion calls this often; setting the same icon rebuilds it.
        if read.icon_name().as_deref() != Some(icon) {
            read.set_icon_name(icon);
            read.set_tooltip_text(Some(tip));
        }
        let (icon, tip) = if row.starred() { ("starred-symbolic", "Unstar") } else { ("salutations-non-starred-symbolic", "Star") };
        if star.icon_name().as_deref() != Some(icon) {
            star.set_icon_name(icon);
            star.set_tooltip_text(Some(tip));
        }
        archive.set_visible(!this.in_archive.get());
        b.set_visible(true);
        let width = b.measure(gtk::Orientation::Horizontal, -1).1;
        let height = b.measure(gtk::Orientation::Vertical, width).1;
        holder.set_covered(Some((width as f32, height as f32)));
    };
    let show2 = show.clone();
    motion.connect_enter(move |c, _, _| show(c));
    // After a click the row's state changed; the icons follow it.
    motion.connect_motion(move |c, _, _| show2(c));
    let (b, h) = (bar.clone(), holder.downgrade());
    motion.connect_leave(move |_| {
        b.set_visible(false);
        if let Some(h) = h.upgrade() {
            h.set_covered(None);
        }
    });
    holder.add_controller(motion);
    bar
}

/// Drags a row's messages, or the checked ones from a checked row, onto a
/// folder. Mouse and touchpad only: on touch a long press selects.
fn drag_source(list: std::rc::Weak<MessageList>, item: &gtk::ListItem, suppress_click: Rc<Cell<bool>>) -> gtk::DragSource {
    let drag = gtk::DragSource::new();
    drag.set_actions(gtk::gdk::DragAction::MOVE);
    // In selection mode the row's click claims presses in the capture
    // phase, so a bubble-phase drag would never see the motion.
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    let item_weak = item.downgrade();
    let label: Rc<RefCell<String>> = Rc::default();
    let l = label.clone();
    drag.connect_prepare(move |src, _, _| {
        let touch = src
            .current_event_device()
            .is_some_and(|d| d.source() == gtk::gdk::InputSource::Touchscreen);
        if touch {
            return None;
        }
        let (this, item) = (list.upgrade()?, item_weak.upgrade()?);
        let row = item.item().and_downcast::<MessageObject>()?;
        let (uids, thread, _) = this.row_targets(&row);
        if uids.is_empty() {
            return None;
        }
        *l.borrow_mut() = match (thread, uids.len()) {
            (false, 1) => {
                let s = row.data().subject.clone();
                if s.is_empty() { "(No subject)".into() } else { s }
            }
            (_, n) => format!("{n} messages"),
        };
        let cb = this.on_drag.borrow().clone()?;
        cb(uids, thread)
    });
    drag.connect_drag_begin(move |_, d| {
        // The press that started the drag must not also check the row.
        suppress_click.set(true);
        let text = gtk::Label::new(Some(label.borrow().as_str()));
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.set_max_width_chars(40);
        text.add_css_class("drag-label");
        gtk::DragIcon::for_drag(d).set_child(Some(&text));
    });
    drag
}

/// A search result row; `key` identifies it among the results.
pub fn hit_row(info: &camel::MessageInfo, key: &str, location: Option<String>) -> Option<MessageObject> {
    let m = msg_from_info(info)?;
    let row = single_row(&m, RowKind::Single, key);
    let mut data = row.data().clone();
    data.location = location;
    Some(MessageObject::new(data, m.flags & SEEN == 0, m.flags & FLAGGED != 0).responded(m.flags & ANSWERED != 0, m.forwarded))
}

fn single_row(m: &Msg, kind: RowKind, thread: &str) -> MessageObject {
    MessageObject::new(
        RowData {
            kind,
            thread: thread.to_string(),
            uid: Some(m.uid.clone()),
            sender: m.from.display().to_string(),
            participants: vec![m.from.clone()],
            date: m.date,
            subject: m.subject.clone(),
            preview: m.preview.clone(),
            count: 1,
            attachment: m.flags & ATTACHMENTS != 0,
            important: m.important,
            location: None,
            verified: m.verified.clone(),
            logos: Vec::new(),
        },
        m.flags & SEEN == 0,
        m.flags & FLAGGED != 0,
    )
    .responded(m.flags & ANSWERED != 0, m.forwarded)
}

fn summary_row(t: &Thread, newest_first: bool, own: Option<&str>) -> MessageObject {
    let newest = t.newest(newest_first);
    let by_recency: Vec<&Msg> = if newest_first {
        t.msgs.iter().collect()
    } else {
        t.msgs.iter().rev().collect()
    };
    let mut participants: Vec<Person> = Vec::new();
    for m in by_recency {
        let email = m.from.email.to_ascii_lowercase();
        if own.is_some_and(|o| o == email) {
            continue;
        }
        if !participants.iter().any(|p| p.email.eq_ignore_ascii_case(&email)) {
            participants.push(m.from.clone());
        }
    }
    if participants.is_empty() {
        participants.push(newest.from.clone());
    }
    // A forged message from the same address can join a thread, so one
    // unverified message keeps the logo off.
    let logos: Vec<Option<String>> = participants
        .iter()
        .map(|p| {
            let mut domains = t.msgs.iter().filter(|m| m.from.email.eq_ignore_ascii_case(&p.email)).map(|m| m.verified.clone().flatten());
            let first = domains.next().flatten()?;
            domains.all(|d| d.as_deref() == Some(first.as_str())).then_some(first)
        })
        .collect();
    let others = participants.len() - 1;
    let sender = match others {
        0 => participants[0].display().to_string(),
        1 => format!("{} and 1 other", participants[0].display()),
        n => format!("{} and {n} others", participants[0].display()),
    };
    MessageObject::new(
        RowData {
            kind: RowKind::Summary,
            thread: t.key.clone(),
            uid: None,
            sender,
            participants,
            date: newest.date,
            subject: newest.subject.clone(),
            preview: newest.preview.clone(),
            count: t.msgs.len(),
            attachment: t.msgs.iter().any(|m| m.flags & ATTACHMENTS != 0),
            important: t.msgs.iter().any(|m| m.important),
            location: None,
            verified: None,
            logos,
        },
        t.msgs.iter().any(|m| m.flags & SEEN == 0),
        t.msgs.iter().any(|m| m.flags & FLAGGED != 0),
    )
    .responded(t.msgs.iter().any(|m| m.flags & ANSWERED != 0), t.msgs.iter().any(|m| m.forwarded))
}

pub fn format_date(ts: i64) -> String {
    let Ok(dt) = glib::DateTime::from_unix_local(ts) else { return String::new() };
    let Ok(now) = glib::DateTime::now_local() else { return String::new() };
    let days = |d: &glib::DateTime| d.ymd();
    let (y, m, d) = days(&dt);
    let (ny, nm, nd) = days(&now);
    let fmt = if (y, m, d) == (ny, nm, nd) {
        crate::clock::time_format()
    } else if now.difference(&dt).as_seconds() < 7 * 86_400 {
        if now.add_days(-1).ok().map(|yd| days(&yd)) == Some((y, m, d)) {
            return "Yesterday".into();
        }
        "%a"
    } else if y == ny {
        "%b %e"
    } else {
        "%b %e, %Y"
    };
    // %e and %l pad single digits with a space.
    dt.format(fmt)
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

impl MessageList {
    /// For a message the summary has no verification result for, reads it
    /// from the copy downloaded for offline use, if there is one, so a
    /// BIMI logo can show on it. Never asks the server.
    fn check_verification(self: &Rc<Self>, row: &MessageObject, widget: &gtk::Widget) {
        let d = row.data();
        if d.verified.is_some() || d.kind == RowKind::Summary || !salutations_core::Settings::get().avatar_bimi {
            return;
        }
        let (Some(uid), Some(person)) = (d.uid.clone(), d.participants.first().cloned()) else { return };
        let Some(folder) = self.state.borrow().folder.clone() else { return };
        let Some(avatar) = find_avatar(widget) else { return };
        if let Some(found) = self.verified_cache.borrow().get(&uid) {
            if let Some(domain) = found {
                crate::people::bind(&avatar, &person.name, &person.email, !crate::people::junk_shown(), Some(domain));
            }
            return;
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let job = SendFolder(folder);
            let u = uid.clone();
            let header = gio::spawn_blocking(move || {
                let SendFolder(folder) = job;
                cached_header(&folder, &u, "Authentication-Results")
            })
            .await
            .ok()
            .flatten();
            let domain = header.and_then(|h| salutations_core::verify::parse(&h).verified_domain(&person.email));
            this.verified_cache.borrow_mut().insert(uid, domain.clone());
            if let Some(domain) = domain
                && avatar.root().is_some()
            {
                crate::people::bind(&avatar, &person.name, &person.email, !crate::people::junk_shown(), Some(&domain));
            }
        });
    }
}

/// A folder handed to a worker thread; Camel folders are thread-safe.
struct SendFolder(camel::Folder);
unsafe impl Send for SendFolder {}

/// The first `name` header of message `uid` from the offline copy only.
fn cached_header(folder: &camel::Folder, uid: &str, name: &str) -> Option<String> {
    use glib::translate::*;
    let msg: Option<camel::MimeMessage> = unsafe {
        from_glib_full(camel::ffi::camel_folder_get_message_cached(
            folder.to_glib_none().0,
            uid.to_glib_none().0,
            std::ptr::null_mut(),
        ))
    };
    msg?.header(name).map(|h| h.to_string())
}

/// The avatar in a row widget.
fn find_avatar(w: &gtk::Widget) -> Option<adw::Avatar> {
    if let Some(a) = w.downcast_ref::<adw::Avatar>() {
        return Some(a.clone());
    }
    let mut c = w.first_child();
    while let Some(child) = c {
        if let Some(a) = find_avatar(&child) {
            return Some(a);
        }
        c = child.next_sibling();
    }
    None
}

/// Avatars for a summary row: newest on top, older ones peeking out below.
/// From `STACK_MAX + 1` participants on, the layers under the top one fade.
/// `logos` holds, by person, the domain whose logo the avatar may show.
pub fn avatar_stack(people: &[Person], logos: &[Option<String>]) -> gtk::Widget {
    let count = people.len().min(STACK_MAX);
    let fade = people.len() > STACK_MAX;
    let step = if count > 1 { (32 / (count as i32 - 1)).min(10) } else { 0 };
    let fixed = gtk::Fixed::new();
    fixed.add_css_class("avatar-stack");
    fixed.set_size_request(40, 32 + step * (count as i32 - 1));
    fixed.set_valign(gtk::Align::Start);
    let opacity = [1.0, 0.55, 0.3, 0.15];
    for k in (0..count).rev() {
        let p = &people[k];
        let logo = logos.get(k).cloned().flatten();
        let av = crate::people::avatar_verified(32, &p.name, &p.email, logo.as_deref());
        av.set_show_initials(k == 0);
        if fade {
            av.set_opacity(opacity[k]);
        }
        fixed.put(&av, 4.0, (k as i32 * step) as f64);
    }
    fixed.upcast()
}

/// The selection checkbox inside a row holder.
fn row_check(holder: &gtk::Widget) -> Option<gtk::Widget> {
    holder.first_child()?.first_child().filter(|c| c.has_css_class("row-check"))
}

pub fn row_widget(row: &MessageObject) -> gtk::Widget {
    let d = row.data();
    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    outer.add_css_class("message-row");
    outer.set_hexpand(true);
    // Shown in selection mode. Drawn only; the row's click gesture decides
    // what a click does.
    let check = gtk::CheckButton::builder().can_target(false).can_focus(false).valign(gtk::Align::Center).build();
    check.add_css_class("row-check");
    row.bind_property("checked", &check, "active").sync_create().build();
    row.bind_property("inconsistent", &check, "inconsistent").sync_create().build();
    outer.append(&check);
    let avatar: gtk::Widget = match d.kind {
        RowKind::Summary => avatar_stack(&d.participants, &d.logos),
        RowKind::Child => {
            outer.add_css_class("child");
            let p = &d.participants[0];
            let av = crate::people::avatar_verified(32, &p.name, &p.email, d.verified.clone().flatten().as_deref());
            // One indent level, after any checkbox.
            av.set_margin_start(28);
            av.upcast()
        }
        RowKind::Single => {
            let p = &d.participants[0];
            crate::people::avatar_verified(40, &p.name, &p.email, d.verified.clone().flatten().as_deref()).upcast()
        }
    };
    avatar.set_valign(gtk::Align::Start);
    outer.append(&avatar);

    let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
    col.set_hexpand(true);
    let line = |left: &gtk::Label, right: Option<&gtk::Widget>| {
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        left.set_hexpand(true);
        left.set_xalign(0.0);
        left.set_ellipsize(gtk::pango::EllipsizeMode::End);
        b.append(left);
        if let Some(r) = right {
            b.append(r);
        }
        b
    };
    let sender = gtk::Label::new(Some(&d.sender));
    sender.add_css_class("sender");
    let date = gtk::Label::new(Some(&format_date(d.date)));
    date.add_css_class("date");
    date.add_css_class("caption");
    date.add_css_class("dimmed");
    let when = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    when.append(&date);
    if d.important {
        let i = gtk::Image::from_icon_name("salutations-important-symbolic");
        i.set_tooltip_text(Some("High importance"));
        i.add_css_class("important");
        when.append(&i);
    }
    col.append(&line(&sender, Some(when.upcast_ref())));

    if d.kind != RowKind::Child {
        let subject = gtk::Label::new(Some(if d.subject.is_empty() { "(No subject)" } else { &d.subject }));
        subject.add_css_class("subject");
        let badge = (d.kind == RowKind::Summary).then(|| {
            let b = gtk::Label::new(Some(&d.count.to_string()));
            b.add_css_class("thread-count");
            b.set_valign(gtk::Align::Center);
            b.upcast::<gtk::Widget>()
        });
        col.append(&line(&subject, badge.as_ref()));
    }

    let preview = gtk::Label::new(Some(&d.preview));
    preview.add_css_class("preview");
    preview.add_css_class("caption");
    let icons = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    if d.attachment {
        let i = gtk::Image::from_icon_name("mail-attachment-symbolic");
        i.set_tooltip_text(Some("Has attachments"));
        icons.append(&i);
    }
    for (icon, tip, property) in [("mail-replied-symbolic", "Replied", "answered"), ("mail-forward-symbolic", "Forwarded", "forwarded")] {
        let i = gtk::Image::from_icon_name(icon);
        i.set_tooltip_text(Some(tip));
        row.bind_property(property, &i, "visible").sync_create().build();
        icons.append(&i);
    }
    let star = gtk::Image::from_icon_name("starred-symbolic");
    star.set_tooltip_text(Some("Starred"));
    row.bind_property("starred", &star, "visible").sync_create().build();
    icons.append(&star);
    icons.add_css_class("dimmed");
    icons.add_css_class("row-icons");
    col.append(&line(&preview, Some(icons.upcast_ref())));
    if let Some(loc) = &d.location {
        let l = gtk::Label::builder()
            .label(loc)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        l.add_css_class("caption");
        l.add_css_class("dimmed");
        col.append(&l);
    }
    outer.append(&col);

    let w: gtk::Widget = outer.upcast();
    set_unread_class(&w, row.unread());
    w
}

pub fn set_unread_class(w: &gtk::Widget, unread: bool) {
    if unread {
        w.add_css_class("unread");
    } else {
        w.remove_css_class("unread");
    }
}
