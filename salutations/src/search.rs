//! The search bar and its results. The window runs the searches; this
//! module shows the query, the scope and the hits.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use salutations_core::camel::{self, prelude::*};

use crate::message_list::{hit_row, row_widget, set_unread_class};
use crate::message_object::MessageObject;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Folder,
    Account,
    All,
}

type QueryHandler = Rc<dyn Fn(String, Scope)>;
type OpenHandler = Rc<dyn Fn(camel::Folder, String)>;

pub struct Search {
    pub bar: gtk::SearchBar,
    pub entry: gtk::SearchEntry,
    scope: adw::ToggleGroup,
    pub results: gtk::ScrolledWindow,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    /// Result key (see [`key`]) to its folder and UID.
    hits: RefCell<HashMap<String, (camel::Folder, String)>>,
    on_query: RefCell<Option<QueryHandler>>,
    on_open: RefCell<Option<OpenHandler>>,
    suppress: Cell<bool>,
    /// Selection mode: rows show checkboxes and a tap checks.
    select_mode: Rc<Cell<bool>>,
    /// The bound rows' checkboxes, shown in selection mode.
    checks: Rc<RefCell<Vec<glib::WeakRef<gtk::Widget>>>>,
    on_checked: RefCell<Option<Rc<dyn Fn()>>>,
}

/// Identifies a result across folders and accounts.
pub fn key(account: &str, folder: &str, uid: &str) -> String {
    format!("{account}\u{1f}{folder}\u{1f}{uid}")
}

impl Search {
    pub fn new() -> Rc<Self> {
        let entry = gtk::SearchEntry::builder()
            .placeholder_text("Search mail")
            .hexpand(true)
            .build();
        // Shorter labels when the full ones do not fit; both groups share
        // the choice.
        let group = |labels: [&str; 3]| {
            let g = adw::ToggleGroup::new();
            for (name, label) in ["folder", "account", "all"].into_iter().zip(labels) {
                g.add(adw::Toggle::builder().name(name).label(label).build());
            }
            g.set_active_name(Some("folder"));
            g.add_css_class("round");
            g.set_homogeneous(true);
            g
        };
        let scope = group(["This folder", "This account", "All accounts"]);
        let short = group(["Folder", "Account", "All"]);
        scope.bind_property("active-name", &short, "active-name").bidirectional().sync_create().build();
        let scopes = crate::fit_stack::FitStack::new(vec![scope.clone().upcast(), short.clone().upcast()]);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
        column.append(&entry);
        column.append(&scopes);
        let clamp = adw::Clamp::builder().maximum_size(520).child(&column).build();
        let bar = gtk::SearchBar::builder().child(&clamp).show_close_button(false).build();
        bar.connect_entry(&entry);

        let store = gio::ListStore::new::<MessageObject>();
        let sorted = gtk::SortListModel::new(
            Some(store.clone()),
            Some(gtk::CustomSorter::new(|a, b| {
                let da = a.downcast_ref::<MessageObject>().map_or(0, |o| o.data().date);
                let db = b.downcast_ref::<MessageObject>().map_or(0, |o| o.data().date);
                db.cmp(&da).into()
            })),
        );
        let selection = gtk::SingleSelection::builder()
            .model(&sorted)
            .autoselect(false)
            .can_unselect(true)
            .build();
        let view = gtk::ListView::builder().model(&selection).build();
        let factory = gtk::SignalListItemFactory::new();
        let select_mode: Rc<Cell<bool>> = Rc::default();
        let checks: Rc<RefCell<Vec<glib::WeakRef<gtk::Widget>>>> = Rc::default();
        let (selecting, bound) = (select_mode.clone(), checks.clone());
        factory.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
            let Some(row) = item.item().and_downcast::<MessageObject>() else { return };
            let widget = row_widget(&row);
            if let Some(check) = widget.first_child().filter(|c| c.has_css_class("row-check")) {
                check.set_visible(selecting.get());
                let mut bound = bound.borrow_mut();
                bound.retain(|w| w.upgrade().is_some());
                bound.push(check.downgrade());
            }
            let w = widget.downgrade();
            row.connect_unread_notify(move |r| {
                if let Some(w) = w.upgrade() {
                    set_unread_class(&w, r.unread());
                }
            });
            item.set_child(Some(&widget));
        });
        view.set_factory(Some(&factory));
        let empty = adw::StatusPage::builder()
            .icon_name("system-search-symbolic")
            .title("No results")
            .build();
        empty.add_css_class("compact");
        let stack = gtk::Stack::new();
        stack.add_named(&view, Some("list"));
        stack.add_named(&empty, Some("empty"));
        let results = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&stack)
            .build();
        let s = stack.clone();
        sorted.connect_items_changed(move |m, _, _, _| {
            s.set_visible_child_name(if m.n_items() == 0 { "empty" } else { "list" });
        });
        stack.set_visible_child_name("empty");

        let this = Rc::new(Self {
            bar,
            entry,
            scope,
            results,
            store,
            selection,
            hits: RefCell::new(HashMap::new()),
            on_query: RefCell::new(None),
            on_open: RefCell::new(None),
            suppress: Cell::new(false),
            select_mode,
            checks,
            on_checked: RefCell::new(None),
        });
        let weak = Rc::downgrade(&this);
        this.entry.connect_search_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.emit_query();
            }
        });
        let weak = Rc::downgrade(&this);
        this.scope.connect_active_name_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.emit_query();
            }
        });
        let weak = Rc::downgrade(&this);
        this.selection.connect_selected_notify(move |sel| {
            let Some(this) = weak.upgrade() else { return };
            if this.suppress.get() {
                return;
            }
            let Some(row) = sel.selected_item().and_downcast::<MessageObject>() else { return };
            if this.select_mode.get() {
                // A tap checks or unchecks; nothing opens.
                row.set_checked(!row.checked());
                let this = this.clone();
                glib::idle_add_local_once(move || {
                    this.unselect();
                    this.emit_checked();
                });
                return;
            }
            let hit = this.hits.borrow().get(&row.data().thread).cloned();
            let cb = this.on_open.borrow().clone();
            if let (Some((folder, uid)), Some(cb)) = (hit, cb) {
                cb(folder, uid);
            }
        });
        this
    }

    pub fn query(&self) -> String {
        self.entry.text().trim().to_string()
    }

    pub fn scope(&self) -> Scope {
        match self.scope.active_name().as_deref() {
            Some("account") => Scope::Account,
            Some("all") => Scope::All,
            _ => Scope::Folder,
        }
    }

    fn emit_query(&self) {
        let cb = self.on_query.borrow().clone();
        if let Some(cb) = cb {
            cb(self.query(), self.scope());
        }
    }

    /// Called after the text or the scope changes.
    pub fn connect_query(&self, f: impl Fn(String, Scope) + 'static) {
        *self.on_query.borrow_mut() = Some(Rc::new(f));
    }

    /// Called with a result the user picked.
    pub fn connect_open(&self, f: impl Fn(camel::Folder, String) + 'static) {
        *self.on_open.borrow_mut() = Some(Rc::new(f));
    }

    pub fn clear(&self) {
        let had = !self.checked().is_empty();
        self.hits.borrow_mut().clear();
        self.suppress.set(true);
        self.store.remove_all();
        self.suppress.set(false);
        if had {
            self.emit_checked();
        }
    }

    fn rows(&self) -> Vec<MessageObject> {
        (0..self.store.n_items()).filter_map(|i| self.store.item(i).and_downcast::<MessageObject>()).collect()
    }


    /// Shows or hides the checkboxes; leaving selection mode unchecks all.
    pub fn set_select_mode(&self, on: bool) {
        self.select_mode.set(on);
        if !on {
            for r in self.rows() {
                r.set_checked(false);
            }
        }
        for c in self.checks.borrow().iter().filter_map(|w| w.upgrade()) {
            c.set_visible(on);
        }
    }

    /// The checked results, as folders and UIDs.
    pub fn checked(&self) -> Vec<(camel::Folder, String)> {
        let hits = self.hits.borrow();
        self.rows().iter().filter(|r| r.checked()).filter_map(|r| hits.get(&r.data().thread).cloned()).collect()
    }

    pub fn set_all_checked(&self, on: bool) {
        for r in self.rows() {
            r.set_checked(on);
        }
        self.emit_checked();
    }

    /// Checks exactly the results of `kind`.
    pub fn check_kind(&self, kind: crate::message_list::Kind) {
        for r in self.rows() {
            r.set_checked(kind.matches(r.unread(), r.starred()));
        }
        self.emit_checked();
    }

    /// Called when the checked results change.
    pub fn connect_checked(&self, f: impl Fn() + 'static) {
        *self.on_checked.borrow_mut() = Some(Rc::new(f));
    }

    fn emit_checked(&self) {
        let cb = self.on_checked.borrow().clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    /// While results are checked the query and scope cannot change.
    pub fn set_locked(&self, locked: bool) {
        self.entry.set_sensitive(!locked);
        // Both versions sit in one parent; disabling it covers both.
        if let Some(p) = self.scope.parent() {
            p.set_sensitive(!locked);
        }
    }

    /// Adds results not shown yet. `location` labels them with their
    /// folder when the scope goes beyond the open one.
    pub fn add(&self, account: &str, folder: &camel::Folder, uids: &[String], location: Option<&str>) {
        let name = folder.full_name().map(|n| n.to_string()).unwrap_or_default();
        let mut rows = Vec::new();
        {
            let mut hits = self.hits.borrow_mut();
            for uid in uids {
                let k = key(account, &name, uid);
                if hits.contains_key(&k) {
                    continue;
                }
                let Some(info) = folder.message_info(uid) else { continue };
                let Some(row) = hit_row(&info, &k, location.map(str::to_string)) else { continue };
                hits.insert(k, (folder.clone(), uid.clone()));
                rows.push(row);
            }
        }
        self.suppress.set(true);
        self.store.extend_from_slice(&rows);
        self.suppress.set(false);
    }

    /// Drops results that left their folder.
    pub fn remove(&self, folder: &camel::Folder, uids: &[String]) {
        let gone: Vec<String> = self
            .hits
            .borrow()
            .iter()
            .filter(|(_, (f, u))| f == folder && uids.contains(u))
            .map(|(k, _)| k.clone())
            .collect();
        self.hits.borrow_mut().retain(|k, _| !gone.contains(k));
        let checked_gone = self.rows().iter().any(|r| r.checked() && gone.contains(&r.data().thread));
        self.suppress.set(true);
        self.store.retain(|o| {
            o.downcast_ref::<MessageObject>()
                .is_none_or(|m| !gone.contains(&m.data().thread))
        });
        self.suppress.set(false);
        if checked_gone {
            self.emit_checked();
        }
    }

    /// Updates read and star state after flag changes.
    pub fn refresh_flags(&self) {
        let hits = self.hits.borrow();
        for i in 0..self.store.n_items() {
            let Some(row) = self.store.item(i).and_downcast::<MessageObject>() else { continue };
            if let Some((folder, uid)) = hits.get(&row.data().thread) {
                let flags = folder.message_flags(uid);
                row.set_unread(flags & camel::MessageFlags::SEEN.bits() == 0);
                row.set_starred(flags & camel::MessageFlags::FLAGGED.bits() != 0);
            }
        }
    }

    pub fn unselect(&self) {
        self.suppress.set(true);
        self.selection.set_selected(gtk::INVALID_LIST_POSITION);
        self.suppress.set(false);
    }
}
