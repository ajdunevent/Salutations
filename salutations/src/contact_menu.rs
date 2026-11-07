//! The menu for a person in a message's headers or a recipient pill: New
//! message, Add to contacts… or Show in Contacts, and Copy address.
//!
//! Adding saves the name and address to an address book, asking which when
//! more than one can be written to, then opens the new contact in GNOME
//! Contacts for editing. Contacts has no way to open a pre-filled new
//! contact; its search provider can find one by address and open it.

use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use salutations_core::contacts::{Book, Contacts};

const PROVIDER: &str = "org.gnome.Contacts.SearchProvider";
const PROVIDER_PATH: &str = "/org/gnome/Contacts/SearchProvider";
const PROVIDER_IFACE: &str = "org.gnome.Shell.SearchProvider2";
/// A new contact reaches Contacts' search a moment after it is saved.
const FIND_TRIES: u32 = 10;
const FIND_WAIT: Duration = Duration::from_millis(500);

/// What the menu needs from the window.
pub struct Ctx {
    pub contacts: Contacts,
    /// Starts a message to "Name <address>".
    pub compose: Box<dyn Fn(&str)>,
    pub toast: Box<dyn Fn(&str)>,
}

/// Opens the menu for `email` on `anchor`, pointing at `at` (in `anchor`'s
/// coordinates) when given. `compose` offers New message.
pub fn popup(
    ctx: &Rc<Ctx>,
    anchor: &impl IsA<gtk::Widget>,
    at: Option<gdk::Rectangle>,
    name: Option<String>,
    email: String,
    compose: bool,
) {
    let anchor = anchor.as_ref().clone();
    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        // The index answers at once; only before it has loaded is a book
        // asked, which can take a moment.
        let known = match crate::people::known(&email) {
            Some(k) => k,
            None => {
                let contacts = ctx.contacts.clone();
                let e = email.clone();
                gio::spawn_blocking(move || contacts.is_known_sync(&e)).await.unwrap_or(false)
            }
        };
        if anchor.root().is_none() {
            return;
        }
        let group = gio::SimpleActionGroup::new();
        let menu = gio::Menu::new();
        let add = |id: &str, label: &str, f: Box<dyn Fn()>| {
            let action = gio::SimpleAction::new(id, None);
            action.connect_activate(move |_, _| f());
            group.add_action(&action);
            menu.append(Some(label), Some(&format!("contact.{id}")));
        };
        if compose {
            let to = salutations_core::compose::format_address(&salutations_core::Address {
                name: name.clone(),
                email: email.clone(),
            });
            let c = ctx.clone();
            add("compose", "New message", Box::new(move || (c.compose)(&to)));
        }
        let (c, e, w) = (ctx.clone(), email.clone(), anchor.downgrade());
        if known {
            add("show", "Show in Contacts", Box::new(move || show(&c, &e)));
        } else {
            let n = name.clone();
            add(
                "add",
                "Add to contacts…",
                Box::new(move || {
                    if let Some(w) = w.upgrade() {
                        add_contact(&c, &w, n.clone(), e.clone());
                    }
                }),
            );
        }
        let e = email.clone();
        let display = anchor.display();
        add("copy", "Copy address", Box::new(move || display.clipboard().set_text(&e)));

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.insert_action_group("contact", Some(&group));
        popover.set_parent(&anchor);
        if let Some(at) = at {
            popover.set_pointing_to(Some(&at));
        }
        popover.connect_closed(|p| {
            // Unparented after its action runs, which follows closing.
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    });
}

/// Saves the contact, then opens it in Contacts.
fn add_contact(ctx: &Rc<Ctx>, anchor: &gtk::Widget, name: Option<String>, email: String) {
    let ctx = ctx.clone();
    let anchor = anchor.clone();
    glib::spawn_future_local(async move {
        let contacts = ctx.contacts.clone();
        let books = gio::spawn_blocking(move || contacts.writable_books_sync()).await.unwrap_or_default();
        let book = match books.as_slice() {
            [] => return (ctx.toast)("No address book can be added to"),
            [one] => one.clone(),
            _ => match choose_book(&anchor, &books).await {
                Some(b) => b,
                None => return,
            },
        };
        let contacts = ctx.contacts.clone();
        let (uid, n, e) = (book.uid.clone(), name.clone(), email.clone());
        let added = gio::spawn_blocking(move || contacts.add_sync(&uid, n.as_deref(), &e).map_err(|e| e.message().to_string()))
            .await
            .unwrap_or_else(|_| Err("The contact was not added".into()));
        if let Err(e) = added {
            return (ctx.toast)(&format!("Cannot add {email}: {e}"));
        }
        if !open_in_contacts(&email, FIND_TRIES).await {
            (ctx.toast)(&format!("Added {email} to {}", book.name));
        }
    });
}

fn show(ctx: &Rc<Ctx>, email: &str) {
    let ctx = ctx.clone();
    let email = email.to_string();
    glib::spawn_future_local(async move {
        if !open_in_contacts(&email, 1).await {
            (ctx.toast)("GNOME Contacts could not show this contact");
        }
    });
}

/// Asks which book to add to; None when cancelled.
async fn choose_book(anchor: &gtk::Widget, books: &[Book]) -> Option<Book> {
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let mut first: Option<gtk::CheckButton> = None;
    let mut checks = Vec::new();
    for book in books {
        let check = gtk::CheckButton::new();
        if let Some(f) = &first {
            check.set_group(Some(f));
        } else {
            check.set_active(true);
            first = Some(check.clone());
        }
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(&book.name)).activatable_widget(&check).build();
        row.add_prefix(&check);
        list.append(&row);
        checks.push(check);
    }
    let dialog = adw::AlertDialog::builder().heading("Add to which address book?").extra_child(&list).build();
    dialog.add_responses(&[("cancel", "_Cancel"), ("add", "_Add")]);
    dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("add"));
    dialog.set_close_response("cancel");
    let response = dialog.choose_future(Some(anchor)).await;
    if response != "add" {
        return None;
    }
    checks.iter().position(|c| c.is_active()).map(|i| books[i].clone())
}

/// Finds the contact with `email` through Contacts' search provider and
/// opens it, trying `tries` times. False when Contacts is missing or did
/// not find it.
async fn open_in_contacts(email: &str, tries: u32) -> bool {
    let Ok(bus) = gio::bus_get_future(gio::BusType::Session).await else { return false };
    let terms = vec![email.to_string()];
    for attempt in 0..tries {
        if attempt > 0 {
            glib::timeout_future(FIND_WAIT).await;
        }
        let Ok(reply) = bus
            .call_future(
                Some(PROVIDER),
                PROVIDER_PATH,
                PROVIDER_IFACE,
                "GetInitialResultSet",
                Some(&(terms.clone(),).to_variant()),
                Some(glib::VariantTy::new("(as)").unwrap()),
                gio::DBusCallFlags::NONE,
                10_000,
            )
            .await
        else {
            return false;
        };
        let (ids,): (Vec<String>,) = reply.get().unwrap_or_default();
        if ids.is_empty() {
            continue;
        }
        let id = pick(&bus, &ids, email).await;
        return bus
            .call_future(
                Some(PROVIDER),
                PROVIDER_PATH,
                PROVIDER_IFACE,
                "ActivateResult",
                Some(&(id, terms.clone(), 0u32).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                10_000,
            )
            .await
            .is_ok();
    }
    false
}

/// The result whose description is `email`, else the first. The search
/// matches names too, so other people can come first.
async fn pick(bus: &gio::DBusConnection, ids: &[String], email: &str) -> String {
    let metas = bus
        .call_future(
            Some(PROVIDER),
            PROVIDER_PATH,
            PROVIDER_IFACE,
            "GetResultMetas",
            Some(&(ids.to_vec(),).to_variant()),
            Some(glib::VariantTy::new("(aa{sv})").unwrap()),
            gio::DBusCallFlags::NONE,
            10_000,
        )
        .await;
    if let Ok(reply) = metas {
        let (list,): (Vec<std::collections::HashMap<String, glib::Variant>>,) = reply.get().unwrap_or_default();
        for meta in list {
            let text = |k: &str| meta.get(k).and_then(|v| v.get::<String>()).unwrap_or_default();
            if text("description").split(" / ").next().is_some_and(|d| d.eq_ignore_ascii_case(email)) {
                return text("id");
            }
        }
    }
    ids[0].clone()
}
