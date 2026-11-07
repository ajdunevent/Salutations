mod background;
mod clock;
mod composer;
mod contact_menu;
mod debug;
mod default_app;
mod folder_object;
mod fit_stack;
mod folder_pane;
mod label_bar;
mod manage;
mod message_list;
mod message_object;
mod overflow_box;
mod notify;
mod online_accounts;
mod preferences;
mod people;
mod pull_refresh;
mod recipients;
mod row_holder;
mod search;
mod tile_grid;
mod viewer;
mod window;

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use salutations_core::camel::prelude::*;
use salutations_core::edataserver::SourceRegistry;
use salutations_core::{APP_ID, Session};

fn main() -> glib::ExitCode {
    debug::init();
    gio::resources_register_include!("salutations.gresource").expect("bundled resources");
    if let Err(e) = salutations_core::init() {
        eprintln!("Salutations cannot start: {e}");
        return glib::ExitCode::FAILURE;
    }
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .resource_base_path("/net/dunevent/Salutations")
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    // Autostart passes --background: the first activation keeps the
    // window hidden.
    let hidden_start = std::rc::Rc::new(std::cell::Cell::new(false));
    let main: Rc<RefCell<Option<Rc<window::Window>>>> = Rc::default();
    app.connect_startup(|app| {
        viewer::register_cid_scheme();
        viewer::clear_opened_attachments();
        clock::init();
        install_app_actions(app);
    });
    let (m, hidden_start_a) = (main.clone(), hidden_start.clone());
    app.connect_activate(move |app| {
        if let Some(w) = m.borrow().as_ref() {
            w.window.present();
            return;
        }
        let registry = match SourceRegistry::new_sync(None::<&gio::Cancellable>) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Cannot reach the Evolution Data Server account registry: {e}");
                app.quit();
                return;
            }
        };
        let session = Session::new(&registry, &salutations_core::data_dir(), &salutations_core::cache_dir());
        session.set_online(true);
        let w = window::Window::new(app, registry, session);
        background::set_window(&w.window);
        background::apply();
        let hidden = hidden_start_a.replace(false) && salutations_core::Settings::get().run_in_background;
        if !hidden {
            w.window.present();
            default_app::first_run(&w.window);
        }
        *m.borrow_mut() = Some(w);
    });
    // A second launch hands its arguments to the running instance:
    // --background (autostart), --compose (the desktop's New message
    // action), mailto: links, and .eml files to import.
    let (m, hidden) = (main.clone(), hidden_start.clone());
    app.connect_command_line(move |app, cmd| {
        let mut compose = false;
        let mut mailto = None;
        let mut files = Vec::new();
        for arg in cmd.arguments().iter().skip(1) {
            let text = arg.to_string_lossy();
            if text == background::FLAG {
                hidden.set(m.borrow().is_none());
            } else if text == "--compose" {
                compose = true;
            } else if let Some(link) = salutations_core::mailto::parse(&text) {
                mailto.get_or_insert(link);
            } else if !text.starts_with("--") {
                files.push(cmd.create_file_for_arg(arg));
            }
        }
        app.activate();
        let Some(w) = m.borrow().clone() else { return glib::ExitCode::SUCCESS };
        if mailto.is_some() || compose {
            w.compose_external(mailto);
        }
        if !files.is_empty() {
            w.import_files(files, None);
        }
        glib::ExitCode::SUCCESS
    });
    // Quit waits for an unsaved draft to reach the server, and for queued
    // moves unless the user quits anyway.
    let quit = gio::SimpleAction::new("quit", None);
    let (a, m) = (app.clone(), main.clone());
    quit.connect_activate(move |_, _| {
        let (a, w) = (a.clone(), m.borrow().clone());
        glib::spawn_future_local(async move {
            if let Some(w) = w {
                w.save_before_exit().await;
                w.finish_queue().await;
            }
            a.quit();
        });
    });
    app.add_action(&quit);
    let m = main.clone();
    app.connect_shutdown(move |_| {
        if let Some(w) = m.borrow().as_ref() {
            w.shutdown();
        }
        stop_sandbox_gnupg();
    });
    app.run()
}

/// In the Flatpak, gpg starts a gpg-agent and scdaemon of its own inside
/// the sandbox, and they outlive Salutations, keeping the sandbox running.
/// They serve only this sandbox, so they are stopped on quit. Outside the
/// Flatpak the agent is the user's, shared with everything else, and is
/// left alone.
fn stop_sandbox_gnupg() {
    if !std::path::Path::new("/.flatpak-info").exists() {
        return;
    }
    let _ = std::process::Command::new("gpgconf")
        .args(["--kill", "all"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The metainfo in the resource bundle, which the About dialog reads.
const METAINFO: &str = "/net/dunevent/Salutations/net.dunevent.Salutations.metainfo.xml";

/// The About dialog. Name, developer, links and release notes come from the
/// metainfo, so they are edited there; the release notes shown are those of
/// the Cargo version, which the metainfo must list (a test checks).
fn about_dialog() -> adw::AboutDialog {
    let version = env!("CARGO_PKG_VERSION");
    let d = adw::AboutDialog::from_appdata(METAINFO, Some(version));
    d.set_application_icon(APP_ID);
    d.set_version(version);
    d.set_copyright("© 2026 Salutations contributors");
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
        ("Undo move, archive or delete", "<Control>z"),
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
