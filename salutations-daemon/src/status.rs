//! The line GNOME Shell shows under Background Apps, set through the
//! Background portal's SetStatus.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gio::prelude::*;
use salutations_core::engine::SyncState;

use crate::daemon::Daemon;

/// The portal's limit.
const MAX: usize = 96;

#[derive(Default)]
pub struct Status {
    sent: RefCell<String>,
    pending: RefCell<Option<glib::SourceId>>,
    /// When a sync last ended without error, any account.
    checked: Cell<Option<i64>>,
    twelve_hour: Cell<bool>,
}

fn sandboxed() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

/// `text` cut to `max` characters, with an ellipsis when cut.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

impl Daemon {
    /// Marks a finished sync for the "checked at" time.
    pub(crate) fn synced_now(&self) {
        self.status.checked.set(Some(glib::DateTime::now_local().map_or(0, |d| d.to_unix())));
    }

    /// Reads the desktop's clock format, then updates the status.
    pub(crate) fn read_clock(self: &Rc<Self>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(bus) = gio::bus_get_future(gio::BusType::Session).await
                && let Ok(v) = bus
                    .call_future(
                        Some("org.freedesktop.portal.Desktop"),
                        "/org/freedesktop/portal/desktop",
                        "org.freedesktop.portal.Settings",
                        "ReadOne",
                        Some(&("org.gnome.desktop.interface", "clock-format").to_variant()),
                        None,
                        gio::DBusCallFlags::NONE,
                        5000,
                    )
                    .await
            {
                let value = v.child_value(0).as_variant().and_then(|v| v.str().map(str::to_string));
                this.status.twelve_hour.set(value.as_deref() == Some("12h"));
            }
            this.update_status();
        });
    }

    fn status_text(&self) -> String {
        let accounts = self.engine.accounts();
        let failed: Vec<_> = accounts.iter().filter(|a| a.state() == SyncState::Error).collect();
        match failed.as_slice() {
            [] => {}
            [a] => {
                let what = if a.sign_in_failed() { ": sign-in needed" } else { ": can't connect" };
                return format!("{}{what}", cut(&a.account.display_name(), MAX - what.chars().count()));
            }
            many => return format!("{} accounts can't connect", many.len()),
        }
        let waiting = salutations_core::outbox::load().len();
        if waiting > 0 {
            return format!("{} not sent; will retry after the next check", plural(waiting, "message", "messages"));
        }
        if accounts.iter().any(|a| a.state() == SyncState::Syncing) {
            return "Checking for new mail…".into();
        }
        if accounts.iter().any(|a| a.state() == SyncState::Offline) {
            return "Offline; will check when the network returns".into();
        }
        if !accounts.is_empty() && accounts.iter().all(|a| self.engine.paused(a)) {
            return "Paused in power save mode".into();
        }
        let unread: i64 = accounts.iter().map(|a| self.extra(a.uid()).unread.get().unwrap_or(0)).sum();
        let unread = if unread == 0 { "No unread mail".to_string() } else { format!("{unread} unread") };
        let at = self.status.checked.get().and_then(|t| glib::DateTime::from_unix_local(t).ok()).and_then(|d| {
            let f = if self.status.twelve_hour.get() { "%l:%M %p" } else { "%H:%M" };
            d.format(f).ok().map(|s| s.trim().to_string())
        });
        match at {
            Some(at) => format!("{unread} · checked at {at}"),
            None => unread,
        }
    }

    /// Sends the status when it changed, a second after the last change so
    /// a quick sync does not flicker.
    pub(crate) fn update_status(self: &Rc<Self>) {
        if !sandboxed() {
            return;
        }
        if let Some(id) = self.status.pending.borrow_mut().take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(std::time::Duration::from_secs(1), move || {
            let Some(this) = weak.upgrade() else { return };
            this.status.pending.borrow_mut().take();
            let text = cut(&this.status_text(), MAX);
            if *this.status.sent.borrow() == text {
                return;
            }
            *this.status.sent.borrow_mut() = text.clone();
            glib::spawn_future_local(async move {
                let Ok(bus) = gio::bus_get_future(gio::BusType::Session).await else { return };
                let options = glib::VariantDict::new(None);
                options.insert_value("message", &text.to_variant());
                let result = bus
                    .call_future(
                        Some("org.freedesktop.portal.Desktop"),
                        "/org/freedesktop/portal/desktop",
                        "org.freedesktop.portal.Background",
                        "SetStatus",
                        Some(&glib::Variant::tuple_from_iter([options.end()])),
                        None,
                        gio::DBusCallFlags::NONE,
                        -1,
                    )
                    .await;
                if let Err(e) = result {
                    salutations_core::debug::log(&format!("background status: {}", e.message()));
                }
            });
        });
        *self.status.pending.borrow_mut() = Some(id);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cut_keeps_the_limit() {
        assert_eq!(super::cut("abc", 5), "abc");
        assert_eq!(super::cut("abcdef", 4), "abc…");
        assert_eq!(super::cut(&"x".repeat(200), 96).chars().count(), 96);
    }
}
