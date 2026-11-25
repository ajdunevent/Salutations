//! Salutations' background process. It owns the mail accounts, keeps them
//! synced, notifies about new mail, and runs the rules; the window is
//! a separate process it starts on demand, so GTK, WebKit, and the GPU
//! driver are not loaded while no window is open.

mod daemon;
mod images;
mod inbox;
mod notify;
mod outbox;
mod people;
mod status;

use std::cell::RefCell;
use std::rc::Rc;

use gio::prelude::*;
use salutations_core::camel::prelude::*;
use salutations_core::edataserver::SourceRegistry;
use salutations_core::engine::Engine;
use salutations_core::{APP_ID, Session, Settings};

use daemon::Daemon;

/// Starts without a window; used by autostart.
const BACKGROUND: &str = "--background";

fn main() -> glib::ExitCode {
    salutations_core::debug::init();
    if let Err(e) = salutations_core::init() {
        eprintln!("Salutations cannot start: {e}");
        return glib::ExitCode::FAILURE;
    }
    let app = gio::Application::new(Some(APP_ID), gio::ApplicationFlags::HANDLES_COMMAND_LINE);
    let daemon: Rc<RefCell<Option<Rc<Daemon>>>> = Rc::default();
    let d = daemon.clone();
    app.connect_startup(move |app| {
        let registry = match SourceRegistry::new_sync(None::<&gio::Cancellable>) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Cannot reach the Evolution Data Server account registry: {e}");
                return;
            }
        };
        let session = Session::new(&registry, &salutations_core::data_dir(), &salutations_core::cache_dir());
        session.set_online(true);
        let people = people::People::start(salutations_core::contacts::Contacts::new(&registry));
        let engine = Engine::new(registry, session);
        let this = Daemon::new(app, engine.clone(), people);
        engine.start();
        *d.borrow_mut() = Some(this);
    });
    let d = daemon.clone();
    app.connect_activate(move |_| {
        if let Some(d) = d.borrow().clone() {
            d.show_window();
        }
    });
    // Every launch comes here, the first one and those handed over from
    // a second process: --background (autostart), --compose, mailto: links
    // and .eml files to import.
    let (d, first) = (daemon.clone(), std::cell::Cell::new(true));
    app.connect_command_line(move |app, cmd| {
        let Some(d) = d.borrow().clone() else {
            app.quit();
            return glib::ExitCode::FAILURE;
        };
        let mut background = false;
        let mut compose = None;
        let mut files = Vec::new();
        for arg in cmd.arguments().iter().skip(1) {
            let text = arg.to_string_lossy();
            if text == BACKGROUND {
                background = true;
            } else if text == "--compose" {
                compose.get_or_insert(String::new());
            } else if text.starts_with("mailto:") {
                compose = Some(text.to_string());
            } else if !text.starts_with("--") {
                files.push(cmd.create_file_for_arg(arg).path().map(|p| p.to_string_lossy().to_string()).unwrap_or_default());
            }
        }
        let starting = first.replace(false);
        let hidden = background && starting && Settings::get().run_in_background;
        if !hidden && !(background && !starting) {
            d.show_window();
        }
        if let Some(link) = compose {
            d.emit("compose", link.to_variant());
        }
        files.retain(|f| !f.is_empty());
        if !files.is_empty() {
            d.emit("import", files.to_variant());
        }
        glib::ExitCode::SUCCESS
    });
    let d = daemon.clone();
    app.connect_shutdown(move |_| {
        if let Some(d) = d.borrow().as_ref() {
            d.shutdown();
        }
        stop_sandbox_gnupg();
    });
    app.run()
}

/// Hands memory glibc keeps after frees back to the system; without it
/// the process stays as large as its busiest moment.
pub(crate) fn trim_memory() {
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> std::ffi::c_int;
    }
    // SAFETY: malloc_trim only releases free heap pages.
    unsafe {
        malloc_trim(0);
    }
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
