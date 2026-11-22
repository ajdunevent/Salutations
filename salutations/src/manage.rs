//! Dialogs for managing messages: confirmations and the folder pickers.

use adw::prelude::*;
use gtk::glib;

/// Asks before deleting `n` messages for good, every time.
pub async fn confirm_permanent(parent: &impl IsA<gtk::Widget>, n: usize, no_trash: bool) -> bool {
    let what = if n == 1 { "This message".to_string() } else { format!("These {n} messages") };
    let mut body = format!("{what} will be deleted permanently. This cannot be undone.");
    if no_trash {
        body = format!("This account has no Trash folder. {body}");
    }
    let heading = if n == 1 { "Delete message permanently?" } else { "Delete messages permanently?" };
    let dialog = adw::AlertDialog::new(Some(heading), Some(&body));
    dialog.add_responses(&[("cancel", "_Cancel"), ("delete", "_Delete")]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.choose_future(Some(parent)).await == "delete"
}

/// One folder offered by the picker.
#[derive(Debug, Clone)]
pub struct FolderChoice {
    /// The account's UID.
    pub account: String,
    pub full_name: String,
    /// The folder's names from the top, joined with " / ".
    pub path: String,
    pub title: String,
    pub icon: String,
    pub depth: usize,
    /// An account's name over its folders; not a folder.
    pub heading: bool,
}

/// Lets the user pick a folder; None when the dialog is closed.
pub async fn pick_folder(parent: &impl IsA<gtk::Widget>, title: &str, folders: Vec<FolderChoice>) -> Option<FolderChoice> {
    match pick(parent, title, folders, false).await? {
        Picked::Folder(f) => Some(f),
        Picked::New => None,
    }
}

/// What the move picker returned.
pub enum Picked {
    Folder(FolderChoice),
    /// New folder… was pressed.
    New,
}

/// The move picker, with New folder… in its header bar.
pub async fn pick_folder_or_new(parent: &impl IsA<gtk::Widget>, title: &str, folders: Vec<FolderChoice>) -> Option<Picked> {
    pick(parent, title, folders, true).await
}

async fn pick(parent: &impl IsA<gtk::Widget>, title: &str, folders: Vec<FolderChoice>, offer_new: bool) -> Option<Picked> {
    let (tx, rx) = futures_channel::oneshot::channel::<Option<Picked>>();
    let tx = std::rc::Rc::new(std::cell::RefCell::new(Some(tx)));
    let list = folder_list(&folders, gtk::SelectionMode::None);
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    let header = adw::HeaderBar::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&scrolled));
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(360)
        .content_height(480)
        .child(&view)
        .build();
    if offer_new {
        let new = gtk::Button::builder().icon_name("folder-new-symbolic").tooltip_text("New folder…").build();
        header.pack_start(&new);
        let (d, t) = (dialog.clone(), tx.clone());
        new.connect_clicked(move |_| {
            if let Some(tx) = t.borrow_mut().take() {
                let _ = tx.send(Some(Picked::New));
            }
            d.close();
        });
    }
    let d = dialog.clone();
    let t = tx.clone();
    list.connect_row_activated(move |_, row| {
        let Some(choice) = folders.get(row.index() as usize).filter(|f| !f.heading).cloned() else { return };
        if let Some(tx) = t.borrow_mut().take() {
            let _ = tx.send(Some(Picked::Folder(choice)));
        }
        d.close();
    });
    dialog.connect_closed(move |_| {
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(None);
        }
    });
    dialog.present(Some(parent));
    rx.await.ok().flatten()
}

/// Lets the user check any number of the folders, given as (full name,
/// path) pairs with paths joined by " / ", with `chosen` checked to
/// start. None when cancelled.
pub async fn pick_folders(parent: &impl IsA<gtk::Widget>, title: &str, folders: &[(String, String)], chosen: &[String]) -> Option<Vec<String>> {
    let (tx, rx) = futures_channel::oneshot::channel::<Option<Vec<String>>>();
    let tx = std::rc::Rc::new(std::cell::RefCell::new(Some(tx)));
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
    list.add_css_class("navigation-sidebar");
    let checks: Vec<(String, gtk::CheckButton)> = folders
        .iter()
        .map(|(full, path)| {
            let depth = path.matches(" / ").count();
            let name = path.rsplit(" / ").next().unwrap_or(path);
            let check = gtk::CheckButton::builder()
                .active(chosen.iter().any(|c| c.eq_ignore_ascii_case(full)))
                .valign(gtk::Align::Center)
                .build();
            let label = gtk::Label::builder().label(name).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.set_margin_start(6 + 18 * depth as i32);
            row.set_margin_top(6);
            row.set_margin_bottom(6);
            row.append(&check);
            row.append(&label);
            let lbr = gtk::ListBoxRow::builder().child(&row).activatable(true).build();
            lbr.update_property(&[gtk::accessible::Property::Label(path)]);
            let c = check.clone();
            lbr.connect_activate(move |_| c.set_active(!c.is_active()));
            list.append(&lbr);
            (full.clone(), check)
        })
        .collect();
    list.connect_row_activated(|_, row| {
        row.emit_activate();
    });
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .vexpand(true)
        .child(&list)
        .build();
    let cancel = gtk::Button::with_mnemonic("_Cancel");
    let done = gtk::Button::builder().use_underline(true).label("_Done").css_classes(["suggested-action"]).build();
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    header.pack_start(&cancel);
    header.pack_end(&done);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&scrolled));
    let dialog = adw::Dialog::builder().title(title).content_width(360).content_height(480).child(&view).build();
    let update = {
        let (done, checks) = (done.clone(), checks.clone());
        move || done.set_sensitive(checks.iter().any(|(_, c)| c.is_active()))
    };
    update();
    for (_, c) in &checks {
        let u = update.clone();
        c.connect_active_notify(move |_| u());
    }
    let (d, t) = (dialog.clone(), tx.clone());
    done.connect_clicked(move |_| {
        let picked: Vec<String> = checks.iter().filter(|(_, c)| c.is_active()).map(|(f, _)| f.clone()).collect();
        if let Some(tx) = t.borrow_mut().take() {
            let _ = tx.send(Some(picked));
        }
        d.close();
    });
    let d = dialog.clone();
    cancel.connect_clicked(move |_| {
        d.close();
    });
    dialog.connect_closed(move |_| {
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(None);
        }
    });
    dialog.present(Some(parent));
    rx.await.ok().flatten()
}

/// The folders as rows, under their accounts' headings.
fn folder_list(folders: &[FolderChoice], mode: gtk::SelectionMode) -> gtk::ListBox {
    let list = gtk::ListBox::builder().selection_mode(mode).build();
    list.add_css_class("navigation-sidebar");
    for f in folders {
        if f.heading {
            let label = gtk::Label::builder().label(&f.title).xalign(0.0).css_classes(["heading"]).margin_top(12).margin_start(6).build();
            list.append(&gtk::ListBoxRow::builder().child(&label).activatable(false).selectable(false).build());
            continue;
        }
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.set_margin_start(6 + 18 * f.depth as i32);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        row.append(&gtk::Image::from_icon_name(&f.icon));
        let label = gtk::Label::builder()
            .label(&f.title)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        row.append(&label);
        let lbr = gtk::ListBoxRow::builder().child(&row).activatable(true).build();
        lbr.update_property(&[gtk::accessible::Property::Label(&f.path)]);
        list.append(&lbr);
    }
    list
}

/// Asks where to import `what` (a file name, or how many messages), with
/// the folder at `selected` chosen to start with. Nothing is imported
/// until Import is pressed.
pub async fn pick_import_folder(
    parent: &impl IsA<gtk::Widget>,
    what: &str,
    folders: Vec<FolderChoice>,
    selected: Option<(String, String)>,
) -> Option<FolderChoice> {
    let (tx, rx) = futures_channel::oneshot::channel::<Option<FolderChoice>>();
    let tx = std::rc::Rc::new(std::cell::RefCell::new(Some(tx)));
    let list = folder_list(&folders, gtk::SelectionMode::Single);
    // A click only chooses; Import, a double click, or Enter imports.
    list.set_activate_on_single_click(false);
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .vexpand(true)
        .child(&list)
        .build();
    let cancel = gtk::Button::with_mnemonic("_Cancel");
    let import = gtk::Button::builder().use_underline(true).label("_Import").sensitive(false).css_classes(["suggested-action"]).build();
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    header.pack_start(&cancel);
    header.pack_end(&import);
    let summary = gtk::Label::builder()
        .label(what)
        .wrap(true)
        .xalign(0.0)
        .margin_start(18)
        .margin_end(18)
        .margin_top(6)
        .margin_bottom(6)
        .css_classes(["dim-label"])
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&summary);
    content.append(&scrolled);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&content));
    let dialog = adw::Dialog::builder()
        .title("Import to")
        .content_width(360)
        .content_height(480)
        .child(&view)
        .build();
    let index = selected.and_then(|(account, name)| folders.iter().position(|f| !f.heading && f.account == account && f.full_name == name));
    if let Some(row) = index.and_then(|i| list.row_at_index(i as i32)) {
        list.select_row(Some(&row));
        import.set_sensitive(true);
        // Scrolled to once the list has its size.
        let r = row.clone();
        glib::idle_add_local_once(move || {
            r.grab_focus();
        });
    }
    let i = import.clone();
    list.connect_row_selected(move |_, row| i.set_sensitive(row.is_some()));
    let i = import.clone();
    list.connect_row_activated(move |_, _| i.emit_clicked());
    let (d, t, l) = (dialog.clone(), tx.clone(), list.clone());
    import.connect_clicked(move |_| {
        let choice = l.selected_row().and_then(|r| folders.get(r.index() as usize)).filter(|f| !f.heading).cloned();
        if let Some(tx) = t.borrow_mut().take() {
            let _ = tx.send(choice);
        }
        d.close();
    });
    let d = dialog.clone();
    cancel.connect_clicked(move |_| {
        d.close();
    });
    dialog.connect_closed(move |_| {
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(None);
        }
    });
    dialog.present(Some(parent));
    rx.await.ok().flatten()
}

/// What to do about an account without an Archive folder.
pub enum ArchiveSetup {
    Create,
    Choose,
}

pub async fn ask_archive_setup(parent: &impl IsA<gtk::Widget>) -> Option<ArchiveSetup> {
    let dialog = adw::AlertDialog::new(
        Some("No Archive folder"),
        Some("No Archive folder is set for this account. Create one, or choose an existing folder for archived mail?"),
    );
    dialog.add_responses(&[("cancel", "_Cancel"), ("choose", "C_hoose folder…"), ("create", "_Create \u{201c}Archive\u{201d}")]);
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("create"));
    dialog.set_close_response("cancel");
    match dialog.choose_future(Some(parent)).await.as_str() {
        "create" => Some(ArchiveSetup::Create),
        "choose" => Some(ArchiveSetup::Choose),
        _ => None,
    }
}
