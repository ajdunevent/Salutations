//! Preferences → an account → Folders: each folder's page with its name,
//! offline use, notifications, visibility, and role, walked one level per
//! page.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use salutations_core::{Role, Settings, SpecialFolders};

use crate::mail::camel;

/// A folder as the Folders pages show it.
#[derive(Clone, Debug)]
pub struct FolderItem {
    pub full_name: String,
    pub title: String,
    pub icon: String,
    /// Hidden by itself; its subfolders are hidden with it.
    pub hidden: bool,
    pub children: Vec<FolderItem>,
}

/// What a Folders page asks the window to do.
pub enum FolderOp {
    New { account: String, parent: Option<String> },
    Rename { account: String, path: String, name: String },
    Move { account: String, path: String },
    Delete { account: String, path: String },
    SetRole { account: String, path: String, role: Option<Role> },
    SetHidden { account: String, path: String, hidden: bool },
}

type Tree = Rc<dyn Fn(&str) -> Vec<FolderItem>>;
type StoreOf = Rc<dyn Fn(&str) -> Option<camel::Store>>;
type OpenPage = (String, glib::WeakRef<adw::NavigationPage>, Rc<dyn Fn()>);

/// What the Folders pages need from the window.
#[derive(Clone)]
pub struct FolderHooks {
    /// The account's folder tree.
    pub tree: Tree,
    pub special: Rc<dyn Fn(&str) -> SpecialFolders>,
    /// The account's store, once it has been opened.
    pub store: StoreOf,
    /// Whether the account downloads specific folders rather than all.
    pub specific: Rc<dyn Fn(&str) -> bool>,
    pub act: Rc<dyn Fn(FolderOp)>,
}

thread_local! {
    static PAGES: RefCell<Vec<OpenPage>> = const { RefCell::new(Vec::new()) };
}

/// Rebuilds the account's open Folders pages after its folders changed.
pub fn folders_changed(account: &str) {
    let refills: Vec<Rc<dyn Fn()>> = PAGES.with(|p| {
        let mut p = p.borrow_mut();
        p.retain(|(_, page, _)| page.upgrade().is_some());
        p.iter().filter(|(a, ..)| a == account).map(|(.., f)| f.clone()).collect()
    });
    for f in refills {
        f();
    }
}

fn register(account: &str, page: &adw::NavigationPage, refill: Rc<dyn Fn()>) {
    PAGES.with(|p| p.borrow_mut().push((account.to_string(), page.downgrade(), refill)));
}

fn find<'a>(items: &'a [FolderItem], path: &str) -> Option<&'a FolderItem> {
    items.iter().find_map(|i| if i.full_name == path { Some(i) } else { find(&i.children, path) })
}

/// The hidden folder that hides `path`, other than itself.
fn hidden_above<'a>(items: &'a [FolderItem], path: &str) -> Option<&'a FolderItem> {
    let parts: Vec<&str> = path.split('/').collect();
    (1..parts.len()).filter_map(|i| find(items, &parts[..i].join("/"))).find(|i| i.hidden)
}

fn folder_row(item: &FolderItem, hidden: bool) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(glib::markup_escape_text(&item.title)).activatable(true).build();
    row.add_prefix(&gtk::Image::from_icon_name(&item.icon));
    if !item.children.is_empty() {
        let n = item.children.len();
        row.set_subtitle(&if n == 1 { "1 subfolder".to_string() } else { format!("{n} subfolders") });
    }
    if hidden {
        let icon = gtk::Image::from_icon_name("view-conceal-symbolic");
        icon.set_tooltip_text(Some("Hidden from the sidebar"));
        row.add_suffix(&icon);
        row.add_css_class("dim-label");
    }
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row
}

/// The rows of `items`, each opening its folder's page, then New folder.
/// `hidden` says the parent is hidden, hiding these too.
fn folder_list(
    dialog: &adw::PreferencesDialog,
    account: &str,
    hooks: &FolderHooks,
    items: &[FolderItem],
    parent: Option<&str>,
    title: &str,
    hidden: bool,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).build();
    for item in items {
        let row = folder_row(item, hidden || item.hidden);
        let (dialog, account, hooks, path) = (dialog.downgrade(), account.to_string(), hooks.clone(), item.full_name.clone());
        row.connect_activated(move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.push_subpage(&folder_page(&dialog, &account, &path, &hooks));
            }
        });
        group.add(&row);
    }
    let new = adw::ButtonRow::builder()
        .title(if parent.is_some() { "_New subfolder…" } else { "_New folder…" })
        .use_underline(true)
        .start_icon_name("folder-new-symbolic")
        .build();
    let (act, account, parent) = (hooks.act.clone(), account.to_string(), parent.map(str::to_string));
    new.connect_activated(move |_| act(FolderOp::New { account: account.clone(), parent: parent.clone() }));
    group.add(&new);
    group
}

/// The account's top-level folders.
pub fn root_page(dialog: &adw::PreferencesDialog, account: &str, hooks: &FolderHooks) -> adw::NavigationPage {
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    let page = adw::NavigationPage::builder().title("Folders").child(&view).build();
    let fill = {
        let (dialog, account, hooks, view) = (dialog.downgrade(), account.to_string(), hooks.clone(), view.downgrade());
        Rc::new(move || {
            let (Some(dialog), Some(view)) = (dialog.upgrade(), view.upgrade()) else { return };
            let content = adw::PreferencesPage::new();
            let tree = (hooks.tree)(&account);
            content.add(&folder_list(&dialog, &account, &hooks, &tree, None, "", false));
            view.set_content(Some(&content));
        })
    };
    fill();
    register(account, &page, fill);
    page
}

const ROLES: [Option<Role>; 6] = [None, Some(Role::Sent), Some(Role::Drafts), Some(Role::Archive), Some(Role::Junk), Some(Role::Trash)];

/// One folder's page: name, offline use, notifications, role, its
/// subfolders, and Move and Delete.
pub fn folder_page(dialog: &adw::PreferencesDialog, account: &str, path: &str, hooks: &FolderHooks) -> adw::NavigationPage {
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    let page = adw::NavigationPage::builder().title("Folder").child(&view).build();
    let fill = {
        let (dialog, account, hooks, view, page_weak, path) =
            (dialog.downgrade(), account.to_string(), hooks.clone(), view.downgrade(), page.downgrade(), path.to_string());
        Rc::new(move || {
            let (Some(dialog), Some(view), Some(page)) = (dialog.upgrade(), view.upgrade(), page_weak.upgrade()) else { return };
            let tree = (hooks.tree)(&account);
            let Some(item) = find(&tree, &path).cloned() else {
                // Renamed, moved, or deleted: its parent's page shows where.
                if page.is_mapped() {
                    dialog.pop_subpage();
                }
                return;
            };
            page.set_title(&item.title);
            let above = hidden_above(&tree, &path).map(|i| i.title.clone());
            view.set_content(Some(&folder_content(&dialog, &account, &item, above.as_deref(), &hooks)));
        })
    };
    fill();
    register(account, &page, fill);
    page
}

/// `hidden_by` names the hidden parent hiding the folder, if any.
fn folder_content(dialog: &adw::PreferencesDialog, account: &str, item: &FolderItem, hidden_by: Option<&str>, hooks: &FolderHooks) -> adw::PreferencesPage {
    let path = item.full_name.clone();
    let inbox = path.eq_ignore_ascii_case("INBOX");
    let special = (hooks.special)(account);
    let role = special.role_of(&path);
    let changeable = !inbox && role.is_none();
    let content = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    if changeable {
        let name = adw::EntryRow::builder().title("Name").text(&item.title).show_apply_button(true).build();
        let (act, account, p) = (hooks.act.clone(), account.to_string(), path.clone());
        name.connect_apply(move |row| {
            let text = row.text().trim().to_string();
            if text.is_empty() || text.contains('/') {
                return;
            }
            act(FolderOp::Rename { account: account.clone(), path: p.clone(), name: text });
        });
        group.add(&name);
    } else {
        let name = adw::ActionRow::builder()
            .title("Name")
            .subtitle(glib::markup_escape_text(&item.title))
            .build();
        name.add_css_class("property");
        name.set_tooltip_text(Some(if inbox { "Inbox can't be renamed" } else { "Special folders can't be renamed" }));
        group.add(&name);
    }
    if (hooks.specific)(account) {
        let offline = adw::SwitchRow::builder().title("Download for offline use").sensitive(false).build();
        if inbox {
            offline.set_subtitle("Required");
        }
        group.add(&offline);
        if let Some(store) = (hooks.store)(account) {
            let p = path.clone();
            glib::spawn_future_local(async move {
                let Ok(Some(folder)) = store.folder_future(&p, camel::StoreGetFolderFlags::empty(), glib::ffi::G_PRIORITY_DEFAULT).await else {
                    offline.set_subtitle("Cannot be opened");
                    return;
                };
                offline.set_active(inbox || folder.synced());
                offline.set_sensitive(!inbox);
                offline.connect_active_notify(move |row| folder.set_synced(row.is_active()));
            });
        }
    }
    let hidden = item.hidden || hidden_by.is_some();
    let notify = adw::SwitchRow::builder()
        .title("Notify about new mail")
        .active(Settings::get().notify_folders_for(account).iter().any(|f| f.eq_ignore_ascii_case(&path)))
        .build();
    if hidden && notify.is_active() {
        notify.set_subtitle("Not while hidden");
    }
    let (a, p) = (account.to_string(), path.clone());
    notify.connect_active_notify(move |row| {
        let mut st = Settings::get();
        let mut list = st.notify_folders_for(&a);
        list.retain(|f| !f.eq_ignore_ascii_case(&p));
        if row.is_active() {
            list.push(p.clone());
        }
        st.notify_folders.insert(a.clone(), list);
        if let Err(e) = st.save() {
            glib::g_warning!("salutations", "Could not save settings: {}", e);
        }
    });
    group.add(&notify);
    if !inbox {
        let hide = adw::SwitchRow::builder().title("Hide from sidebar").active(hidden).build();
        match hidden_by {
            Some(parent) => {
                hide.set_subtitle(&format!("Hidden with {}", glib::markup_escape_text(parent)));
                hide.set_sensitive(false);
            }
            None => hide.set_subtitle("Unsubscribes, so other apps that follow subscriptions hide it too"),
        }
        let (act, a, p) = (hooks.act.clone(), account.to_string(), path.clone());
        hide.connect_active_notify(move |row| act(FolderOp::SetHidden { account: a.clone(), path: p.clone(), hidden: row.is_active() }));
        group.add(&hide);
    }
    if !inbox {
        let labels: Vec<&str> = ROLES.iter().map(|r| r.map_or("None", Role::label)).collect();
        let use_as = adw::ComboRow::builder()
            .title("Use as")
            .model(&gtk::StringList::new(&labels))
            .selected(ROLES.iter().position(|r| *r == role).unwrap_or(0) as u32)
            .build();
        let (act, a, p) = (hooks.act.clone(), account.to_string(), path.clone());
        use_as.connect_selected_notify(move |row| {
            let chosen = ROLES.get(row.selected() as usize).copied().flatten();
            if chosen != role {
                act(FolderOp::SetRole { account: a.clone(), path: p.clone(), role: chosen });
            }
        });
        group.add(&use_as);
    }
    content.add(&group);
    content.add(&folder_list(dialog, account, hooks, &item.children, Some(&path), "Subfolders", hidden));
    if changeable {
        let manage = adw::PreferencesGroup::new();
        let move_row = adw::ButtonRow::builder().title("_Move…").use_underline(true).build();
        let (act, a, p) = (hooks.act.clone(), account.to_string(), path.clone());
        move_row.connect_activated(move |_| act(FolderOp::Move { account: a.clone(), path: p.clone() }));
        manage.add(&move_row);
        let delete = adw::ButtonRow::builder().title("_Delete folder…").use_underline(true).build();
        delete.add_css_class("destructive-action");
        let (act, a, p) = (hooks.act.clone(), account.to_string(), path.clone());
        delete.connect_activated(move |_| act(FolderOp::Delete { account: a.clone(), path: p.clone() }));
        manage.add(&delete);
        content.add(&manage);
    }
    content
}

/// The Folders row on an account's settings, opening the Folders page.
pub fn folders_group(dialog: &adw::PreferencesDialog, account: &str, hooks: &FolderHooks) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    let row = adw::ActionRow::builder()
        .title("Folders")
        .subtitle("Names, offline use, notifications, hiding, and special folders")
        .activatable(true)
        .build();
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    let (dialog, account, hooks) = (dialog.downgrade(), account.to_string(), hooks.clone());
    row.connect_activated(move |_| {
        if let Some(dialog) = dialog.upgrade() {
            dialog.push_subpage(&root_page(&dialog, &account, &hooks));
        }
    });
    group.add(&row);
    group
}

/// Opens the Folders page and the pages down to `path`'s.
pub fn open_at(dialog: &adw::PreferencesDialog, account: &str, path: &str, hooks: &FolderHooks) {
    dialog.push_subpage(&root_page(dialog, account, hooks));
    let parts: Vec<&str> = path.split('/').collect();
    for i in 1..=parts.len() {
        let p = parts[..i].join("/");
        if find(&(hooks.tree)(account), &p).is_some() {
            dialog.push_subpage(&folder_page(dialog, account, &p, hooks));
        }
    }
}
