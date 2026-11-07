//! The first run's note about choosing Salutations as the default mail
//! app. Flatpak apps cannot see which app the desktop uses for `mailto:`
//! (the choice lives in the user's mimeapps.list, and the apps installed
//! natively and as Flatpaks), so Salutations does not check; it says once
//! where the choice is made.

use adw::prelude::*;
use gtk::glib;
use salutations_core::Settings;

pub fn first_run(parent: &impl IsA<gtk::Widget>) {
    let mut st = Settings::get();
    if st.default_app_asked {
        return;
    }
    st.default_app_asked = true;
    if let Err(e) = st.save() {
        glib::g_warning!("salutations", "Could not save settings: {}", e);
    }
    let dialog = adw::AlertDialog::new(
        Some("Default mail app"),
        Some("If you haven't already done so, Salutations can be set as your default mail app in Settings."),
    );
    dialog.add_responses(&[("continue", "_Continue"), ("settings", "_Open Settings")]);
    dialog.set_default_response(Some("continue"));
    dialog.set_close_response("continue");
    let parent = parent.clone().upcast::<gtk::Widget>();
    glib::spawn_future_local(async move {
        if dialog.choose_future(Some(&parent)).await == "settings" {
            crate::online_accounts::open_panel("applications");
        }
    });
}
