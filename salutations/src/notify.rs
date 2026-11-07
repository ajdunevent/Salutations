//! New-mail notifications and the unread badge.
//!
//! Notifications go to `org.freedesktop.Notifications`, the interface GNOME
//! Shell and Phosh both serve, because it carries what GNotification cannot:
//! a per-notification sound choice (`suppress-sound`) and an image
//! (`image-data`). Its actions reach only the running process; a
//! notification is withdrawn when Salutations quits.
//!
//! The badge is the Unity launcher API (`com.canonical.Unity.LauncherEntry`)
//! that Dash to Dock, Plank and KDE read: an `Update` signal on each change,
//! and a `Query` method answering docks that start (or restart) later.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{cairo, gio, glib};
use salutations_core::APP_ID;

const BUS: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// What a notification stands for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Subject {
    Message { account: String, uid: String },
    Combined { account: String },
}

/// What the user did with a notification.
#[derive(Debug, Clone)]
pub enum Action {
    Open { account: String, uid: String },
    Delete { account: String, uid: String },
    Junk { account: String, uid: String },
    Reply { account: String, uid: String },
    OpenInbox { account: String },
}

pub struct MessageNote<'a> {
    pub account: &'a str,
    pub uid: &'a str,
    pub sender: &'a str,
    /// The address book's name for the sender, for the icon's initials.
    pub contact_name: Option<String>,
    pub photo: Option<gtk::gdk::Texture>,
    pub subject: &'a str,
    pub preview: &'a str,
    pub sound: bool,
    /// Offer Junk; false when the account has no Junk folder.
    pub junk: bool,
}

type ActionHandler = Rc<dyn Fn(Action)>;

pub struct Notifier {
    bus: RefCell<Option<gio::DBusConnection>>,
    /// Server ID of each shown notification.
    ids: RefCell<HashMap<Subject, u32>>,
    on_action: RefCell<Option<ActionHandler>>,
    subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    /// A badge set before the bus was ready.
    badge: RefCell<Option<i64>>,
    /// The count `Query` answers with; read from the bus's thread.
    count: std::sync::Arc<std::sync::atomic::AtomicI64>,
    entry: RefCell<Option<gio::RegistrationId>>,
}

const ENTRY_PATH: &str = "/net/dunevent/Salutations/LauncherEntry";
const ENTRY_XML: &str = r#"<node>
  <interface name="com.canonical.Unity.LauncherEntry">
    <method name="Query">
      <arg type="s" name="app_uri" direction="out"/>
      <arg type="a{sv}" name="properties" direction="out"/>
    </method>
    <signal name="Update">
      <arg type="s" name="app_uri"/>
      <arg type="a{sv}" name="properties"/>
    </signal>
  </interface>
</node>"#;

/// The Update and Query payload for `count`.
fn entry_payload(count: i64) -> glib::Variant {
    let props = glib::VariantDict::new(None);
    props.insert_value("count", &count.to_variant());
    props.insert_value("count-visible", &(count > 0).to_variant());
    glib::Variant::tuple_from_iter([format!("application://{APP_ID}.desktop").to_variant(), props.end()])
}

impl Notifier {
    pub fn new() -> Rc<Self> {
        let this = Rc::new(Self {
            bus: RefCell::new(None),
            ids: RefCell::new(HashMap::new()),
            on_action: RefCell::new(None),
            subscriptions: RefCell::new(Vec::new()),
            badge: RefCell::new(None),
            count: Default::default(),
            entry: RefCell::new(None),
        });
        let weak = Rc::downgrade(&this);
        glib::spawn_future_local(async move {
            let Ok(bus) = gio::bus_get_future(gio::BusType::Session).await else { return };
            let Some(this) = weak.upgrade() else { return };
            let w = weak.clone();
            let invoked = bus.subscribe_to_signal(
                Some(BUS),
                Some(BUS),
                Some("ActionInvoked"),
                Some(PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |s| {
                    if let (Some(this), Some((id, key))) = (w.upgrade(), s.parameters.get::<(u32, String)>()) {
                        this.invoked(id, &key);
                    }
                },
            );
            let w = weak.clone();
            let closed = bus.subscribe_to_signal(
                Some(BUS),
                Some(BUS),
                Some("NotificationClosed"),
                Some(PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |s| {
                    if let (Some(this), Some((id, _))) = (w.upgrade(), s.parameters.get::<(u32, u32)>()) {
                        this.ids.borrow_mut().retain(|_, v| *v != id);
                    }
                },
            );
            *this.subscriptions.borrow_mut() = vec![invoked, closed];
            if let Ok(node) = gio::DBusNodeInfo::for_xml(ENTRY_XML)
                && let Some(iface) = node.lookup_interface("com.canonical.Unity.LauncherEntry")
            {
                let count = this.count.clone();
                let id = bus
                    .register_object(ENTRY_PATH, &iface)
                    .method_call(move |_, _, _, _, method, _, invocation| {
                        if method == "Query" {
                            let n = count.load(std::sync::atomic::Ordering::Relaxed);
                            invocation.return_value(Some(&entry_payload(n)));
                        } else {
                            invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method);
                        }
                    })
                    .build();
                *this.entry.borrow_mut() = id.ok();
            }
            *this.bus.borrow_mut() = Some(bus);
            if let Some(n) = this.badge.borrow_mut().take() {
                this.set_badge(n);
            }
        });
        this
    }

    pub fn connect_action(&self, f: impl Fn(Action) + 'static) {
        *self.on_action.borrow_mut() = Some(Rc::new(f));
    }

    fn invoked(&self, id: u32, key: &str) {
        let subject = self.ids.borrow().iter().find(|(_, v)| **v == id).map(|(k, _)| k.clone());
        let Some(subject) = subject else { return };
        let action = match (&subject, key) {
            (Subject::Message { account, uid }, "default") => Action::Open { account: account.clone(), uid: uid.clone() },
            (Subject::Message { account, uid }, "delete") => Action::Delete { account: account.clone(), uid: uid.clone() },
            (Subject::Message { account, uid }, "junk") => Action::Junk { account: account.clone(), uid: uid.clone() },
            (Subject::Message { account, uid }, "reply") => Action::Reply { account: account.clone(), uid: uid.clone() },
            (Subject::Combined { account }, "default" | "open-inbox") => Action::OpenInbox { account: account.clone() },
            _ => return,
        };
        // Servers close a notification after one of its actions.
        self.ids.borrow_mut().remove(&subject);
        let cb = self.on_action.borrow().clone();
        if let Some(cb) = cb {
            cb(action);
        }
    }

    pub fn message(&self, m: MessageNote) {
        let sender = if m.sender.is_empty() { "Unknown sender" } else { m.sender };
        let subject = if m.subject.is_empty() { "(No subject)" } else { m.subject };
        let body = if m.preview.is_empty() {
            escape(subject)
        } else {
            format!("<b>{}</b>\n{}", escape(subject), escape(m.preview))
        };
        let mut actions = vec![("default", "Open"), ("reply", "Reply"), ("delete", "Delete")];
        if m.junk {
            actions.push(("junk", "Junk"));
        }
        let key = Subject::Message { account: m.account.into(), uid: m.uid.into() };
        let icon = avatar(m.contact_name.as_deref().unwrap_or(sender), m.photo.as_ref());
        self.send(key, sender, &body, &actions, Some(icon), m.sound);
    }

    /// Replaces an existing combined notification in place.
    pub fn combined(&self, account: &str, account_name: &str, count: usize, senders: &[String], sound: bool) {
        let title = match count {
            1 => format!("1 new message in {account_name}"),
            n => format!("{n} new messages in {account_name}"),
        };
        let mut names: Vec<String> = Vec::new();
        for s in senders {
            if !names.contains(s) {
                names.push(s.clone());
            }
        }
        let body = match names.len() {
            0 => String::new(),
            1..=3 => escape(&names.join(", ")),
            n => escape(&format!("{} and {} others", names[..2].join(", "), n - 2)),
        };
        let key = Subject::Combined { account: account.into() };
        self.send(key, &title, &body, &[("default", "Open Inbox"), ("open-inbox", "Open Inbox")], None, sound);
    }

    fn send(&self, key: Subject, title: &str, body: &str, actions: &[(&str, &str)], image: Option<glib::Variant>, sound: bool) {
        let Some(bus) = self.bus.borrow().clone() else { return };
        let replaces = self.ids.borrow().get(&key).copied().unwrap_or(0);
        let actions: Vec<String> = actions.iter().flat_map(|(k, l)| [k.to_string(), l.to_string()]).collect();
        let hints = glib::VariantDict::new(None);
        hints.insert_value("desktop-entry", &APP_ID.to_variant());
        hints.insert_value("category", &"email.arrived".to_variant());
        // An update of a shown notification never makes a sound.
        if sound && replaces == 0 {
            hints.insert_value("sound-name", &"message-new-email".to_variant());
        } else {
            hints.insert_value("suppress-sound", &true.to_variant());
        }
        if let Some(image) = image {
            hints.insert_value("image-data", &image);
        }
        let params = glib::Variant::tuple_from_iter([
            "Salutations".to_variant(),
            replaces.to_variant(),
            APP_ID.to_variant(),
            title.to_variant(),
            body.to_variant(),
            actions.to_variant(),
            hints.end(),
            (-1i32).to_variant(),
        ]);
        // Synchronous, so a quick replacement gets this notification's ID.
        let result = bus.call_sync(
            Some(BUS),
            PATH,
            BUS,
            "Notify",
            Some(&params),
            Some(glib::VariantTy::new("(u)").expect("type")),
            gio::DBusCallFlags::NONE,
            2000,
            None::<&gio::Cancellable>,
        );
        match result {
            Ok(v) => {
                if let Some((id,)) = v.get::<(u32,)>() {
                    self.ids.borrow_mut().insert(key, id);
                }
            }
            Err(e) => glib::g_warning!("salutations", "Cannot show a notification: {}", e),
        }
    }

    pub fn withdraw(&self, key: &Subject) {
        let Some(id) = self.ids.borrow_mut().remove(key) else { return };
        if let Some(bus) = self.bus.borrow().clone() {
            bus.call(
                Some(BUS),
                PATH,
                BUS,
                "CloseNotification",
                Some(&(id,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                2000,
                None::<&gio::Cancellable>,
                |_| {},
            );
        }
    }

    /// Withdraws everything before quitting; synchronous, since the
    /// process ends right after.
    pub fn withdraw_all(&self) {
        let ids: Vec<u32> = self.ids.borrow_mut().drain().map(|(_, v)| v).collect();
        let Some(bus) = self.bus.borrow().clone() else { return };
        for id in ids {
            let _ = bus.call_sync(
                Some(BUS),
                PATH,
                BUS,
                "CloseNotification",
                Some(&(id,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                1000,
                None::<&gio::Cancellable>,
            );
        }
        self.set_badge(0);
        let _ = bus.flush_sync(None::<&gio::Cancellable>);
    }

    /// The unread count on the dock icon; 0 hides it.
    pub fn set_badge(&self, count: i64) {
        self.count.store(count, std::sync::atomic::Ordering::Relaxed);
        let Some(bus) = self.bus.borrow().clone() else {
            *self.badge.borrow_mut() = Some(count);
            return;
        };
        let _ = bus.emit_signal(None, ENTRY_PATH, "com.canonical.Unity.LauncherEntry", "Update", Some(&entry_payload(count)));
    }
}

fn escape(s: &str) -> String {
    glib::markup_escape_text(s).to_string()
}

/// AdwAvatar's colours (font, gradient top, gradient bottom), from
/// libadwaita's _avatar.scss; the index is `g_str_hash(text) % 14`.
const COLORS: [(u32, u32, u32); 14] = [
    (0xcfe1f5, 0x83b6ec, 0x337fdc),
    (0xcaeaf2, 0x7ad9f1, 0x0f9ac8),
    (0xcef8d8, 0x8de6b1, 0x29ae74),
    (0xe6f9d7, 0xb5e98a, 0x6ab85b),
    (0xf9f4e1, 0xf8e359, 0xd29d09),
    (0xffead1, 0xffcb62, 0xd68400),
    (0xffe5c5, 0xffa95a, 0xed5b00),
    (0xf8d2ce, 0xf78773, 0xe62d42),
    (0xfac7de, 0xe973ab, 0xe33b6a),
    (0xe7c2e8, 0xcb78d4, 0x9945b5),
    (0xd5d2f5, 0x9e91e8, 0x7a59ca),
    (0xf2eade, 0xe3cf9c, 0xb08952),
    (0xe5d6ca, 0xbe916d, 0x785336),
    (0xd8d7d3, 0xc0bfbc, 0x6e6d71),
];

/// GLib's `g_str_hash`, which hashes signed chars.
fn str_hash(s: &str) -> u32 {
    s.bytes().fold(5381u32, |h, b| h.wrapping_mul(33).wrapping_add(b as i8 as i32 as u32))
}

/// AdwAvatar's initials: the first character, and the first after the
/// last space.
fn initials(text: &str) -> String {
    let t = text.trim().to_uppercase();
    let mut out: String = t.chars().take(1).collect();
    if let Some(i) = t.rfind(' ')
        && let Some(c) = t[i + 1..].chars().next()
    {
        out.push(c);
    }
    out
}

/// The avatar the list shows for `name`, or `photo`, as `image-data`.
/// AdwAvatar can only render inside a realized window, so it is drawn here.
fn avatar(name: &str, photo: Option<&gtk::gdk::Texture>) -> glib::Variant {
    const SIZE: i32 = 64;
    let (fg, top, bottom) = COLORS[(str_hash(name) % 14) as usize];
    let rgb = |c: u32| (((c >> 16) & 0xff) as f64 / 255.0, ((c >> 8) & 0xff) as f64 / 255.0, (c & 0xff) as f64 / 255.0);
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, SIZE, SIZE).expect("surface");
    {
        let cr = cairo::Context::new(&surface).expect("context");
        let grad = cairo::LinearGradient::new(0.0, 0.0, 0.0, SIZE as f64);
        let (r, g, b) = rgb(top);
        grad.add_color_stop_rgb(0.0, r, g, b);
        let (r, g, b) = rgb(bottom);
        grad.add_color_stop_rgb(1.0, r, g, b);
        let half = SIZE as f64 / 2.0;
        cr.arc(half, half, half, 0.0, std::f64::consts::TAU);
        let _ = cr.set_source(&grad);
        let _ = cr.fill();
        if let Some(p) = photo {
            paint_photo(&cr, p, SIZE as f64);
        }
        let (r, g, b) = rgb(fg);
        cr.set_source_rgb(r, g, b);
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(SIZE as f64 * 0.38);
        let text = if photo.is_some() { String::new() } else { initials(name) };
        if let Ok(ext) = cr.text_extents(&text) {
            cr.move_to(
                half - ext.width() / 2.0 - ext.x_bearing(),
                half - ext.height() / 2.0 - ext.y_bearing(),
            );
            let _ = cr.show_text(&text);
        }
    }
    surface.flush();
    let stride = surface.stride();
    let data = surface.data().expect("surface data");
    // Cairo stores premultiplied native-endian ARGB; the hint wants RGBA.
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let i = (y * stride + x * 4) as usize;
            let px = u32::from_ne_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
            let a = (px >> 24) & 0xff;
            let un = |c: u32| if a == 0 { 0 } else { ((c * 255 + a / 2) / a).min(255) as u8 };
            rgba.extend_from_slice(&[un((px >> 16) & 0xff), un((px >> 8) & 0xff), un(px & 0xff), a as u8]);
        }
    }
    let data = glib::Variant::array_from_fixed_array(&rgba);
    glib::Variant::tuple_from_iter([
        SIZE.to_variant(),
        SIZE.to_variant(),
        (SIZE * 4).to_variant(),
        true.to_variant(),
        8i32.to_variant(),
        4i32.to_variant(),
        data,
    ])
}

/// Paints `photo` over the avatar circle at `size`, clipped to it.
fn paint_photo(cr: &cairo::Context, photo: &gtk::gdk::Texture, size: f64) {
    let (w, h) = (photo.width(), photo.height());
    let Ok(mut surface) = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h) else { return };
    let stride = surface.stride() as usize;
    {
        let Ok(mut data) = surface.data() else { return };
        // The default download format is Cairo's ARGB32.
        photo.download(&mut data, stride);
    }
    surface.mark_dirty();
    let _ = cr.save();
    let half = size / 2.0;
    cr.arc(half, half, half, 0.0, std::f64::consts::TAU);
    cr.clip();
    cr.scale(size / w as f64, size / h as f64);
    let _ = cr.set_source_surface(&surface, 0.0, 0.0);
    let _ = cr.paint();
    let _ = cr.restore();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_like_adw() {
        assert_eq!(initials("Elena Ruiz"), "ER");
        assert_eq!(initials("Priya"), "P");
        assert_eq!(initials("  sam  okafor "), "SO");
    }

    #[test]
    fn hash_like_glib() {
        // g_str_hash("a") = 5381 * 33 + 97
        assert_eq!(str_hash("a"), 177670);
    }
}
