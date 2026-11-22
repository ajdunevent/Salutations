//! The private D-Bus connection between the background process and the
//! window process: a socket pair made by the background process, whose
//! other end the window inherits as file descriptor 3. Calls go from the
//! window to the background process as `Call(name, args)`; everything the
//! background process tells the window is an `Event(name, data)` signal.

use glib::prelude::*;
use glib::translate::*;

pub const PATH: &str = "/net/dunevent/Salutations";
pub const IFACE: &str = "net.dunevent.Salutations.Mail";
pub const FD_ENV: &str = "SALUTATIONS_IPC_FD";
pub const FD: i32 = 3;
/// D-Bus errors carrying a GError's domain and code: `PREFIX.<quark>.<code>`.
const ERROR_PREFIX: &str = "net.dunevent.Salutations.Error";

pub const XML: &str = r#"<node>
  <interface name="net.dunevent.Salutations.Mail">
    <method name="Call">
      <arg type="s" name="name" direction="in"/>
      <arg type="v" name="args" direction="in"/>
      <arg type="v" name="result" direction="out"/>
    </method>
    <signal name="Event">
      <arg type="s" name="name"/>
      <arg type="v" name="data"/>
    </signal>
  </interface>
</node>"#;

/// A message's summary as the window lists it.
#[derive(Debug, Clone, Default, PartialEq, glib::Variant)]
pub struct Info {
    pub uid: String,
    pub flags: u32,
    pub user_flags: Vec<String>,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub preview: String,
    pub date_sent: i64,
    pub date_received: i64,
    pub message_id: u64,
    /// The headers Camel keeps in the summary; see `summary_headers`.
    pub headers: Vec<(String, String)>,
}

/// User flags the window reads.
pub const USER_FLAGS: [&str; 2] = ["$Forwarded", "$Junk"];

impl Info {
    pub fn from_camel(info: &camel::MessageInfo) -> Option<Info> {
        use camel::prelude::*;
        let s = |v: Option<glib::GString>| v.map(|s| s.to_string()).unwrap_or_default();
        Some(Info {
            uid: info.uid()?.to_string(),
            flags: info.flags(),
            user_flags: USER_FLAGS.iter().filter(|f| camel::message_info_user_flag(info, f)).map(|f| f.to_string()).collect(),
            from: s(info.from()),
            to: s(info.to()),
            cc: s(info.cc()),
            subject: s(info.subject()),
            preview: s(info.preview()),
            date_sent: info.date_sent(),
            date_received: info.date_received(),
            message_id: info.message_id(),
            headers: crate::summary_headers::WANTED
                .iter()
                .filter_map(|n| info.user_header(n).map(|v| (n.to_string(), v.to_string())))
                .collect(),
        })
    }
}

/// A thread as Camel builds it: a message and its replies.
#[derive(Debug, Clone, Default)]
pub struct Thread {
    pub uid: String,
    pub children: Vec<Thread>,
}

/// Recursive types cannot derive their variant type, so trees are sent in
/// preorder with each entry's depth.
pub fn flatten_threads(threads: &[Thread]) -> Vec<(String, u32)> {
    fn walk(t: &Thread, depth: u32, out: &mut Vec<(String, u32)>) {
        out.push((t.uid.clone(), depth));
        for c in &t.children {
            walk(c, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    for t in threads {
        walk(t, 0, &mut out);
    }
    out
}

pub fn unflatten_threads(flat: &[(String, u32)]) -> Vec<Thread> {
    unflatten(flat, |(_, d)| *d, |(u, _)| Thread { uid: u.clone(), children: Vec::new() }, |t| &mut t.children)
}

fn unflatten<E, T>(flat: &[E], depth: impl Fn(&E) -> u32, make: impl Fn(&E) -> T, children: impl Fn(&mut T) -> &mut Vec<T>) -> Vec<T> {
    let mut roots: Vec<T> = Vec::new();
    for e in flat {
        let mut level = &mut roots;
        for _ in 0..depth(e) {
            if level.is_empty() {
                break;
            }
            let last = level.len() - 1;
            level = children(&mut level[last]);
        }
        level.push(make(e));
    }
    roots
}

/// One folder's changes since the last event.
#[derive(Debug, Clone, Default, glib::Variant)]
pub struct Changes {
    pub account: String,
    pub folder: String,
    pub added: Vec<Info>,
    pub changed: Vec<Info>,
    pub removed: Vec<String>,
}

/// An account as the window shows it.
#[derive(Debug, Clone, Default, glib::Variant)]
pub struct AccountInfo {
    pub uid: String,
    /// "idle", "syncing", "offline", or "error".
    pub state: String,
    pub message: String,
    pub sign_in_failed: bool,
    pub paused: bool,
    pub connected: bool,
    pub special: Special,
    /// INBOX is open in the background process.
    pub inbox: bool,
}

/// `SpecialFolders` as sent; D-Bus has no optional values, so an empty
/// name means none.
#[derive(Debug, Clone, Default, PartialEq, glib::Variant)]
pub struct Special {
    pub known: bool,
    pub drafts: String,
    pub sent: String,
    pub trash: String,
    pub junk: String,
    pub archive: String,
    pub chosen: u32,
}

impl Special {
    pub fn from_folders(s: Option<&crate::SpecialFolders>) -> Special {
        let o = |v: &Option<String>| v.clone().unwrap_or_default();
        match s {
            None => Special::default(),
            Some(s) => Special {
                known: true,
                drafts: o(&s.drafts),
                sent: o(&s.sent),
                trash: o(&s.trash),
                junk: o(&s.junk),
                archive: o(&s.archive),
                chosen: s.chosen,
            },
        }
    }

    pub fn to_folders(&self) -> Option<crate::SpecialFolders> {
        let o = |v: &String| (!v.is_empty()).then(|| v.clone());
        self.known.then(|| crate::SpecialFolders {
            drafts: o(&self.drafts),
            sent: o(&self.sent),
            trash: o(&self.trash),
            junk: o(&self.junk),
            archive: o(&self.archive),
            chosen: self.chosen,
        })
    }
}

/// A `camel::FolderNode` as sent, with its depth in the tree.
#[derive(Debug, Clone, Default, glib::Variant)]
pub struct Node {
    pub full_name: String,
    pub display_name: String,
    pub flags: u32,
    pub unread: i32,
    pub total: i32,
    pub depth: u32,
}

pub fn flatten_nodes(nodes: &[camel::FolderNode]) -> Vec<Node> {
    fn walk(n: &camel::FolderNode, depth: u32, out: &mut Vec<Node>) {
        out.push(Node {
            full_name: n.full_name.clone(),
            display_name: n.display_name.clone(),
            flags: n.flags.bits(),
            unread: n.unread,
            total: n.total,
            depth,
        });
        for c in &n.children {
            walk(c, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    for n in nodes {
        walk(n, 0, &mut out);
    }
    out
}

pub fn unflatten_nodes(flat: &[Node]) -> Vec<camel::FolderNode> {
    unflatten(
        flat,
        |n| n.depth,
        |n| camel::FolderNode {
            full_name: n.full_name.clone(),
            display_name: n.display_name.clone(),
            flags: camel::FolderInfoFlags::from_bits_retain(n.flags),
            unread: n.unread,
            total: n.total,
            children: Vec::new(),
        },
        |n| &mut n.children,
    )
}

/// The D-Bus error name for `e`, keeping its domain and code.
pub fn error_name(e: &glib::Error) -> String {
    let quark: glib::Quark = unsafe { from_glib((*e.as_ptr()).domain) };
    let code = unsafe { (*e.as_ptr()).code };
    // D-Bus names allow only [A-Za-z0-9_] in each element.
    let domain: String = quark.as_str().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let encoded: String = quark.as_str().bytes().map(|b| format!("{b:02x}")).collect();
    format!("{ERROR_PREFIX}.{domain}.x{encoded}.c{}", code.max(0))
}

/// Rebuilds the error a remote call failed with, domain and code included.
pub fn error_from_remote(e: &glib::Error) -> glib::Error {
    let name = gio::DBusError::remote_error(e).map(|n| n.to_string()).unwrap_or_default();
    let mut stripped = e.clone();
    gio::DBusError::strip_remote_error(&mut stripped);
    let message = stripped.message().to_string();
    let parts: Vec<&str> = name.strip_prefix(ERROR_PREFIX).unwrap_or("").split('.').collect();
    let (Some(hex), Some(code)) = (
        parts.iter().find_map(|p| p.strip_prefix('x')),
        parts.iter().find_map(|p| p.strip_prefix('c')).and_then(|c| c.parse::<i32>().ok()),
    ) else {
        return glib::Error::new(gio::IOErrorEnum::Failed, &message);
    };
    let bytes: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()).collect();
    let domain = String::from_utf8_lossy(&bytes).to_string();
    let quark = glib::Quark::from_str(domain.as_str());
    unsafe { from_glib_full(glib::ffi::g_error_new_literal(quark.into_glib(), code, message.to_glib_none().0)) }
}

/// The variant for no value; D-Bus has no empty tuple.
pub fn unit() -> glib::Variant {
    false.to_variant()
}
