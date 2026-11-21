mod background;
mod clock;
mod composer;
mod contact_menu;
mod debug;
mod default_app;
mod folder_object;
mod fit_stack;
mod folder_pane;
mod inline_image;
mod label_bar;
mod manage;
mod message_list;
mod message_object;
mod overflow_box;
mod mail;
mod online_accounts;
mod preferences;
mod people;
mod pull_refresh;
mod remote;
mod recipients;
mod row_holder;
mod rule_editor;
mod search;
mod tile_grid;
mod viewer;
mod window;

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use salutations_core::APP_ID;
use salutations_core::edataserver::SourceRegistry;

/// The window process, started by the background process (the
/// `salutations` command), which it talks to over a private connection.
fn main() -> glib::ExitCode {
    debug::init();
    gio::resources_register_include!("salutations.gresource").expect("bundled resources");
    if let Err(e) = salutations_core::init_window() {
        eprintln!("Salutations cannot start: {e}");
        return glib::ExitCode::FAILURE;
    }
    // The desktop matches the window to the app by this name.
    glib::set_prgname(Some(APP_ID));
    let app = adw::Application::builder()
        .resource_base_path("/net/dunevent/Salutations")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let main: Rc<RefCell<Option<Rc<window::Window>>>> = Rc::default();
    app.connect_startup(|app| {
        viewer::register_cid_scheme();
        viewer::clear_opened_attachments();
        clock::init();
        install_app_actions(app);
    });
    let m = main.clone();
    app.connect_activate(move |app| {
        if let Some(w) = m.borrow().as_ref() {
            w.window.present();
            return;
        }
        // Held until the window exists, which needs the connection first.
        let hold = app.hold();
        let (app, m) = (app.clone(), m.clone());
        glib::spawn_future_local(async move {
            let _hold = hold;
            if let Err(e) = remote::connect().await {
                eprintln!("Salutations: {e}");
                return;
            }
            let registry = match SourceRegistry::new_future().await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Cannot reach the Evolution Data Server account registry: {e}");
                    return;
                }
            };
            let w = window::Window::new(&app, registry);
            w.window.present();
            default_app::first_run(&w.window);
            *m.borrow_mut() = Some(w);
        });
    });
    // Quit waits for an unsaved draft to reach the server, and for queued
    // moves unless the user quits anyway; it also ends background running.
    let quit = gio::SimpleAction::new("quit", None);
    let (a, m) = (app.clone(), main.clone());
    quit.connect_activate(move |_, _| {
        let (a, w) = (a.clone(), m.borrow().clone());
        glib::spawn_future_local(async move {
            if let Some(w) = w {
                w.save_before_exit().await;
                w.finish_queue().await;
            }
            let _ = remote::call("quit", salutations_core::ipc::unit()).await;
            a.quit();
        });
    });
    app.add_action(&quit);
    app.run()
}

/// The metainfo in the resource bundle, which the About dialog reads.
const METAINFO: &str = "/net/dunevent/Salutations/net.dunevent.Salutations.metainfo.xml";

/// The About dialog. Name, developer, links, and release notes come from the
/// metainfo, so they are edited there; the release notes shown are those of
/// the Cargo version, which the metainfo must list (a test checks).
fn about_dialog() -> adw::AboutDialog {
    let version = env!("CARGO_PKG_VERSION");
    let d = adw::AboutDialog::from_appdata(METAINFO, Some(version));
    d.set_application_icon(APP_ID);
    d.set_version(version);
    d.set_copyright("© 2026 Salutations contributors");
    d.set_developers(&["AJ Dunevent"]);
    d.set_license_type(gtk::License::Gpl30);
    d.add_link("_Donate", "https://dunevent.net/donate");
    d.add_acknowledgement_section(
        Some("Built on"),
        &[
            "Evolution Data Server and Camel https://gitlab.gnome.org/GNOME/evolution-data-server",
            "GNOME Online Accounts https://gitlab.gnome.org/GNOME/gnome-online-accounts",
        ],
    );
    d.add_acknowledgement_section(
        Some("X-Face decoding"),
        &["FFmpeg https://ffmpeg.org", "James Ashton, for compface"],
    );
    d.add_acknowledgement_section(
        Some("JPEG encoding"),
        &[
            "jpeg-encoder https://github.com/vstroebel/jpeg-encoder",
            "This software is based in part on the work of the Independent JPEG Group",
        ],
    );
    d.add_acknowledgement_section(
        Some("Icons"),
        &["Adwaita icon theme https://gitlab.gnome.org/GNOME/adwaita-icon-theme"],
    );
    d.add_acknowledgement_section(
        Some("Lessons learned from"),
        &[
            "Stamp https://gitlab.gnome.org/jbrummer/stamp",
            "Convey https://gitlab.gnome.org/donnybeelo/convey",
        ],
    );
    d.add_legal_section(
        "Camel and EDataServer bindings",
        Some("© 2026 Salutations contributors"),
        gtk::License::Lgpl21,
        None,
    );
    d.add_legal_section(
        "X-Face decoder",
        Some("© 1990 James Ashton\n© 2012 Stefano Sabatini"),
        gtk::License::Lgpl21,
        None,
    );
    d.add_legal_section(
        "jpeg-encoder",
        Some("© 2021 Volker Ströbel\n© 1991-2020 Thomas G. Lane, Guido Vollbeding\n© 2015, 2020 D. R. Commander"),
        gtk::License::Custom,
        Some(
            "Licensed under the <a href=\"https://opensource.org/license/mit\">MIT</a> or \
             <a href=\"https://www.apache.org/licenses/LICENSE-2.0\">Apache 2.0</a> license, \
             with the parts from the Independent JPEG Group's software under the IJG license.",
        ),
    );
    d.add_legal_section(
        "Adwaita icons",
        Some("© GNOME Project"),
        gtk::License::Custom,
        Some(
            "Some symbolic icons are copied or modified from the Adwaita icon theme, \
             licensed under the <a href=\"https://creativecommons.org/licenses/by-sa/3.0/\">\
             Creative Commons Attribution-Share Alike 3.0</a> license or the \
             <a href=\"https://www.gnu.org/licenses/lgpl-3.0.html\">GNU Lesser General \
             Public License, version 3</a>.",
        ),
    );
    d
}

fn install_app_actions(app: &adw::Application) {


    let accounts = gio::SimpleAction::new("online-accounts", None);
    accounts.connect_activate(|_, _| online_accounts::open());
    app.add_action(&accounts);

    let about = gio::SimpleAction::new("about", None);
    let a = app.clone();
    about.connect_activate(move |_, _| {
        about_dialog().present(a.active_window().as_ref());
    });
    app.add_action(&about);

    let shortcuts = gio::SimpleAction::new("shortcuts", None);
    let a = app.clone();
    shortcuts.connect_activate(move |_, _| shortcuts_dialog(a.active_window().as_ref()));
    app.add_action(&shortcuts);

    app.set_accels_for_action("app.quit", &["<Control>q"]);
    app.set_accels_for_action("win.preferences", &["<Control>comma"]);
    app.set_accels_for_action("app.shortcuts", &["<Control>question"]);
    app.set_accels_for_action("win.search", &["<Control>f"]);
    app.set_accels_for_action("win.sync", &["F5"]);
    app.set_accels_for_action("win.print", &["<Control>p"]);
    app.set_accels_for_action("win.save-message", &["<Control>s"]);
    app.set_accels_for_action("win.toggle-folders", &["F9"]);
}

fn shortcuts_dialog(parent: Option<&gtk::Window>) {
    let section = adw::ShortcutsSection::new(Some("Messages"));
    for (title, accel) in [
        ("Previous / next message", "Up Down"),
        ("Select or deselect the current message", "Menu <Shift>F10"),
        ("First / last message in thread", "Left Right"),
        ("Toggle read", "m"),
        ("Archive", "a"),
        ("Move to Trash", "Delete"),
        ("Delete permanently", "<Shift>Delete"),
        ("Undo move, archive, or delete", "<Control>z"),
        ("Print", "<Control>p"),
        ("Save as file", "<Control>s"),
        ("Search", "<Control>f"),
        ("Sync account", "F5"),
    ] {
        section.add(adw::ShortcutsItem::new(title, accel));
    }
    let general = adw::ShortcutsSection::new(Some("General"));
    for (title, accel) in [
        ("Show or hide folders", "F9"),
        ("Preferences", "<Control>comma"),
        ("Keyboard shortcuts", "<Control>question"),
        ("Quit", "<Control>q"),
    ] {
        general.add(adw::ShortcutsItem::new(title, accel));
    }
    let dialog = adw::ShortcutsDialog::new();
    dialog.add(section);
    dialog.add(general);
    dialog.present(parent);
}
