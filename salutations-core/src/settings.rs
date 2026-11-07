//! App preferences in `~/.config/salutations/settings.ini`.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::offline::{Download, Refresh, TimeUnit, Window};

const READ: &str = "read";
const REMOTE: &str = "remote-content";
const ACCOUNTS: &str = "accounts";
const WINDOW: &str = "window";
const BACKGROUND: &str = "background";
const NOTIFY: &str = "notifications";
const VIEW: &str = "view";
const AVATARS: &str = "avatars";
const CRYPTO: &str = "encryption";
const MAIL_APP: &str = "default-mail-app";
/// Followed by the account UID: one group per account.
const COMPOSE: &str = "compose:";
/// Followed by the account UID.
const SYNC: &str = "sync:";

/// How an account checks and downloads, besides what its EDS source keeps.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SyncPrefs {
    /// On metered connections; None is the same as otherwise.
    pub metered_refresh: Option<Refresh>,
    pub metered_download: Option<Download>,
    /// "Download From" is Never. The source's window is kept for when it
    /// is set again.
    pub never_download: bool,
    /// Nothing automatic happens while Power Saver is on.
    pub pause_power_save: bool,
}

fn refresh_key(r: Refresh) -> String {
    match r {
        Refresh::Minutes(m) => m.to_string(),
        Refresh::Never => "never".into(),
    }
}

fn refresh_from_key(k: &str) -> Option<Refresh> {
    if k == "never" { Some(Refresh::Never) } else { k.parse().ok().filter(|m| *m > 0).map(Refresh::Minutes) }
}

fn download_key(d: Download) -> String {
    match d {
        Download::Never => "never".into(),
        Download::Within(Window::AllTime) => "all".into(),
        Download::Within(Window::Last(unit, n)) => {
            let u = match unit {
                TimeUnit::Days => "d",
                TimeUnit::Weeks => "w",
                TimeUnit::Months => "m",
                TimeUnit::Years => "y",
            };
            format!("{n}{u}")
        }
    }
}

fn download_from_key(k: &str) -> Option<Download> {
    match k {
        "never" => return Some(Download::Never),
        "all" => return Some(Download::Within(Window::AllTime)),
        _ => {}
    }
    let (n, u) = k.split_at(k.len().checked_sub(1)?);
    let unit = match u {
        "d" => TimeUnit::Days,
        "w" => TimeUnit::Weeks,
        "m" => TimeUnit::Months,
        "y" => TimeUnit::Years,
        _ => return None,
    };
    n.parse().ok().filter(|n| *n > 0).map(|n| Download::Within(Window::Last(unit, n)))
}

/// How an account's new messages start.
#[derive(Debug, Clone, PartialEq)]
pub struct ComposePrefs {
    pub format: crate::compose::Format,
    pub signature: String,
    pub include_signature: bool,
    /// The signature goes above the quoted message instead of at the end.
    pub signature_above: bool,
    /// Fingerprint of the OpenPGP key that signs; None turns signing,
    /// encryption and key sharing off for the account.
    pub signing_key: Option<String>,
    /// New messages and forwards start with Sign & Encrypt.
    pub encrypt_new: bool,
    /// Replies to encrypted messages start with Sign & Encrypt.
    pub encrypt_replies: bool,
    /// Send the public key in an Autocrypt header.
    pub autocrypt: bool,
    /// Also attach the public key; only while `autocrypt` is on.
    pub attach_key: AttachKey,
}

impl Default for ComposePrefs {
    fn default() -> Self {
        Self {
            format: crate::compose::Format::Markdown,
            signature: String::new(),
            include_signature: true,
            signature_above: false,
            signing_key: None,
            encrypt_new: false,
            encrypt_replies: true,
            autocrypt: true,
            attach_key: AttachKey::Never,
        }
    }
}

/// When the public key is attached to messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachKey {
    Always,
    WhenProtected,
    Never,
}

/// What to do with keys that trusted messages carry for their senders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKeys {
    Always,
    Prompt,
    Never,
}

macro_rules! keyed {
    ($t:ty, $($v:ident => $k:literal),*) => {
        impl $t {
            pub fn key(self) -> &'static str {
                match self { $(Self::$v => $k),* }
            }
            pub fn from_key(k: &str) -> Option<Self> {
                match k { $($k => Some(Self::$v),)* _ => None }
            }
        }
    };
}
keyed!(AttachKey, Always => "always", WhenProtected => "when-protected", Never => "never");
keyed!(ImportKeys, Always => "always", Prompt => "prompt", Never => "never");

/// The default keyserver for looking up recipients' keys.
pub const DEFAULT_KEYSERVER: &str = "hkps://keys.openpgp.org";

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Mark a message read after it has been shown for `mark_read_delay`.
    pub mark_read: bool,
    /// Seconds; 0 marks it read as soon as it is shown.
    pub mark_read_delay: u32,
    /// With `mark_read`: instead of after a delay, when a reply to or
    /// forward of the message is sent.
    pub read_on_response: bool,
    /// Plain-text mail in the monospace font instead of the interface font.
    pub plain_monospace: bool,
    /// Ask before opening any link. Links whose text names another site
    /// ask regardless.
    pub confirm_links: bool,
    /// The link dialog's "Remove site tracking" choice, kept from its last
    /// use. Also applies when links open without asking.
    pub remove_tracking: bool,
    /// Lowercased From addresses whose remote content always loads.
    pub remote_senders: Vec<String>,
    /// Lowercased domains whose remote content always loads.
    pub remote_domains: Vec<String>,
    /// Allowed remote content is fetched without known tracking parameters
    /// and parameters carrying the account's address.
    pub remote_strip: bool,
    /// Account UIDs in the order the user set; accounts not listed follow,
    /// by name.
    pub account_order: Vec<String>,
    /// The account for `mailto:` links and other messages started outside
    /// a folder; None or a missing account means the first.
    pub default_account: Option<String>,
    /// The first run's note about the default mail app was shown.
    pub default_app_asked: bool,
    /// The widest the message list may be, in sp, once the user has
    /// dragged its edge.
    pub list_width: Option<f64>,
    /// In wide windows the message goes below the list rather than beside
    /// it; narrow windows show one pane either way.
    pub message_below: bool,
    /// The message view shows two lines of headers; the user's last choice.
    pub compact_headers: bool,
    /// Messages moved to Trash or Junk are marked read first.
    pub read_on_delete: bool,
    pub read_on_junk: bool,
    /// Opt-in avatar sources; each sends requests about senders.
    pub avatar_bimi: bool,
    pub avatar_libravatar: bool,
    pub avatar_gravatar: bool,
    /// The list's height in px when the message is below it, once the
    /// user has dragged the divider.
    pub list_height: Option<i32>,
    /// Closing the window keeps Salutations running.
    pub run_in_background: bool,
    /// Start hidden at login; needs `run_in_background`.
    pub autostart: bool,
    /// Account UIDs whose notifications make no sound.
    pub silent_accounts: Vec<String>,
    /// New messages shown as notifications of their own per account; more
    /// are combined into one. None shows every message on its own; 0
    /// always combines.
    pub notification_limit: Option<u32>,
    /// The message list's sort menu, for every folder.
    pub newest_first: bool,
    pub threaded: bool,
    /// Per account UID.
    pub compose: std::collections::BTreeMap<String, ComposePrefs>,
    /// Per account UID.
    pub sync: std::collections::BTreeMap<String, SyncPrefs>,
    pub import_keys: ImportKeys,
    pub keyserver: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mark_read: true,
            mark_read_delay: 2,
            read_on_response: false,
            plain_monospace: false,
            confirm_links: true,
            remove_tracking: true,
            remote_senders: Vec::new(),
            remote_domains: Vec::new(),
            remote_strip: false,
            account_order: Vec::new(),
            default_account: None,
            default_app_asked: false,
            list_width: None,
            message_below: false,
            compact_headers: false,
            read_on_delete: true,
            read_on_junk: true,
            avatar_bimi: false,
            avatar_libravatar: false,
            avatar_gravatar: false,
            list_height: None,
            run_in_background: false,
            autostart: false,
            silent_accounts: Vec::new(),
            notification_limit: Some(4),
            newest_first: true,
            threaded: true,
            compose: Default::default(),
            sync: Default::default(),
            import_keys: ImportKeys::Prompt,
            keyserver: DEFAULT_KEYSERVER.to_string(),
        }
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("salutations").join("settings.ini")
}

static CURRENT: Mutex<Option<Settings>> = Mutex::new(None);

impl Settings {
    /// The settings as last loaded or saved in this process.
    pub fn get() -> Settings {
        let mut cur = CURRENT.lock().unwrap();
        cur.get_or_insert_with(Self::load).clone()
    }

    fn load() -> Settings {
        let mut s = Settings::default();
        let kf = glib::KeyFile::new();
        if kf.load_from_file(path(), glib::KeyFileFlags::NONE).is_err() {
            return s;
        }
        if let Ok(v) = kf.boolean(READ, "mark-read") {
            s.mark_read = v;
        }
        if let Ok(v) = kf.integer(READ, "delay") {
            s.mark_read_delay = v.clamp(0, 86_400) as u32;
        }
        s.read_on_response = kf.boolean(READ, "on-response").unwrap_or(false);
        if let Ok(v) = kf.boolean(READ, "plain-monospace") {
            s.plain_monospace = v;
        }
        if let Ok(v) = kf.boolean(READ, "confirm-links") {
            s.confirm_links = v;
        }
        if let Ok(v) = kf.boolean(READ, "remove-tracking") {
            s.remove_tracking = v;
        }
        let list = |key: &str| {
            kf.string_list(REMOTE, key)
                .map(|l| {
                    l.iter()
                        .map(|v| v.to_ascii_lowercase())
                        .filter(|v| !v.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        };
        s.remote_senders = list("senders");
        s.remote_domains = list("domains");
        s.remote_strip = kf.boolean(REMOTE, "strip-tracking").unwrap_or(false);
        s.account_order = kf
            .string_list(ACCOUNTS, "order")
            .map(|l| l.iter().map(|v| v.to_string()).filter(|v| !v.is_empty()).collect())
            .unwrap_or_default();
        s.default_account = kf.string(ACCOUNTS, "default").ok().map(|v| v.to_string()).filter(|v| !v.is_empty());
        s.default_app_asked = kf.boolean(MAIL_APP, "asked").unwrap_or(false);
        s.run_in_background = kf.boolean(BACKGROUND, "run").unwrap_or(false);
        s.autostart = kf.boolean(BACKGROUND, "autostart").unwrap_or(false);
        s.silent_accounts = kf
            .string_list(NOTIFY, "silent")
            .map(|l| l.iter().map(|v| v.to_string()).filter(|v| !v.is_empty()).collect())
            .unwrap_or_default();
        // -1 means no limit.
        if let Ok(n) = kf.integer(NOTIFY, "limit") {
            s.notification_limit = u32::try_from(n).ok();
        }
        s.newest_first = kf.boolean(VIEW, "newest-first").unwrap_or(true);
        s.threaded = kf.boolean(VIEW, "threaded").unwrap_or(true);
        s.list_width = kf.double(WINDOW, "list-width").ok().filter(|w| w.is_finite() && *w > 0.0);
        s.message_below = kf.boolean(VIEW, "message-below").unwrap_or(false);
        s.compact_headers = kf.boolean(VIEW, "compact-headers").unwrap_or(false);
        s.read_on_delete = kf.boolean(READ, "on-delete").unwrap_or(true);
        s.read_on_junk = kf.boolean(READ, "on-junk").unwrap_or(true);
        s.avatar_bimi = kf.boolean(AVATARS, "bimi").unwrap_or(false);
        s.avatar_libravatar = kf.boolean(AVATARS, "libravatar").unwrap_or(false);
        s.avatar_gravatar = kf.boolean(AVATARS, "gravatar").unwrap_or(false);
        s.list_height = kf.integer(WINDOW, "list-height").ok().filter(|h| *h > 0);
        for group in kf.groups().iter() {
            let Some(uid) = group.strip_prefix(SYNC).filter(|u| !u.is_empty()) else { continue };
            let prefs = SyncPrefs {
                metered_refresh: kf.string(group, "metered-refresh").ok().and_then(|k| refresh_from_key(&k)),
                metered_download: kf.string(group, "metered-download").ok().and_then(|k| download_from_key(&k)),
                never_download: kf.boolean(group, "never-download").unwrap_or(false),
                pause_power_save: kf.boolean(group, "pause-power-save").unwrap_or(false),
            };
            s.sync.insert(uid.to_string(), prefs);
        }
        for group in kf.groups().iter() {
            let Some(uid) = group.strip_prefix(COMPOSE).filter(|u| !u.is_empty()) else { continue };
            let d = ComposePrefs::default();
            let prefs = ComposePrefs {
                format: kf
                    .string(group, "format")
                    .ok()
                    .and_then(|f| crate::compose::Format::from_key(&f))
                    .unwrap_or(d.format),
                signature: kf.string(group, "signature").map(|s| s.to_string()).unwrap_or_default(),
                include_signature: kf.boolean(group, "include-signature").unwrap_or(d.include_signature),
                signature_above: kf.boolean(group, "signature-above").unwrap_or(d.signature_above),
                signing_key: kf.string(group, "signing-key").ok().map(|s| s.to_string()).filter(|s| !s.is_empty()),
                // Before the two switches: "encrypt" was always, on-reply or never.
                encrypt_new: kf
                    .boolean(group, "encrypt-new")
                    .unwrap_or_else(|_| kf.string(group, "encrypt").is_ok_and(|v| v == "always")),
                encrypt_replies: kf
                    .boolean(group, "encrypt-replies")
                    .unwrap_or_else(|_| kf.string(group, "encrypt").map_or(d.encrypt_replies, |v| v != "never")),
                autocrypt: kf.boolean(group, "autocrypt").unwrap_or(d.autocrypt),
                attach_key: kf.string(group, "attach-key").ok().and_then(|s| AttachKey::from_key(&s)).unwrap_or(d.attach_key),
            };
            s.compose.insert(uid.to_string(), prefs);
        }
        if let Some(v) = kf.string(CRYPTO, "import-keys").ok().and_then(|s| ImportKeys::from_key(&s)) {
            s.import_keys = v;
        }
        if let Ok(v) = kf.string(CRYPTO, "keyserver")
            && !v.trim().is_empty()
        {
            s.keyserver = v.to_string();
        }
        s
    }

    pub fn save(&self) -> Result<(), glib::Error> {
        let kf = glib::KeyFile::new();
        kf.set_boolean(READ, "mark-read", self.mark_read);
        kf.set_integer(READ, "delay", self.mark_read_delay as i32);
        kf.set_boolean(READ, "on-response", self.read_on_response);
        kf.set_boolean(READ, "plain-monospace", self.plain_monospace);
        kf.set_boolean(READ, "confirm-links", self.confirm_links);
        kf.set_boolean(READ, "remove-tracking", self.remove_tracking);
        // GKeyFile's list format: items separated by ";", read back with
        // string_list().
        kf.set_string(REMOTE, "senders", &self.remote_senders.join(";"));
        kf.set_string(REMOTE, "domains", &self.remote_domains.join(";"));
        kf.set_boolean(REMOTE, "strip-tracking", self.remote_strip);
        kf.set_string(ACCOUNTS, "order", &self.account_order.join(";"));
        kf.set_string(ACCOUNTS, "default", self.default_account.as_deref().unwrap_or(""));
        kf.set_boolean(MAIL_APP, "asked", self.default_app_asked);
        kf.set_boolean(VIEW, "newest-first", self.newest_first);
        kf.set_boolean(VIEW, "threaded", self.threaded);
        kf.set_boolean(BACKGROUND, "run", self.run_in_background);
        kf.set_boolean(BACKGROUND, "autostart", self.autostart);
        kf.set_string(NOTIFY, "silent", &self.silent_accounts.join(";"));
        kf.set_integer(NOTIFY, "limit", self.notification_limit.map_or(-1, |n| n.min(i32::MAX as u32) as i32));
        if let Some(w) = self.list_width {
            kf.set_double(WINDOW, "list-width", w);
        }
        kf.set_boolean(VIEW, "message-below", self.message_below);
        kf.set_boolean(VIEW, "compact-headers", self.compact_headers);
        kf.set_boolean(READ, "on-delete", self.read_on_delete);
        kf.set_boolean(READ, "on-junk", self.read_on_junk);
        kf.set_boolean(AVATARS, "bimi", self.avatar_bimi);
        kf.set_boolean(AVATARS, "libravatar", self.avatar_libravatar);
        kf.set_boolean(AVATARS, "gravatar", self.avatar_gravatar);
        if let Some(h) = self.list_height {
            kf.set_integer(WINDOW, "list-height", h);
        }
        for (uid, p) in &self.compose {
            let group = format!("{COMPOSE}{uid}");
            kf.set_string(&group, "format", p.format.key());
            kf.set_string(&group, "signature", &p.signature);
            kf.set_boolean(&group, "include-signature", p.include_signature);
            kf.set_boolean(&group, "signature-above", p.signature_above);
            kf.set_string(&group, "signing-key", p.signing_key.as_deref().unwrap_or(""));
            kf.set_boolean(&group, "encrypt-new", p.encrypt_new);
            kf.set_boolean(&group, "encrypt-replies", p.encrypt_replies);
            kf.set_boolean(&group, "autocrypt", p.autocrypt);
            kf.set_string(&group, "attach-key", p.attach_key.key());
        }
        for (uid, p) in &self.sync {
            let group = format!("{SYNC}{uid}");
            // An absent key is "Same as Above".
            if let Some(r) = p.metered_refresh {
                kf.set_string(&group, "metered-refresh", &refresh_key(r));
            }
            if let Some(d) = p.metered_download {
                kf.set_string(&group, "metered-download", &download_key(d));
            }
            kf.set_boolean(&group, "never-download", p.never_download);
            kf.set_boolean(&group, "pause-power-save", p.pause_power_save);
        }
        kf.set_string(CRYPTO, "import-keys", self.import_keys.key());
        kf.set_string(CRYPTO, "keyserver", &self.keyserver);
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        kf.save_to_file(&p)?;
        *CURRENT.lock().unwrap() = Some(self.clone());
        Ok(())
    }

    pub fn sync_for(&self, account_uid: &str) -> SyncPrefs {
        self.sync.get(account_uid).copied().unwrap_or_default()
    }

    /// The account's compose preferences, defaults when never set.
    pub fn compose_for(&self, account_uid: &str) -> ComposePrefs {
        self.compose.get(account_uid).cloned().unwrap_or_default()
    }

    pub fn allows_remote(&self, from: &crate::Address) -> bool {
        let email = from.email.to_ascii_lowercase();
        self.remote_senders.contains(&email)
            || from.domain().is_some_and(|d| self.remote_domains.contains(&d))
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    #[test]
    fn keys() {
        for d in [Download::Never, Download::Within(Window::AllTime), Download::Within(Window::Last(TimeUnit::Days, 90))] {
            assert_eq!(download_from_key(&download_key(d)), Some(d));
        }
        for r in [Refresh::Never, Refresh::Minutes(15)] {
            assert_eq!(refresh_from_key(&refresh_key(r)), Some(r));
        }
        assert_eq!(download_from_key("x"), None);
    }
}
