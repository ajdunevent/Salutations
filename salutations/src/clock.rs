//! GNOME's 12- or 24-hour clock setting (`clock-format` in
//! org.gnome.desktop.interface), read through the desktop settings portal.
//! A Flatpak cannot read the host's GSettings directly; the portal answers
//! both inside and outside a sandbox.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::{gio, glib};

const NAMESPACE: &str = "org.gnome.desktop.interface";
const KEY: &str = "clock-format";

thread_local! {
    static USE_24H: Cell<bool> = const { Cell::new(false) };
    static LISTENERS: RefCell<Vec<Box<dyn Fn()>>> = const { RefCell::new(Vec::new()) };
    static WATCH: RefCell<Option<gio::SignalSubscription>> = const { RefCell::new(None) };
}

pub fn use_24h() -> bool {
    USE_24H.get()
}

pub fn connect_changed(f: impl Fn() + 'static) {
    LISTENERS.with(|l| l.borrow_mut().push(Box::new(f)));
}

fn set(value: &glib::Variant) {
    // The portal passes the value as a string nick, possibly boxed.
    let value = value.as_variant().unwrap_or_else(|| value.clone());
    let Some(nick) = value.str() else { return };
    let h24 = nick == "24h";
    if USE_24H.replace(h24) != h24 {
        LISTENERS.with(|l| l.borrow().iter().for_each(|f| f()));
    }
}

/// Reads the setting and follows its changes; call once at startup. Until
/// the answer arrives, and without a portal, times use the 12-hour clock.
pub fn init() {
    glib::spawn_future_local(async {
        let Ok(bus) = gio::bus_get_future(gio::BusType::Session).await else { return };
        let call = |method: &'static str| {
            bus.call_future(
                Some("org.freedesktop.portal.Desktop"),
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Settings",
                method,
                Some(&(NAMESPACE, KEY).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                5000,
            )
        };
        // ReadOne is portal version 2; Read wraps the value once more.
        let reply = match call("ReadOne").await {
            Ok(r) => Some(r),
            Err(_) => call("Read").await.ok(),
        };
        if let Some(v) = reply.and_then(|r| r.try_child_value(0)) {
            set(&v);
        }
        let sub = bus.subscribe_to_signal(
            Some("org.freedesktop.portal.Desktop"),
            Some("org.freedesktop.portal.Settings"),
            Some("SettingChanged"),
            Some("/org/freedesktop/portal/desktop"),
            None,
            gio::DBusSignalFlags::NONE,
            |signal| {
                let p = signal.parameters;
                let is_ours = p.try_child_value(0).and_then(|v| v.get::<String>()).as_deref() == Some(NAMESPACE)
                    && p.try_child_value(1).and_then(|v| v.get::<String>()).as_deref() == Some(KEY);
                if is_ours && let Some(v) = p.try_child_value(2) {
                    set(&v);
                }
            },
        );
        WATCH.with(|w| *w.borrow_mut() = Some(sub));
    });
}

/// A time of day in the user's clock format.
pub fn time_format() -> &'static str {
    if use_24h() { "%H:%M" } else { "%l:%M %p" }
}
