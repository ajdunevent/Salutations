//! Per-account offline sync: which folders keep message bodies on disk and
//! how far back. Salutations is the only client of these settings, so it
//! writes them to the account's EDS source directly.
//!
//! "All messages" is the Offline extension's stay-synchronized. With
//! "Specific folders", each folder's own offline-sync state decides, and
//! INBOX is always on. The age window is Camel's limit-by-age, limit-unit
//! and limit-value, kept in the source's backend extension.

use camel::prelude::*;
use edataserver::prelude::*;
use edataserver::{Source, SourceCamel, SourceRegistry};
use glib::translate::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeUnit {
    Days,
    Weeks,
    Months,
    Years,
}

impl TimeUnit {
    fn camel(self) -> camel::ffi::CamelTimeUnit {
        match self {
            TimeUnit::Days => camel::ffi::CAMEL_TIME_UNIT_DAYS,
            TimeUnit::Weeks => camel::ffi::CAMEL_TIME_UNIT_WEEKS,
            TimeUnit::Months => camel::ffi::CAMEL_TIME_UNIT_MONTHS,
            TimeUnit::Years => camel::ffi::CAMEL_TIME_UNIT_YEARS,
        }
    }

    fn from_camel(unit: camel::ffi::CamelTimeUnit) -> TimeUnit {
        match unit {
            camel::ffi::CAMEL_TIME_UNIT_WEEKS => TimeUnit::Weeks,
            camel::ffi::CAMEL_TIME_UNIT_MONTHS => TimeUnit::Months,
            camel::ffi::CAMEL_TIME_UNIT_YEARS => TimeUnit::Years,
            _ => TimeUnit::Days,
        }
    }
}

/// How far back message bodies are kept. A value set elsewhere that is not
/// one of [`Window::PRESETS`] is kept and shown as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Last(TimeUnit, i32),
    AllTime,
}

impl Window {
    pub const PRESETS: [Window; 5] = [
        Window::Last(TimeUnit::Weeks, 1),
        Window::Last(TimeUnit::Months, 1),
        Window::Last(TimeUnit::Days, 90),
        Window::Last(TimeUnit::Years, 1),
        Window::AllTime,
    ];

    pub fn label(self) -> String {
        let (one, many) = match self {
            Window::AllTime => return "All time".into(),
            Window::Last(TimeUnit::Days, _) => ("day", "days"),
            Window::Last(TimeUnit::Weeks, _) => ("week", "weeks"),
            Window::Last(TimeUnit::Months, _) => ("month", "months"),
            Window::Last(TimeUnit::Years, _) => ("year", "years"),
        };
        match self {
            Window::Last(_, 1) => format!("Last {one}"),
            Window::Last(_, n) => format!("Last {n} {many}"),
            Window::AllTime => unreachable!(),
        }
    }

    /// Seconds since the epoch before which messages fall outside the
    /// window, counted back from `now` the way Camel's downloads count.
    pub fn cutoff(self, now: &glib::DateTime) -> Option<i64> {
        let Window::Last(unit, n) = self else { return None };
        let n = -n;
        let then = match unit {
            TimeUnit::Days => now.add_days(n),
            TimeUnit::Weeks => now.add_weeks(n),
            TimeUnit::Months => now.add_months(n),
            TimeUnit::Years => now.add_years(n),
        };
        then.ok().map(|t| t.to_unix())
    }
}

/// What "Download From" offers: a window, or no downloads ahead of
/// opening. Never is Salutations' own; Camel's window stays as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Download {
    Within(Window),
    Never,
}

impl Download {
    /// How far back it reaches from `now`, in seconds: 0 for Never.
    pub fn reach(self, now: &glib::DateTime) -> i64 {
        match self {
            Download::Never => 0,
            Download::Within(w) => w.cutoff(now).map_or(i64::MAX, |c| now.to_unix() - c),
        }
    }

    /// The narrower of the two.
    pub fn min(self, other: Download, now: &glib::DateTime) -> Download {
        if other.reach(now) < self.reach(now) { other } else { self }
    }

    pub fn label(self) -> String {
        match self {
            Download::Within(w) => w.label(),
            Download::Never => "Never".into(),
        }
    }
}

/// The downsync expression that keeps Camel's `window` from reaching back
/// past `cutoff`, or None when `window` alone is no wider. Camel combines an
/// expression with its window's condition when it has one, and otherwise
/// runs the expression as the whole search.
pub fn narrower(window: Window, cutoff: i64, now: &glib::DateTime) -> Option<String> {
    match window.cutoff(now) {
        Some(c) if c >= cutoff => None,
        Some(_) => Some(format!("(> (get-sent-date) {cutoff})")),
        None => Some(format!("(match-all (> (get-sent-date) {cutoff}))")),
    }
}

/// How often the account is checked besides push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    Minutes(u32),
    Never,
}

impl Refresh {
    pub const PRESETS: [Refresh; 5] =
        [Refresh::Minutes(5), Refresh::Minutes(15), Refresh::Minutes(30), Refresh::Minutes(60), Refresh::Never];

    pub fn label(self) -> String {
        match self {
            Refresh::Never => "Never".into(),
            Refresh::Minutes(1) => "Every minute".into(),
            Refresh::Minutes(60) => "Every hour".into(),
            Refresh::Minutes(m) if m % 60 == 0 => format!("Every {} hours", m / 60),
            Refresh::Minutes(m) => format!("Every {m} minutes"),
        }
    }
}

pub const DEFAULT_REFRESH: Refresh = Refresh::Minutes(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfflineSync {
    /// "All messages": every folder; otherwise only folders switched on.
    pub all_folders: bool,
    pub window: Window,
}

/// The default for an account Salutations has not configured before.
pub const DEFAULT: OfflineSync = OfflineSync { all_folders: false, window: Window::Last(TimeUnit::Days, 90) };

/// The named extension, created when missing (unlike
/// `SourceExtManual::extension`, which only finds existing ones).
fn extension(source: &Source, name: &str) -> glib::Object {
    unsafe {
        from_glib_none(edataserver::ffi::e_source_get_extension(source.to_glib_none().0, name.to_glib_none().0)
            as *mut glib::gobject_ffi::GObject)
    }
}

/// The CamelOfflineSettings kept in the source's backend extension.
fn settings(source: &Source, backend: &str) -> Option<camel::OfflineSettings> {
    let name = SourceCamel::extension_name(backend)?;
    let ext = extension(source, &name);
    let ext = ext.downcast::<SourceCamel>().ok()?;
    let ptr = unsafe { edataserver::ffi::e_source_camel_get_settings(ext.to_glib_none().0) };
    let s: Option<camel::Settings> = unsafe { from_glib_none(ptr) };
    s?.downcast().ok()
}

fn source(registry: &SourceRegistry, uid: &str) -> Result<Source, glib::Error> {
    registry
        .ref_source(uid)
        .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::NotFound, &format!("No account source {uid}")))
}

pub fn read(registry: &SourceRegistry, account: &crate::Account) -> Result<OfflineSync, glib::Error> {
    let source = source(registry, &account.uid)?;
    let all_folders = extension(&source, edataserver::E_SOURCE_EXTENSION_OFFLINE).property::<bool>("stay-synchronized");
    let window = match settings(&source, &account.backend_name) {
        Some(s) if s.is_limit_by_age() => unsafe {
            let p = s.to_glib_none().0;
            Window::Last(
                TimeUnit::from_camel(camel::ffi::camel_offline_settings_get_limit_unit(p)),
                camel::ffi::camel_offline_settings_get_limit_value(p),
            )
        },
        _ => Window::AllTime,
    };
    Ok(OfflineSync { all_folders, window })
}

/// Saves `sync` to the account's source. The store's settings are bound to
/// the source, so the change reaches Camel at once.
pub fn write(registry: &SourceRegistry, account: &crate::Account, sync: &OfflineSync) -> Result<(), glib::Error> {
    let source = source(registry, &account.uid)?;
    extension(&source, edataserver::E_SOURCE_EXTENSION_OFFLINE).set_property("stay-synchronized", sync.all_folders);
    if let Some(s) = settings(&source, &account.backend_name) {
        match sync.window {
            Window::Last(unit, value) => unsafe {
                let p = s.to_glib_none().0;
                camel::ffi::camel_offline_settings_set_limit_by_age(p, true.into_glib());
                camel::ffi::camel_offline_settings_set_limit_unit(p, unit.camel());
                // Declared gboolean in the 3.60 header, but stored as gint.
                camel::ffi::camel_offline_settings_set_limit_value(p, value);
            },
            Window::AllTime => s.set_limit_by_age(false),
        }
    }
    write_source(&source)
}

/// The account's check interval, from the source's Refresh extension.
pub fn read_refresh(registry: &SourceRegistry, account: &crate::Account) -> Result<Refresh, glib::Error> {
    let source = source(registry, &account.uid)?;
    let ext = extension(&source, edataserver::E_SOURCE_EXTENSION_REFRESH);
    Ok(if ext.property::<bool>("enabled") {
        Refresh::Minutes(ext.property::<u32>("interval-minutes").max(1))
    } else {
        Refresh::Never
    })
}

pub fn write_refresh(registry: &SourceRegistry, account: &crate::Account, refresh: Refresh) -> Result<(), glib::Error> {
    let source = source(registry, &account.uid)?;
    let ext = extension(&source, edataserver::E_SOURCE_EXTENSION_REFRESH);
    match refresh {
        Refresh::Minutes(m) => {
            ext.set_property("interval-minutes", m);
            ext.set_property("enabled", true);
        }
        Refresh::Never => ext.set_property("enabled", false),
    }
    write_source(&source)
}

pub(crate) fn write_source(source: &Source) -> Result<(), glib::Error> {
    let held = source.uid().and_then(|uid| PUSH_HELD.lock().unwrap().get(uid.as_str()).copied());
    let idle = held.and_then(|_| {
        let backend = extension(source, edataserver::E_SOURCE_EXTENSION_MAIL_ACCOUNT).property::<Option<String>>("backend-name")?;
        idle_settings(source, &backend)
    });
    if let (Some(s), Some(on)) = (&idle, held) {
        s.set_property("use-idle", on);
    }
    let mut error = std::ptr::null_mut();
    let ok = unsafe { edataserver::ffi::e_source_write_sync(source.to_glib_none().0, std::ptr::null_mut(), &mut error) };
    if let Some(s) = &idle {
        s.set_property("use-idle", false);
    }
    if ok == glib::ffi::GFALSE {
        return Err(unsafe { from_glib_full(error) });
    }
    Ok(())
}

fn configured_path() -> std::path::PathBuf {
    crate::data_dir().join("offline-configured")
}

/// Whether Salutations has applied its default for `what` ("offline" or
/// "refresh") to this account before. Offline lines carry the bare UID,
/// as written before the refresh default existed.
pub fn configured(account_uid: &str, what: &str) -> bool {
    let line = marker(account_uid, what);
    std::fs::read_to_string(configured_path()).is_ok_and(|s| s.lines().any(|l| l == line))
}

fn marker(account_uid: &str, what: &str) -> String {
    if what == "offline" { account_uid.to_string() } else { format!("{account_uid}:{what}") }
}

pub fn mark_configured(account_uid: &str, what: &str) {
    let p = configured_path();
    let line = marker(account_uid, what);
    let mut s = std::fs::read_to_string(&p).unwrap_or_default();
    if !s.lines().any(|l| l == line) {
        s.push_str(&line);
        s.push('\n');
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, s);
    }
}

/// Whether the folder keeps bodies on disk, by its own state or, when it
/// has none, by the account's "All messages".
pub fn folder_synced(folder: &camel::Folder) -> bool {
    folder.dynamic_cast_ref::<camel::OfflineFolder>().is_some_and(|f| f.can_downsync())
}

/// Switches one folder on or off for "Specific folders" and saves it in
/// the folder's state file.
pub fn set_folder_synced(folder: &camel::Folder, on: bool) {
    let Some(f) = folder.dynamic_cast_ref::<camel::OfflineFolder>() else { return };
    f.set_offline_sync(if on { camel::ThreeState::On } else { camel::ThreeState::Off });
    folder.save_state();
}

/// The directory of `folder`'s cached message bodies: Camel keeps its data
/// cache next to the folder's state file, with each body at
/// `cur/<hash>/<safe uid>`, where hash is bits 5 to 10 of `g_str_hash(uid)`
/// in two hex digits (camel-data-cache.c, EDS 3.56 to 3.60).
fn cache_dir(folder: &camel::Folder) -> Option<std::path::PathBuf> {
    let state = folder.state_filename()?;
    Some(std::path::Path::new(state.as_str()).parent()?.join("cur"))
}

fn cache_file(dir: &std::path::Path, uid: &str) -> Option<std::path::PathBuf> {
    let c = std::ffi::CString::new(uid).ok()?;
    let hash = (unsafe { glib::ffi::g_str_hash(c.as_ptr() as glib::ffi::gconstpointer) } >> 5) & 0x3f;
    let safe = unsafe { camel::ffi::camel_file_util_safe_filename(c.as_ptr()) };
    if safe.is_null() {
        return None;
    }
    let name: glib::GString = unsafe { from_glib_full(safe) };
    Some(dir.join(format!("{hash:02x}")).join(name.as_str()))
}

/// Deletes every cached body of a folder that is no longer kept offline.
/// A message opened later is fetched again.
pub fn clear_folder(folder: &camel::Folder) {
    if let Some(dir) = cache_dir(folder) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Deletes the cached bodies of messages sent before `cutoff`, or only of
/// those sent in `[since, cutoff)` when the earlier range was handled
/// already. Returns how many files were removed. Blocks.
pub fn prune(folder: &camel::Folder, cutoff: i64, since: Option<i64>) -> usize {
    let Some(dir) = cache_dir(folder) else { return 0 };
    if !dir.exists() {
        return 0;
    }
    let expr = match since {
        Some(s) if s < cutoff => {
            format!("(match-all (and (>= (get-sent-date) {s}) (< (get-sent-date) {cutoff})))")
        }
        Some(_) => return 0,
        None => format!("(match-all (< (get-sent-date) {cutoff}))"),
    };
    let Ok(uids) = folder.search_sync(&expr, None::<&gio::Cancellable>) else { return 0 };
    uids.iter()
        .filter_map(|uid| cache_file(&dir, uid))
        .filter(|f| std::fs::remove_file(f).is_ok())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        // Matches the 3.60 test run: uid "1" lives in cur/2e/1.
        let f = cache_file(std::path::Path::new("/c"), "1").unwrap();
        assert_eq!(f, std::path::Path::new("/c/2e/1"));
    }

    #[test]
    fn labels() {
        assert_eq!(Window::Last(TimeUnit::Days, 3).label(), "Last 3 days");
        assert_eq!(Window::Last(TimeUnit::Weeks, 1).label(), "Last week");
        assert_eq!(Refresh::Minutes(22).label(), "Every 22 minutes");
        assert_eq!(Refresh::Minutes(120).label(), "Every 2 hours");
        let now = glib::DateTime::from_unix_utc(1_000_000_000).unwrap();
        assert_eq!(Window::Last(TimeUnit::Weeks, 1).cutoff(&now), Some(1_000_000_000 - 7 * 86_400));
        assert_eq!(Window::AllTime.cutoff(&now), None);
    }
}

/// IMAPX's `use-idle` setting, which covers IDLE and NOTIFY.
pub fn read_push(registry: &SourceRegistry, account: &crate::Account) -> bool {
    if let Some(on) = PUSH_HELD.lock().unwrap().get(&account.uid) {
        return *on;
    }
    source(registry, &account.uid)
        .ok()
        .and_then(|s| settings(&s, &account.backend_name))
        .filter(|s| s.find_property("use-idle").is_some())
        .is_none_or(|s| s.property::<bool>("use-idle"))
}

/// Takes effect when the account next connects.
pub fn write_push(registry: &SourceRegistry, account: &crate::Account, on: bool) -> Result<(), glib::Error> {
    let source = source(registry, &account.uid)?;
    if let Some(held) = PUSH_HELD.lock().unwrap().get_mut(&account.uid) {
        *held = on;
    } else if let Some(s) = idle_settings(&source, &account.backend_name) {
        s.set_property("use-idle", on);
    }
    write_source(&source)
}

/// Accounts whose push is held off, with the setting to restore. The
/// store's settings are the source's own, so `write_source` puts the
/// setting back for the length of every write.
static PUSH_HELD: std::sync::Mutex<std::collections::BTreeMap<String, bool>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

fn idle_settings(source: &Source, backend: &str) -> Option<camel::OfflineSettings> {
    settings(source, backend).filter(|s| s.find_property("use-idle").is_some())
}

/// Holds the account's push off without saving that, or restores it. The
/// store stops pushing once its connection closes.
pub fn hold_push(registry: &SourceRegistry, account: &crate::Account, hold: bool) {
    let Ok(source) = source(registry, &account.uid) else { return };
    let Some(s) = idle_settings(&source, &account.backend_name) else { return };
    let mut held = PUSH_HELD.lock().unwrap();
    if hold {
        if !held.contains_key(&account.uid) {
            held.insert(account.uid.clone(), s.property::<bool>("use-idle"));
            s.set_property("use-idle", false);
        }
    } else if let Some(on) = held.remove(&account.uid) {
        s.set_property("use-idle", on);
    }
}

/// The account's server, port and whether it uses TLS from the start
/// (`Some(true)`), STARTTLS (`Some(false)`) or nothing (`None`).
pub fn server(registry: &SourceRegistry, account: &crate::Account) -> Option<(String, u16, Option<bool>)> {
    let source = source(registry, &account.uid).ok()?;
    let auth = extension(&source, edataserver::E_SOURCE_EXTENSION_AUTHENTICATION);
    let host = auth.property::<Option<String>>("host").filter(|h| !h.is_empty())?;
    let port = auth.property::<u32>("port") as u16;
    let method = extension(&source, "Security").property::<Option<String>>("method").unwrap_or_default();
    let tls = match method.as_str() {
        "ssl-on-alternate-port" => Some(true),
        "starttls-on-standard-port" => Some(false),
        _ => None,
    };
    let port = if port != 0 { port } else if tls == Some(true) { 993 } else { 143 };
    Some((host, port, tls))
}
