//! Starting at login. Running with the window closed is the background
//! process's; it follows the setting from the settings file.
//!
//! Outside a sandbox, autostart is a desktop file in
//! `~/.config/autostart`; inside Flatpak the Background portal writes it.
//! Either way it runs Salutations with `--background`, which starts it
//! without a window.

use gtk::prelude::*;
use gtk::{gio, glib};
use salutations_core::{APP_ID, Settings};

/// Starts without showing the window.
pub const FLAG: &str = "--background";

fn sandboxed() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

/// Writes or removes the autostart entry; called when either switch
/// changes, since the portal may ask the user.
pub fn apply_autostart() {
    let settings = Settings::get();
    set_autostart(settings.run_in_background && settings.autostart);
}

fn autostart_file() -> std::path::PathBuf {
    glib::user_config_dir().join("autostart").join(format!("{APP_ID}.desktop"))
}

fn set_autostart(on: bool) {
    if sandboxed() {
        request_portal(on);
        return;
    }
    let path = autostart_file();
    if !on {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let exec = glib::shell_quote(&exe);
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Salutations\nExec={} {FLAG}\nIcon={APP_ID}\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
        exec.to_string_lossy()
    );
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, entry) {
        glib::g_warning!("salutations", "Cannot write {}: {}", path.display(), e);
    }
}

/// Asks the Background portal for autostart; the user may be asked once.
fn request_portal(autostart: bool) {
    glib::spawn_future_local(async move {
        let Ok(bus) = gio::bus_get_future(gio::BusType::Session).await else { return };
        let options = glib::VariantDict::new(None);
        options.insert_value("reason", &"Check for new mail with the window closed".to_variant());
        options.insert_value("autostart", &autostart.to_variant());
        options.insert_value("commandline", &vec!["salutations".to_string(), FLAG.to_string()].to_variant());
        let result = bus
            .call_future(
                Some("org.freedesktop.portal.Desktop"),
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Background",
                "RequestBackground",
                Some(&glib::Variant::tuple_from_iter(["".to_variant(), options.end()])),
                None,
                gio::DBusCallFlags::NONE,
                -1,
            )
            .await;
        if let Err(e) = result {
            glib::g_warning!("salutations", "Background portal: {}", e);
        }
    });
}
