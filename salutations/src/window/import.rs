//! Importing `.eml` files into a folder: from the main menu, from Files
//! ("Open with"), or dropped on a folder or the message list. Every way
//! asks for the folder first; a drop starts with its folder chosen.

use super::*;

impl Window {
    /// The main menu's Import messages: picks the files, then the folder.
    pub(super) fn choose_import_files(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Email messages"));
        filter.add_mime_type("message/rfc822");
        filter.add_suffix("eml");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder().title("Import messages").modal(true).filters(&filters).build();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let Ok(list) = dialog.open_multiple_future(Some(&this.window)).await else { return };
            let files: Vec<gio::File> = (0..list.n_items()).filter_map(|i| list.item(i).and_downcast::<gio::File>()).collect();
            if !files.is_empty() {
                this.import_files(files, None);
            }
        });
    }

    /// Asks for the folder, with `folder` (account UID, full name) chosen
    /// to start with, then adds the files there.
    pub(crate) fn import_files(self: &Rc<Self>, files: Vec<gio::File>, folder: Option<(String, String)>) {
        self.window.present();
        let what = match files.as_slice() {
            [one] => format!("Import \u{201c}{}\u{201d} into the chosen folder.", display_name(one)),
            many => format!("Import {} messages into the chosen folder.", many.len()),
        };
        let choices = all_folder_choices(&self.accounts.borrow(), None);
        let this = self.clone();
        glib::spawn_future_local(async move {
            let Some(choice) = manage::pick_import_folder(&this.window, &what, choices, folder).await else { return };
            let Some(ctl) = this.accounts.borrow().iter().find(|c| c.account.uid == choice.account).cloned() else { return };
            let folder = match this.store_for(&ctl) {
                Ok(store) => this.session.folder_future(&store, &choice.full_name).await,
                Err(e) => Err(e),
            };
            let folder = match folder {
                Ok(f) => f,
                Err(e) => return this.toast(&format!("Cannot open {}: {}", choice.path, e.message()), false),
            };
            let (mut added, mut present, mut failed) = (0, 0, Vec::new());
            for file in &files {
                match import_one(&folder, file).await {
                    Ok(true) => added += 1,
                    Ok(false) => present += 1,
                    Err(e) => failed.push(format!("{}: {e}", display_name(file))),
                }
            }
            let mut text = match (added, present) {
                (0, 0) => String::new(),
                (0, _) => format!("Already in {}", choice.title),
                (1, _) => format!("1 message imported to {}", choice.title),
                (n, _) => format!("{n} messages imported to {}", choice.title),
            };
            if added > 0 && present > 0 {
                text.push_str(&format!("; {present} already there"));
            }
            if !failed.is_empty() {
                let sep = if text.is_empty() { "" } else { "; " };
                text.push_str(&format!("{sep}Not imported: {}", failed.join(", ")));
            }
            this.toast(&text, false);
        });
    }
}

fn display_name(file: &gio::File) -> String {
    file.basename().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| file.uri().to_string())
}

/// Adds one file's message to `folder`, marked read. Ok(false) when a
/// message with the same Message-ID is there already.
async fn import_one(folder: &camel::Folder, file: &gio::File) -> Result<bool, String> {
    let stream = file.read_future(glib::Priority::DEFAULT).await.map_err(|e| e.message().to_string())?;
    let msg = camel::MimeMessage::new();
    msg.construct_from_input_stream_future(&stream, glib::ffi::G_PRIORITY_DEFAULT)
        .await
        .map_err(|e| e.message().to_string())?;
    // Anything parses as a message; a mail file has at least one of these.
    if msg.from().is_none() && msg.date().0 == 0 && msg.message_id().is_none() {
        return Err("not an email message".into());
    }
    if let Some(id) = msg.message_id().map(|i| i.to_string()) {
        let f = folder.clone();
        let quoted = id.replace('\\', "\\\\").replace('"', "\\\"");
        let found = gio::spawn_blocking(move || {
            f.search_sync(&format!("(match-all (header-contains \"Message-ID\" \"{quoted}\"))"), None::<&gio::Cancellable>)
                .map(|uids| !uids.is_empty())
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false);
        if found {
            return Ok(false);
        }
    }
    salutations_core::send::append(folder, &msg, SEEN).await.map_err(|e| e.message().to_string())?;
    Ok(true)
}
