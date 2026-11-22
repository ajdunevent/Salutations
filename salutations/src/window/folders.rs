//! Creating, renaming, moving, and deleting folders, from the folder
//! pane's menus, drag and drop, and Preferences.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use salutations_core::{Settings, origins, rules};

use super::{AccountCtl, Window, folder_choices};
use crate::folder_object::FolderObject;
use crate::manage::{self, FolderChoice};
use crate::folder_pane::DraggedFolder;

/// A folder's last name in its full name.
fn leaf(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, l)| l)
}

fn parent_of(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(p, _)| p)
}

fn joined(parent: Option<&str>, name: &str) -> String {
    match parent.filter(|p| !p.is_empty()) {
        Some(p) => format!("{p}/{name}"),
        None => name.to_string(),
    }
}

/// The folders under `obj`, at any depth.
fn descendants(obj: &FolderObject) -> Vec<FolderObject> {
    let mut out = Vec::new();
    let store = obj.children();
    for i in 0..store.n_items() {
        if let Some(c) = store.item(i).and_downcast::<FolderObject>() {
            out.push(c.clone());
            out.extend(descendants(&c));
        }
    }
    out
}

type FolderAction = Box<dyn Fn(&Rc<Window>)>;

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

impl Window {
    /// Whether the account's folder `path` can be renamed, moved, or
    /// deleted: not Inbox and not a special folder.
    pub(super) fn changeable(ctl: &AccountCtl, path: &str) -> bool {
        !path.eq_ignore_ascii_case("INBOX") && ctl.eng.special().is_none_or(|s| s.role_of(path).is_none())
    }

    pub(super) fn ctl_of(&self, account: &str) -> Option<Rc<AccountCtl>> {
        self.accounts.borrow().iter().find(|c| c.account.uid == account).cloned()
    }

    /// The menu of a folder or account row.
    pub(super) fn folder_menu(self: &Rc<Self>, obj: &FolderObject, widget: &gtk::Widget, x: f64, y: f64) {
        let Some(ctl) = self.ctl_of(&obj.account_uid()) else { return };
        let path = obj.full_name();
        let menu = gio::Menu::new();
        let group = gio::SimpleActionGroup::new();
        let add = |name: &str, label: &str, section: &gio::Menu, f: FolderAction| {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    f(&this);
                }
            });
            group.add_action(&action);
            section.append(Some(label), Some(&format!("folder.{name}")));
        };
        let edit = gio::Menu::new();
        let (c, p) = (ctl.clone(), path.clone());
        add(
            "new",
            "New folder…",
            &edit,
            Box::new(move |this| {
                let (this, c, p) = (this.clone(), c.clone(), p.clone());
                glib::spawn_future_local(async move {
                    this.new_folder(&c, (!p.is_empty()).then_some(p.as_str())).await;
                });
            }),
        );
        if !obj.is_account() && Self::changeable(&ctl, &path) {
            let (c, p) = (ctl.clone(), path.clone());
            add("rename", "Rename…", &edit, Box::new(move |this| this.rename_folder(&c, &p)));
            let (c, p) = (ctl.clone(), path.clone());
            add("move", "Move…", &edit, Box::new(move |this| this.move_folder(&c, &p)));
            let (c, p) = (ctl.clone(), path.clone());
            add("delete", "Delete…", &edit, Box::new(move |this| this.delete_folder(&c, &p)));
        }
        menu.append_section(None, &edit);
        if let Some(name) = Self::emptiable(&ctl, &path) {
            let empty = gio::Menu::new();
            let (c, p) = (ctl.clone(), path.clone());
            add("empty", &format!("Empty {name}…"), &empty, Box::new(move |this| this.empty_folder(&c, &p)));
            menu.append_section(None, &empty);
        }
        if !obj.is_account() {
            let settings = gio::Menu::new();
            let (uid, p) = (ctl.account.uid.clone(), path.clone());
            add("settings", "Folder settings…", &settings, Box::new(move |this| this.show_preferences_at(Some(&uid), Some(&p))));
            menu.append_section(None, &settings);
        }
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

    /// Asks for a folder name; None when cancelled. `taken` refuses names
    /// already used beside it.
    async fn ask_folder_name(&self, heading: &str, initial: &str, accept: &str, taken: Vec<String>) -> Option<String> {
        let entry = adw::EntryRow::builder().title("Name").text(initial).activates_default(true).build();
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        list.append(&entry);
        let note = gtk::Label::builder().wrap(true).xalign(0.0).css_classes(["dim-label", "caption"]).visible(false).build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.append(&list);
        content.append(&note);
        let dialog = adw::AlertDialog::builder().heading(heading).extra_child(&content).build();
        dialog.add_responses(&[("cancel", "_Cancel"), ("accept", accept)]);
        dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("accept"));
        dialog.set_close_response("cancel");
        let check = {
            let (dialog, note) = (dialog.clone(), note.clone());
            let initial = initial.to_string();
            move |text: &str| {
                let name = text.trim();
                let problem = if name.contains('/') {
                    Some("Names can't contain \u{201c}/\u{201d}")
                } else if name != initial && taken.iter().any(|t| t.eq_ignore_ascii_case(name)) {
                    Some("A folder with this name is already here")
                } else {
                    None
                };
                note.set_label(problem.unwrap_or(""));
                note.set_visible(problem.is_some());
                dialog.set_response_enabled("accept", !name.is_empty() && name != initial && problem.is_none());
            }
        };
        check(initial);
        entry.connect_changed(move |e| check(&e.text()));
        entry.grab_focus();
        let response = dialog.choose_future(Some(&self.window)).await;
        (response == "accept").then(|| entry.text().trim().to_string())
    }

    /// The names of the folders directly under `parent`, or at the top.
    fn sibling_names(ctl: &AccountCtl, parent: Option<&str>) -> Vec<String> {
        let store = match parent.and_then(|p| ctl.obj.find(p)) {
            Some(o) => o.children().clone(),
            None => ctl.obj.children().clone(),
        };
        (0..store.n_items()).filter_map(|i| store.item(i).and_downcast::<FolderObject>()).map(|o| leaf(&o.full_name()).to_string()).collect()
    }

    /// Asks for a name and creates a folder under `parent`, or at the
    /// top. Its full name once made.
    pub(super) async fn new_folder(self: &Rc<Self>, ctl: &Rc<AccountCtl>, parent: Option<&str>) -> Option<String> {
        let heading = match parent.and_then(|p| ctl.obj.find(p)) {
            Some(o) => format!("New folder in {}", o.title()),
            None => "New folder".to_string(),
        };
        let name = self.ask_folder_name(&heading, "", "C_reate", Self::sibling_names(ctl, parent)).await?;
        let store = self.engine.store_for(&ctl.eng).ok()?;
        let (p, n) = (parent.map(str::to_string), name.clone());
        let made = gio::spawn_blocking(move || store.create_folder_sync(p.as_deref(), &n, None::<&gio::Cancellable>))
            .await
            .unwrap_or_else(|_| Err(glib::Error::new(gio::IOErrorEnum::Failed, "Folder creation failed")));
        if let Err(e) = made {
            self.toast(&format!("Cannot create {name}: {}", e.message()), false);
            return None;
        }
        self.engine.refresh_folders(&ctl.eng).await;
        crate::folder_prefs::folders_changed(&ctl.account.uid);
        Some(joined(parent, &name))
    }

    /// The move picker: the chosen folder's account, full name, and path,
    /// or a new folder made at the top of `source`'s account.
    pub(super) async fn pick_destination(
        self: &Rc<Self>,
        title: &str,
        choices: Vec<FolderChoice>,
        source: &Rc<AccountCtl>,
    ) -> Option<(Rc<AccountCtl>, String, String)> {
        match manage::pick_folder_or_new(&self.window, title, choices).await? {
            manage::Picked::Folder(dest) => {
                let account = self.ctl_of(&dest.account)?;
                Some((account, dest.full_name, dest.path))
            }
            manage::Picked::New => {
                let made = self.new_folder(source, None).await?;
                let path = source.obj.find(&made).map_or_else(|| made.clone(), |o| o.title());
                Some((source.clone(), made, path))
            }
        }
    }

    pub(super) fn rename_folder(self: &Rc<Self>, ctl: &Rc<AccountCtl>, path: &str) {
        let (this, ctl, path) = (self.clone(), ctl.clone(), path.to_string());
        glib::spawn_future_local(async move {
            let parent = parent_of(&path);
            let taken = Self::sibling_names(&ctl, parent);
            let Some(name) = this.ask_folder_name("Rename folder", leaf(&path), "_Rename", taken).await else { return };
            this.rename_to(&ctl, &path, &name).await;
        });
    }

    /// Renames the folder `path` to `name` in the same place.
    pub(super) async fn rename_to(self: &Rc<Self>, ctl: &Rc<AccountCtl>, path: &str, name: &str) -> bool {
        let new = joined(parent_of(path), name);
        if new == path {
            return true;
        }
        if let Err(e) = self.relocate(ctl, path, &new).await {
            self.toast(&format!("Cannot rename {}: {}", leaf(path), e.message()), false);
            return false;
        }
        true
    }

    /// Renames or moves the folder `old` to `new`, then follows it
    /// wherever Salutations keeps its name.
    async fn relocate(self: &Rc<Self>, ctl: &Rc<AccountCtl>, old: &str, new: &str) -> Result<(), glib::Error> {
        let store = self.engine.store_for(&ctl.eng)?;
        let reopen = self
            .open
            .borrow()
            .as_ref()
            .filter(|o| Rc::ptr_eq(&o.account, ctl))
            .and_then(|o| salutations_core::renamed_path(&o.obj.full_name(), old, new));
        store.rename_folder(old, new).await?;
        let uid = &ctl.account.uid;
        let mut st = Settings::get();
        st.folder_renamed(uid, old, new);
        if let Err(e) = st.save() {
            glib::g_warning!("salutations", "Could not save settings: {}", e);
        }
        if let Err(e) = rules::folder_renamed(uid, old, new) {
            glib::g_warning!("salutations", "Could not save rules: {}", e);
        }
        origins::folder_renamed(uid, old, new);
        if let Some(path) = reopen {
            self.show_folder(ctl, &path).await;
        }
        crate::folder_prefs::folders_changed(uid);
        Ok(())
    }

    /// Picks where the folder `path` goes and moves it there.
    pub(super) fn move_folder(self: &Rc<Self>, ctl: &Rc<AccountCtl>, path: &str) {
        let (this, ctl, path) = (self.clone(), ctl.clone(), path.to_string());
        glib::spawn_future_local(async move {
            let here = parent_of(&path).unwrap_or("").to_string();
            let mut choices = vec![FolderChoice {
                account: ctl.account.uid.clone(),
                full_name: String::new(),
                path: ctl.account.display_name(),
                title: "Top level".to_string(),
                icon: "go-top-symbolic".to_string(),
                depth: 0,
                heading: false,
            }];
            choices.extend(
                folder_choices(&ctl, &[])
                    .into_iter()
                    .filter(|c| !salutations_core::within(&c.full_name, &path))
                    .map(|mut c| {
                        c.depth += 1;
                        c
                    }),
            );
            choices.retain(|c| c.full_name != here);
            let title = format!("Move {}", ctl.obj.find(&path).map(|o| o.title()).unwrap_or_else(|| leaf(&path).to_string()));
            let Some(dest) = manage::pick_folder(&this.window, &title, choices).await else { return };
            this.move_folder_to(&ctl, &path, &dest.full_name).await;
        });
    }

    /// Moves the folder `path` under `parent` (empty: the top level),
    /// with a toast that can undo it.
    pub(super) async fn move_folder_to(self: &Rc<Self>, ctl: &Rc<AccountCtl>, path: &str, parent: &str) {
        let name = leaf(path).to_string();
        if Self::sibling_names(ctl, (!parent.is_empty()).then_some(parent)).iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            self.toast(&format!("Cannot move {name}: a folder with this name is already there"), false);
            return;
        }
        let new = joined(Some(parent), &name);
        if let Err(e) = self.relocate(ctl, path, &new).await {
            self.toast(&format!("Cannot move {name}: {}", e.message()), false);
            return;
        }
        let place = ctl.obj.find(parent).map(|o| o.title()).unwrap_or_else(|| "the top level".to_string());
        let toast = adw::Toast::builder().title(format!("Moved {name} to {place}")).button_label("_Undo").build();
        let (weak, ctl, old) = (Rc::downgrade(self), ctl.clone(), path.to_string());
        toast.connect_button_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            let (ctl, old, new) = (ctl.clone(), old.clone(), new.clone());
            glib::spawn_future_local(async move {
                if let Err(e) = this.relocate(&ctl, &new, &old).await {
                    this.toast(&format!("Cannot move {} back: {}", leaf(&old), e.message()), false);
                }
            });
        });
        self.toasts.add_toast(toast);
    }

    /// A folder dropped on `target`: under that folder, or at the top
    /// for an account row.
    pub(super) fn folder_dropped(self: &Rc<Self>, target: &FolderObject, f: DraggedFolder) {
        let Some(ctl) = self.ctl_of(&f.account_uid) else { return };
        if !Self::changeable(&ctl, &f.full_name) {
            return;
        }
        let parent = if target.is_account() { String::new() } else { target.full_name() };
        let this = self.clone();
        glib::spawn_future_local(async move { this.move_folder_to(&ctl, &f.full_name, &parent).await });
    }

    /// Asks, then moves every message in the folder and its subfolders to
    /// Trash and deletes them; in Trash, or without one, deletes for good.
    pub(super) fn delete_folder(self: &Rc<Self>, ctl: &Rc<AccountCtl>, path: &str) {
        let Some(obj) = ctl.obj.find(path) else { return };
        let (this, ctl, path) = (self.clone(), ctl.clone(), path.to_string());
        glib::spawn_future_local(async move {
            let name = obj.title();
            let below = descendants(&obj).len();
            // Counts in the folder tree are not reliable for folders never
            // opened, so the messages are not counted here.
            let trash = ctl.eng.special().and_then(|s| s.trash).filter(|t| !salutations_core::within(&path, t));
            let what = match below {
                0 => name.clone(),
                1 => format!("{name} and its subfolder"),
                n => format!("{name} and its {n} subfolders"),
            };
            let (them, in_them) = if below == 0 { ("the folder", "it") } else { ("the folders", "them") };
            let body = match trash {
                Some(_) => format!("The messages in {what} will be moved to Trash, then {them} deleted."),
                None => format!("{what} will be deleted permanently, with the messages in {in_them}. This cannot be undone."),
            };
            let dialog = adw::AlertDialog::builder().heading(format!("Delete \u{201c}{name}\u{201d}?")).body(body).build();
            dialog.add_responses(&[("cancel", "_Cancel"), ("delete", "_Delete")]);
            dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            if dialog.choose_future(Some(&this.window)).await != "delete" {
                return;
            }
            let showing = this.open.borrow().as_ref().is_some_and(|o| Rc::ptr_eq(&o.account, &ctl) && salutations_core::within(&o.obj.full_name(), &path));
            if showing {
                this.show_folder(&ctl, "INBOX").await;
            }
            let busy = adw::Toast::builder().title(format!("Deleting {name}…")).timeout(0).build();
            this.toasts.add_toast(busy.clone());
            let Ok(store) = this.engine.store_for(&ctl.eng) else { return };
            let result = store.delete_folder(&path).await;
            busy.dismiss();
            match result {
                Ok(moved) => {
                    let mut st = Settings::get();
                    st.folder_deleted(&ctl.account.uid, &path);
                    if let Err(e) = st.save() {
                        glib::g_warning!("salutations", "Could not save settings: {}", e);
                    }
                    let text = match moved {
                        0 => format!("Deleted {name}"),
                        n => format!("Deleted {name}; {} moved to Trash", plural(n as usize, "message", "messages")),
                    };
                    this.toast(&text, false);
                }
                Err(e) => this.toast(&format!("Cannot delete {name}: {}", e.message()), false),
            }
            crate::folder_prefs::folders_changed(&ctl.account.uid);
        });
    }
}

fn items(store: &gio::ListStore) -> Vec<crate::folder_prefs::FolderItem> {
    (0..store.n_items())
        .filter_map(|i| store.item(i).and_downcast::<FolderObject>())
        .map(|o| crate::folder_prefs::FolderItem {
            full_name: o.full_name(),
            title: o.title(),
            icon: o.icon_name(),
            children: items(o.children()),
        })
        .collect()
}

impl Window {
    /// What Preferences' Folders pages read and ask for.
    pub(super) fn folder_hooks(self: &Rc<Self>) -> crate::folder_prefs::FolderHooks {
        use crate::folder_prefs::FolderOp;
        let w = Rc::downgrade(self);
        let tree = Rc::new(move |uid: &str| w.upgrade().and_then(|t| t.ctl_of(uid)).map(|c| items(c.obj.children())).unwrap_or_default());
        let w = Rc::downgrade(self);
        let special = Rc::new(move |uid: &str| w.upgrade().and_then(|t| t.ctl_of(uid)).and_then(|c| c.eng.special()).unwrap_or_default());
        let w = Rc::downgrade(self);
        let store = Rc::new(move |uid: &str| w.upgrade().and_then(|t| t.ctl_of(uid)).and_then(|c| c.eng.store()));
        let registry = self.registry.clone();
        let w = Rc::downgrade(self);
        let specific = Rc::new(move |uid: &str| {
            let Some(c) = w.upgrade().and_then(|t| t.ctl_of(uid)) else { return false };
            salutations_core::offline::read(&registry, &c.account).is_ok_and(|s| !s.all_folders)
        });
        let w = Rc::downgrade(self);
        let act = Rc::new(move |op: FolderOp| {
            let Some(this) = w.upgrade() else { return };
            let account = match &op {
                FolderOp::New { account, .. }
                | FolderOp::Rename { account, .. }
                | FolderOp::Move { account, .. }
                | FolderOp::Delete { account, .. }
                | FolderOp::SetRole { account, .. } => account.clone(),
            };
            let Some(ctl) = this.ctl_of(&account) else { return };
            match op {
                FolderOp::New { parent, .. } => {
                    glib::spawn_future_local(async move {
                        this.new_folder(&ctl, parent.as_deref()).await;
                    });
                }
                FolderOp::Rename { path, name, .. } => {
                    glib::spawn_future_local(async move {
                        this.rename_to(&ctl, &path, &name).await;
                    });
                }
                FolderOp::Move { path, .. } => this.move_folder(&ctl, &path),
                FolderOp::Delete { path, .. } => this.delete_folder(&ctl, &path),
                FolderOp::SetRole { path, role, .. } => this.set_role(&ctl, &path, role),
            }
        });
        crate::folder_prefs::FolderHooks { tree, special, store, specific, act }
    }

    /// Gives the folder `path` the role `role` (None: no role), taking it
    /// from the folder that had it.
    fn set_role(self: &Rc<Self>, ctl: &Rc<AccountCtl>, path: &str, role: Option<salutations_core::Role>) {
        let mut special = ctl.eng.special().unwrap_or_default();
        if let Some(old) = special.role_of(path) {
            special.set(old, None);
        }
        if let Some(role) = role {
            special.set(role, Some(path.to_string()));
        }
        self.engine.set_special(&ctl.eng, special);
        let (this, ctl) = (self.clone(), ctl.clone());
        glib::spawn_future_local(async move {
            // The tree takes the new icons and order.
            this.engine.refresh_folders(&ctl.eng).await;
            crate::folder_prefs::folders_changed(&ctl.account.uid);
        });
    }
}
