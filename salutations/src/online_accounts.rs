//! Opens panels of GNOME Settings (Online Accounts, Apps).

use std::collections::HashMap;

use gtk::prelude::*;
use gtk::{gio, glib};

/// Asks GNOME Settings over D-Bus first, which needs only
/// `--talk-name=org.gnome.Settings` in a Flatpak, then falls back to
/// starting `gnome-control-center`, which works outside a sandbox only.
pub fn open() {
    open_panel("online-accounts");
}

/// Opens GNOME Settings at `panel`, as `open` does.
pub fn open_panel(panel: &'static str) {
    glib::spawn_future_local(async move {
        let Err(e) = activate(panel).await else { return };
        if let Err(e2) = gio::Subprocess::newv(
            &["gnome-control-center".as_ref(), panel.as_ref()],
            gio::SubprocessFlags::NONE,
        ) {
            glib::g_warning!("salutations", "Cannot open Settings ({}): {}; {}", panel, e, e2);
        }
    });
}

async fn activate(panel: &str) -> Result<(), glib::Error> {
    let bus = gio::bus_get_future(gio::BusType::Session).await?;
    // org.freedesktop.Application.ActivateAction(s, av, a{sv}); Settings'
    // "launch-panel" action takes (sav): the panel name and its arguments.
    let panel = (panel, Vec::<glib::Variant>::new()).to_variant();
    // Each element of the av is the parameter itself; wrapping it again
    // would make it a v.
    let args = ("launch-panel", vec![panel], HashMap::<String, glib::Variant>::new()).to_variant();
    bus.call_future(
        Some("org.gnome.Settings"),
        "/org/gnome/Settings",
        "org.freedesktop.Application",
        "ActivateAction",
        Some(&args),
        None,
        gio::DBusCallFlags::NONE,
        5000,
    )
    .await?;
    Ok(())
}
