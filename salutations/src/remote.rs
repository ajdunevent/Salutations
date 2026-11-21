//! The window's side of the private connection to the background process,
//! which owns the accounts. Folders, stores, message summaries and the
//! session are stand-ins with the methods the window uses from Camel's,
//! answered by the background process; messages come as their source and
//! are parsed here with Camel's MIME parser.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

use gtk::prelude::*;
use gtk::{gio, glib};
use salutations_core::camel::prelude::*;
use salutations_core::engine::{Event, SyncState};
use salutations_core::ipc::{self, AccountInfo, Changes, Info, Node, Special};
use salutations_core::{Account, SpecialFolders};

type Real = salutations_core::camel::MimeMessage;

static CONN: OnceLock<gio::DBusConnection> = OnceLock::new();

fn conn() -> &'static gio::DBusConnection {
    CONN.get().expect("connected to the background process")
}

fn args(v: impl ToVariant) -> glib::Variant {
    v.to_variant()
}

fn call_params(name: &str, a: glib::Variant) -> glib::Variant {
    glib::Variant::tuple_from_iter([name.to_variant(), glib::Variant::from_variant(&a)])
}

fn unwrap_result(v: glib::Variant) -> glib::Variant {
    v.child_value(0).as_variant().unwrap_or_else(ipc::unit)
}

/// Calls the background process and waits, from any thread.
pub fn call_sync(name: &str, a: glib::Variant) -> Result<glib::Variant, glib::Error> {
    conn()
        .call_sync(
            None,
            ipc::PATH,
            ipc::IFACE,
            "Call",
            Some(&call_params(name, a)),
            Some(glib::VariantTy::new("(v)").expect("type")),
            gio::DBusCallFlags::NONE,
            -1,
            None::<&gio::Cancellable>,
        )
        .map(unwrap_result)
        .map_err(|e| ipc::error_from_remote(&e))
}

pub async fn call(name: &str, a: glib::Variant) -> Result<glib::Variant, glib::Error> {
    conn()
        .call_future(
            None,
            ipc::PATH,
            ipc::IFACE,
            "Call",
            Some(&call_params(name, a)),
            Some(glib::VariantTy::new("(v)").expect("type")),
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await
        .map(unwrap_result)
        .map_err(|e| ipc::error_from_remote(&e))
}

/// Calls without waiting for the answer; calls keep their order.
pub fn send(name: &str, a: glib::Variant) {
    let name = name.to_string();
    glib::spawn_future_local(async move {
        if let Err(e) = call(&name, a).await {
            crate::debug::log(&format!("{name}: {}", e.message()));
        }
    });
}

/// What the background process asks of the window.
#[derive(Debug, Clone)]
pub enum Request {
    Present,
    /// A `mailto:` link, or empty for a new message.
    Compose(String),
    Import(Vec<String>),
    /// A notification's button: kind, account, message.
    Notification(String, String, String),
    /// The waiting draft being sent for an account, or none.
    Outbox(String, Option<String>),
    Toast(String),
}

/// Connects on the socket the background process handed over.
pub async fn connect() -> Result<(), glib::Error> {
    use std::os::fd::FromRawFd;
    let fd: i32 = std::env::var(ipc::FD_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::NotConnected, "Started without the background process"))?;
    let socket = gio::Socket::from_fd(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })?;
    let stream = socket.connection_factory_create_connection();
    let c = gio::DBusConnection::new_future(&stream, None, gio::DBusConnectionFlags::AUTHENTICATION_CLIENT, None).await?;
    c.set_exit_on_close(false);
    // Nothing works without the background process; the window goes with
    // it rather than stay up unable to do anything.
    c.connect_closed(|_, _, _| {
        eprintln!("Salutations: the background process ended");
        if let Some(app) = gio::Application::default() {
            app.quit();
        }
    });
    let _ = CONN.set(c);
    Ok(())
}

// Engine

pub struct AccountState {
    pub account: Account,
    info: RefCell<AccountInfo>,
}

impl AccountState {
    pub fn uid(&self) -> &str {
        &self.account.uid
    }

    pub fn store(&self) -> Option<Store> {
        Some(Store { account: self.account.uid.clone() })
    }

    pub fn state(&self) -> SyncState {
        match self.info.borrow().state.as_str() {
            "syncing" => SyncState::Syncing,
            "offline" => SyncState::Offline,
            "error" => SyncState::Error,
            _ => SyncState::Idle,
        }
    }

    pub fn message(&self) -> String {
        self.info.borrow().message.clone()
    }

    pub fn sign_in_failed(&self) -> bool {
        self.info.borrow().sign_in_failed
    }

    pub fn special(&self) -> Option<SpecialFolders> {
        self.info.borrow().special.to_folders()
    }

    pub fn connected(&self) -> bool {
        self.info.borrow().connected
    }

    /// INBOX, once the background process has it open.
    pub fn inbox(&self) -> Option<Folder> {
        if !self.info.borrow().inbox {
            return None;
        }
        Folder::open_sync(&self.account.uid, "INBOX").ok()
    }
}

type Listener = Box<dyn Fn(&Event)>;
type RequestListener = Box<dyn Fn(Request)>;

/// The background process's engine as the window sees it.
pub struct Engine {
    registry: salutations_core::edataserver::SourceRegistry,
    accounts: RefCell<Vec<Rc<AccountState>>>,
    listeners: RefCell<Vec<Listener>>,
    requests: RefCell<Vec<RequestListener>>,
    subscription: RefCell<Option<gio::SignalSubscription>>,
    started: Cell<bool>,
    /// Requests that came before the accounts and their trees.
    held: RefCell<Option<Vec<Request>>>,
    /// Each account's latest tree, which may come before the account.
    trees: RefCell<HashMap<String, Vec<salutations_core::camel::FolderNode>>>,
}

impl Engine {
    pub fn new(registry: salutations_core::edataserver::SourceRegistry) -> Rc<Engine> {
        Rc::new(Engine {
            registry,
            accounts: RefCell::new(Vec::new()),
            listeners: RefCell::new(Vec::new()),
            requests: RefCell::new(Vec::new()),
            subscription: RefCell::new(None),
            started: Cell::new(false),
            trees: RefCell::new(HashMap::new()),
            held: RefCell::new(Some(Vec::new())),
        })
    }

    pub fn connect(&self, f: impl Fn(&Event) + 'static) {
        self.listeners.borrow_mut().push(Box::new(f));
    }

    pub fn connect_request(&self, f: impl Fn(Request) + 'static) {
        self.requests.borrow_mut().push(Box::new(f));
    }

    /// The background process already has every account's tree.
    pub fn set_settle(&self, _f: impl Fn() -> Pin<Box<dyn Future<Output = ()>>> + 'static) {}

    fn emit(&self, e: Event) {
        for l in self.listeners.borrow().iter() {
            l(&e);
        }
    }

    fn request(&self, r: Request) {
        if let Some(held) = self.held.borrow_mut().as_mut() {
            held.push(r);
            return;
        }
        for l in self.requests.borrow().iter() {
            l(r.clone());
        }
    }

    /// Follows the background process's events and asks for the accounts.
    pub fn start(self: &Rc<Self>) {
        if self.started.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        let sub = conn().subscribe_to_signal(None, Some(ipc::IFACE), Some("Event"), Some(ipc::PATH), None, gio::DBusSignalFlags::NONE, move |s| {
            let Some(this) = weak.upgrade() else { return };
            let Some((name, data)) = s.parameters.get::<(String, glib::Variant)>() else { return };
            this.event(&name, &data);
        });
        *self.subscription.borrow_mut() = Some(sub);
        let this = self.clone();
        glib::spawn_future_local(async move {
            match call("hello", ipc::unit()).await {
                Ok(v) => {
                    if let Some(list) = v.get::<Vec<AccountInfo>>() {
                        this.set_accounts(list);
                    }
                    this.release_when_ready(false);
                    let weak = Rc::downgrade(&this);
                    glib::timeout_add_seconds_local_once(3, move || {
                        if let Some(this) = weak.upgrade() {
                            this.release_when_ready(true);
                        }
                    });
                }
                Err(e) => glib::g_warning!("salutations", "The background process did not answer: {}", e),
            }
        });
    }

    /// Passes on held requests once every account has its tree.
    fn release_when_ready(&self, force: bool) {
        let ready = force || {
            let trees = self.trees.borrow();
            self.accounts.borrow().iter().all(|a| trees.contains_key(a.uid()))
        };
        if !ready {
            return;
        }
        let Some(held) = self.held.borrow_mut().take() else { return };
        for r in held {
            self.request(r);
        }
    }

    fn set_accounts(&self, list: Vec<AccountInfo>) {
        let found = salutations_core::mail_accounts(&self.registry);
        let mut accounts = Vec::new();
        for info in list {
            let existing = self.accounts.borrow().iter().find(|a| a.account.uid == info.uid).cloned();
            match existing {
                Some(a) => {
                    *a.info.borrow_mut() = info;
                    accounts.push(a);
                }
                None => {
                    let Some(account) = found.iter().find(|a| a.uid == info.uid).cloned() else { continue };
                    accounts.push(Rc::new(AccountState { account, info: RefCell::new(info) }));
                }
            }
        }
        *self.accounts.borrow_mut() = accounts;
        self.emit(Event::Accounts);
        for a in self.accounts() {
            let tree = self.trees.borrow().get(a.uid()).cloned();
            if let Some(nodes) = tree {
                self.emit(Event::Folders { account: a.uid().to_string(), nodes, rebuild: true });
            }
            self.emit(Event::SyncState(a.uid().to_string()));
            if a.info.borrow().inbox {
                self.emit(Event::InboxReady(a.uid().to_string()));
            }
        }
    }

    fn event(&self, name: &str, data: &glib::Variant) {
        match name {
            "accounts" => {
                if let Some(list) = data.get::<Vec<AccountInfo>>() {
                    self.set_accounts(list);
                }
            }
            "account" => {
                let Some(info) = data.get::<AccountInfo>() else { return };
                let Some(a) = self.account(&info.uid) else { return };
                let old = a.info.replace(info.clone());
                if old.state != info.state || old.message != info.message {
                    self.emit(Event::SyncState(info.uid.clone()));
                }
                if old.paused != info.paused {
                    self.emit(Event::Paused(info.uid.clone()));
                }
                if !old.inbox && info.inbox {
                    self.emit(Event::InboxReady(info.uid.clone()));
                }
            }
            "folders" => {
                let Some((account, nodes, rebuild)) = data.get::<(String, Vec<Node>, bool)>() else { return };
                let nodes = ipc::unflatten_nodes(&nodes);
                self.trees.borrow_mut().insert(account.clone(), nodes.clone());
                if self.account(&account).is_some() {
                    self.emit(Event::Folders { account, nodes, rebuild });
                    self.release_when_ready(false);
                }
            }
            "synced" => {
                if let Some(uid) = data.get::<String>() {
                    self.emit(Event::Synced(uid));
                }
            }
            "changes" => {
                if let Some(c) = data.get::<Changes>() {
                    apply_changes(c);
                }
            }
            "present" => self.request(Request::Present),
            "compose" => self.request(Request::Compose(data.get::<String>().unwrap_or_default())),
            "import" => self.request(Request::Import(data.get::<Vec<String>>().unwrap_or_default())),
            "notification" => {
                if let Some((k, a, u)) = data.get::<(String, String, String)>() {
                    self.request(Request::Notification(k, a, u));
                }
            }
            "outbox" => {
                if let Some((a, u)) = data.get::<(String, String)>() {
                    self.request(Request::Outbox(a, (!u.is_empty()).then_some(u)));
                }
            }
            "toast" => self.request(Request::Toast(data.get::<String>().unwrap_or_default())),
            _ => {}
        }
    }

    pub fn accounts(&self) -> Vec<Rc<AccountState>> {
        self.accounts.borrow().clone()
    }

    pub fn account(&self, uid: &str) -> Option<Rc<AccountState>> {
        self.accounts.borrow().iter().find(|a| a.account.uid == uid).cloned()
    }

    pub fn load_accounts(&self) {
        send("account_changed", args((String::new(), "order".to_string())));
    }

    pub fn account_changed(&self, acct: &AccountState, kind: &str) {
        send("account_changed", args((acct.uid().to_string(), kind.to_string())));
    }

    pub fn sync(&self, acct: &AccountState) {
        send("sync", args(acct.uid().to_string()));
    }

    pub fn cancel_sync(&self, acct: &AccountState) {
        send("cancel_sync", args(acct.uid().to_string()));
    }

    pub fn resync(&self, acct: &AccountState, drop_connection: bool) {
        send("resync", args((acct.uid().to_string(), drop_connection)));
    }

    pub fn paused(&self, acct: &AccountState) -> bool {
        acct.info.borrow().paused
    }

    pub fn store_for(&self, acct: &AccountState) -> Result<Store, glib::Error> {
        Ok(Store { account: acct.uid().to_string() })
    }

    pub fn set_special(&self, acct: &AccountState, special: SpecialFolders) {
        let s = Special::from_folders(Some(&special));
        acct.info.borrow_mut().special = s.clone();
        send("set_special", args((acct.uid().to_string(), s)));
    }

    pub async fn refresh_folders(&self, acct: &AccountState) {
        let _ = call("folders", args(acct.uid().to_string())).await;
    }

    pub fn set_open(&self, folder: Option<Folder>) {
        let (a, n) = folder.map(|f| (f.0.account.clone(), f.0.name.clone())).unwrap_or_default();
        send("set_open", args((a, n)));
    }
}

/// Tells the background process what the window shows, so mail arriving
/// there does not notify.
pub fn shown(active: bool, folder: Option<&Folder>) {
    let (a, n) = folder.map(|f| (f.0.account.clone(), f.0.name.clone())).unwrap_or_default();
    send("shown", args((active, a, n)));
}

// Folders

#[derive(Default)]
struct Summary {
    uids: Vec<String>,
    infos: HashMap<String, Info>,
}

struct Inner {
    account: String,
    name: String,
    display: String,
    summary: Mutex<Summary>,
    /// Flag changes collected between `freeze` and `thaw`.
    frozen: Mutex<Option<Vec<(String, u32, u32)>>>,
}

#[derive(Clone)]
pub struct Folder(Arc<Inner>);

impl PartialEq for Folder {
    fn eq(&self, other: &Self) -> bool {
        self.0.account == other.0.account && self.0.name == other.0.name
    }
}

impl Eq for Folder {}

impl std::fmt::Debug for Folder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Folder({}/{})", self.0.account, self.0.name)
    }
}

static FOLDERS: OnceLock<Mutex<HashMap<(String, String), Folder>>> = OnceLock::new();

fn folders() -> std::sync::MutexGuard<'static, HashMap<(String, String), Folder>> {
    FOLDERS.get_or_init(Default::default).lock().unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HandlerId(u64);

type ChangedHandler = Rc<dyn Fn(&Folder, &salutations_core::camel::FolderChanges)>;
type Handlers = Vec<(u64, (String, String), ChangedHandler)>;

thread_local! {
    static HANDLERS: RefCell<Handlers> = const { RefCell::new(Vec::new()) };
    static NEXT: Cell<u64> = const { Cell::new(1) };
}

fn key_of(account: &str, name: &str) -> (String, String) {
    // INBOX is case-insensitive in IMAP.
    let name = if name.eq_ignore_ascii_case("INBOX") { "INBOX".to_string() } else { name.to_string() };
    (account.to_string(), name)
}

fn opened(account: &str, name: &str, v: glib::Variant) -> Result<Folder, glib::Error> {
    let (display, infos) = v
        .get::<(String, Vec<Info>)>()
        .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::InvalidData, "Bad folder summary"))?;
    let key = key_of(account, name);
    if let Some(f) = folders().get(&key) {
        return Ok(f.clone());
    }
    let summary = Summary { uids: infos.iter().map(|i| i.uid.clone()).collect(), infos: infos.into_iter().map(|i| (i.uid.clone(), i)).collect() };
    let f = Folder(Arc::new(Inner {
        account: key.0.clone(),
        name: key.1.clone(),
        display,
        summary: Mutex::new(summary),
        frozen: Mutex::new(None),
    }));
    folders().insert(key, f.clone());
    Ok(f)
}

fn apply_changes(c: Changes) {
    let Some(f) = folders().get(&key_of(&c.account, &c.folder)).cloned() else { return };
    {
        let mut s = f.0.summary.lock().unwrap();
        for i in c.added.iter().chain(c.changed.iter()) {
            if !s.infos.contains_key(&i.uid) {
                s.uids.push(i.uid.clone());
            }
            s.infos.insert(i.uid.clone(), i.clone());
        }
        if !c.removed.is_empty() {
            s.uids.retain(|u| !c.removed.contains(u));
            for u in &c.removed {
                s.infos.remove(u);
            }
        }
    }
    let changes = salutations_core::camel::FolderChanges {
        added: c.added.iter().map(|i| i.uid.clone()).collect(),
        removed: c.removed,
        changed: c.changed.iter().map(|i| i.uid.clone()).collect(),
        recent: Vec::new(),
    };
    let key = key_of(&f.0.account, &f.0.name);
    let handlers: Vec<ChangedHandler> = HANDLERS.with_borrow(|h| h.iter().filter(|(_, k, _)| *k == key).map(|(_, _, f)| f.clone()).collect());
    for h in handlers {
        h(&f, &changes);
    }
}

fn op_error(e: impl std::fmt::Display) -> glib::Error {
    glib::Error::new(gio::IOErrorEnum::Failed, &e.to_string())
}

impl Folder {
    pub async fn open(account: &str, name: &str) -> Result<Folder, glib::Error> {
        if let Some(f) = folders().get(&key_of(account, name)) {
            return Ok(f.clone());
        }
        let v = call("open_folder", args((account.to_string(), name.to_string()))).await?;
        opened(account, name, v)
    }

    pub fn open_sync(account: &str, name: &str) -> Result<Folder, glib::Error> {
        if let Some(f) = folders().get(&key_of(account, name)) {
            return Ok(f.clone());
        }
        let v = call_sync("open_folder", args((account.to_string(), name.to_string())))?;
        opened(account, name, v)
    }

    pub fn account_uid(&self) -> &str {
        &self.0.account
    }

    fn ids(&self) -> (String, String) {
        (self.0.account.clone(), self.0.name.clone())
    }

    pub fn full_name(&self) -> Option<glib::GString> {
        Some(self.0.name.as_str().into())
    }

    pub fn display_name(&self) -> Option<glib::GString> {
        Some(self.0.display.as_str().into())
    }

    pub fn parent_store(&self) -> Option<Store> {
        Some(Store { account: self.0.account.clone() })
    }

    pub fn uids(&self) -> Vec<glib::GString> {
        self.0.summary.lock().unwrap().uids.iter().map(|u| u.as_str().into()).collect()
    }

    pub fn message_info(&self, uid: &str) -> Option<MessageInfo> {
        let info = self.0.summary.lock().unwrap().infos.get(uid).cloned()?;
        Some(MessageInfo { info: Rc::new(RefCell::new(info)), folder: self.clone() })
    }

    pub fn message_flags(&self, uid: &str) -> u32 {
        self.0.summary.lock().unwrap().infos.get(uid).map_or(0, |i| i.flags)
    }

    pub fn folder_summary(&self) -> Option<FolderSummary> {
        let s = self.0.summary.lock().unwrap();
        const SEEN: u32 = salutations_core::camel::MessageFlags::SEEN.bits();
        const DELETED: u32 = salutations_core::camel::MessageFlags::DELETED.bits();
        let unread = s.infos.values().filter(|i| i.flags & (SEEN | DELETED) == 0).count();
        Some(FolderSummary { unread: unread as u32 })
    }

    pub fn set_message_flags(&self, uid: &str, mask: u32, set: u32) -> bool {
        {
            let mut s = self.0.summary.lock().unwrap();
            let Some(i) = s.infos.get_mut(uid) else { return false };
            i.flags = (i.flags & !mask) | (set & mask);
        }
        let mut frozen = self.0.frozen.lock().unwrap();
        match frozen.as_mut() {
            Some(list) => list.push((uid.to_string(), mask, set)),
            None => {
                drop(frozen);
                let (a, n) = self.ids();
                send("set_flags", args((a, n, vec![(uid.to_string(), mask, set)])));
            }
        }
        true
    }

    pub fn freeze(&self) {
        let mut f = self.0.frozen.lock().unwrap();
        if f.is_none() {
            *f = Some(Vec::new());
        }
    }

    pub fn thaw(&self) {
        let Some(list) = self.0.frozen.lock().unwrap().take() else { return };
        if !list.is_empty() {
            let (a, n) = self.ids();
            send("set_flags", args((a, n, list)));
        }
    }

    pub fn connect_changed<F: Fn(&Folder, &salutations_core::camel::FolderChanges) + 'static>(&self, f: F) -> HandlerId {
        let id = NEXT.get();
        NEXT.set(id + 1);
        let key = key_of(&self.0.account, &self.0.name);
        HANDLERS.with_borrow_mut(|h| h.push((id, key, Rc::new(f))));
        HandlerId(id)
    }

    pub fn disconnect(&self, id: HandlerId) {
        HANDLERS.with_borrow_mut(|h| h.retain(|(i, _, _)| *i != id.0));
    }

    pub fn refresh_info_future(&self, _priority: i32) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
        let (a, n) = self.ids();
        Box::pin(async move { call("refresh", args((a, n))).await.map(|_| ()) })
    }

    pub fn refresh_info_sync(&self, _c: Option<&gio::Cancellable>) -> Result<(), glib::Error> {
        call_sync("refresh", args(self.ids())).map(|_| ())
    }

    pub fn synchronize_future(&self, expunge: bool, _priority: i32) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
        self.thaw_now();
        let (a, n) = self.ids();
        Box::pin(async move { call("synchronize", args((a, n, expunge))).await.map(|_| ()) })
    }

    pub fn expunge_future(&self, priority: i32) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
        self.synchronize_future(true, priority)
    }

    /// Flags waiting in a freeze go out before an operation that relies on
    /// them.
    fn thaw_now(&self) {
        let pending = self.0.frozen.lock().unwrap().as_mut().map(std::mem::take);
        if let Some(list) = pending.filter(|l| !l.is_empty()) {
            let (a, n) = self.ids();
            send("set_flags", args((a, n, list)));
        }
    }

    pub fn search_sync(&self, expression: &str, _c: Option<&gio::Cancellable>) -> Result<Vec<String>, glib::Error> {
        let (a, n) = self.ids();
        let v = call_sync("search", args((a, n, expression.to_string())))?;
        Ok(v.get::<Vec<String>>().unwrap_or_default())
    }

    pub fn transfer_messages_to_sync(
        &self,
        uids: &[String],
        dest: &Folder,
        delete_originals: bool,
        _c: Option<&gio::Cancellable>,
    ) -> Result<Vec<Option<String>>, glib::Error> {
        let (a, n) = self.ids();
        let (da, dn) = dest.ids();
        let v = call_sync("transfer", args((a, n, uids.to_vec(), da, dn, delete_originals)))?;
        Ok(v.get::<Vec<String>>().unwrap_or_default().into_iter().map(|u| (!u.is_empty()).then_some(u)).collect())
    }

    pub fn message_future(&self, uid: &str, _priority: i32) -> Pin<Box<dyn Future<Output = Result<Option<Real>, glib::Error>>>> {
        let (a, n, u) = (self.0.account.clone(), self.0.name.clone(), uid.to_string());
        Box::pin(async move {
            let v = call("message", args((a, n, u))).await?;
            let (found, data) = v.get::<(bool, glib::Variant)>().ok_or_else(|| op_error("Bad message"))?;
            if !found {
                return Ok(None);
            }
            parse_message(data.fixed_array::<u8>().map_err(op_error)?).map(Some)
        })
    }

    /// Appends `msg` with `flags`; the new UID when the server reports it.
    pub async fn append(&self, msg: &Real, flags: u32) -> Result<Option<String>, glib::Error> {
        let bytes = mime_bytes(msg)?;
        let (a, n) = self.ids();
        let v = call("append", args((a, n, glib::Variant::array_from_fixed_array(&bytes), flags))).await?;
        Ok(v.get::<String>().filter(|u| !u.is_empty()))
    }

    /// Kept offline (Camel's per-folder offline-sync).
    pub fn synced(&self) -> bool {
        call_sync("folder_synced", args(self.ids())).ok().and_then(|v| v.get::<bool>()).unwrap_or(false)
    }

    /// The first `name` header of message `uid`, from the offline copy only.
    pub fn cached_header(&self, uid: &str, name: &str) -> Option<String> {
        let (a, n) = self.ids();
        call_sync("cached_header", args((a, n, uid.to_string(), name.to_string()))).ok()?.get::<String>().filter(|h| !h.is_empty())
    }

    pub fn set_synced(&self, on: bool) {
        let (a, n) = self.ids();
        send("set_folder_synced", args((a, n, on)));
    }
}

pub struct FolderSummary {
    unread: u32,
}

impl FolderSummary {
    pub fn unread_count(&self) -> u32 {
        self.unread
    }
}

/// A message's summary, as Camel's `MessageInfo`.
#[derive(Clone)]
pub struct MessageInfo {
    info: Rc<RefCell<Info>>,
    folder: Folder,
}

impl MessageInfo {
    fn s(v: &str) -> Option<glib::GString> {
        Some(v.into())
    }

    pub fn uid(&self) -> Option<glib::GString> {
        Self::s(&self.info.borrow().uid)
    }

    pub fn flags(&self) -> u32 {
        self.info.borrow().flags
    }

    pub fn from(&self) -> Option<glib::GString> {
        Self::s(&self.info.borrow().from)
    }

    pub fn to(&self) -> Option<glib::GString> {
        Self::s(&self.info.borrow().to)
    }

    pub fn cc(&self) -> Option<glib::GString> {
        Self::s(&self.info.borrow().cc)
    }

    pub fn subject(&self) -> Option<glib::GString> {
        Self::s(&self.info.borrow().subject)
    }

    pub fn preview(&self) -> Option<glib::GString> {
        let i = self.info.borrow();
        (!i.preview.is_empty()).then(|| i.preview.as_str().into())
    }

    pub fn date_sent(&self) -> i64 {
        self.info.borrow().date_sent
    }

    pub fn date_received(&self) -> i64 {
        self.info.borrow().date_received
    }

    pub fn message_id(&self) -> u64 {
        self.info.borrow().message_id
    }

    pub fn user_header(&self, name: &str) -> Option<glib::GString> {
        self.info.borrow().headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str().into())
    }
}

pub fn message_info_user_flag(info: &MessageInfo, name: &str) -> bool {
    info.info.borrow().user_flags.iter().any(|f| f == name)
}

pub fn message_info_set_user_flag(info: &MessageInfo, name: &str, state: bool) -> bool {
    let uid = {
        let mut i = info.info.borrow_mut();
        i.user_flags.retain(|f| f != name);
        if state {
            i.user_flags.push(name.to_string());
        }
        i.uid.clone()
    };
    if let Some(i) = info.folder.0.summary.lock().unwrap().infos.get_mut(&uid) {
        i.user_flags.retain(|f| f != name);
        if state {
            i.user_flags.push(name.to_string());
        }
    }
    let (a, n) = info.folder.ids();
    send("set_user_flag", args((a, n, uid, name.to_string(), state)));
    true
}

pub struct ThreadNode {
    pub info: MessageInfo,
    pub children: Vec<ThreadNode>,
}

impl ThreadNode {
    pub fn flatten(&self) -> Vec<MessageInfo> {
        let mut out = vec![self.info.clone()];
        for c in &self.children {
            out.extend(c.flatten());
        }
        out
    }
}

/// The folder's messages threaded by Camel in the background process.
pub fn thread_folder(folder: &Folder) -> Vec<ThreadNode> {
    let flat = call_sync("threads", args(folder.ids())).ok().and_then(|v| v.get::<Vec<(String, u32)>>()).unwrap_or_default();
    let threads = ipc::unflatten_threads(&flat);
    fn conv(f: &Folder, t: &ipc::Thread) -> Option<ThreadNode> {
        Some(ThreadNode { info: f.message_info(&t.uid)?, children: t.children.iter().filter_map(|c| conv(f, c)).collect() })
    }
    threads.iter().filter_map(|t| conv(folder, t)).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    account: String,
}

impl Store {
    pub fn account_uid(&self) -> &str {
        &self.account
    }

    pub fn folder_future(
        &self,
        name: &str,
        _flags: salutations_core::camel::StoreGetFolderFlags,
        _priority: i32,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Folder>, glib::Error>>>> {
        let (a, n) = (self.account.clone(), name.to_string());
        Box::pin(async move { Folder::open(&a, &n).await.map(Some) })
    }

    pub fn create_folder_sync(&self, parent: Option<&str>, name: &str, _c: Option<&gio::Cancellable>) -> Result<(), glib::Error> {
        call_sync("create_folder", args((self.account.clone(), parent.unwrap_or_default().to_string(), name.to_string()))).map(|_| ())
    }
}

/// The calls the window made on Camel's session.
#[derive(Clone)]
pub struct Session;

impl Session {
    pub async fn folder_future(&self, store: &Store, name: &str) -> Result<Folder, glib::Error> {
        Folder::open(&store.account, name).await
    }
}

pub fn mime_bytes(msg: &Real) -> Result<Vec<u8>, glib::Error> {
    let out = gio::MemoryOutputStream::new_resizable();
    msg.write_to_output_stream_sync(&out, None::<&gio::Cancellable>)?;
    out.close(None::<&gio::Cancellable>)?;
    Ok(out.steal_as_bytes().to_vec())
}

pub fn parse_message(bytes: &[u8]) -> Result<Real, glib::Error> {
    let msg = Real::new();
    let input = gio::MemoryInputStream::from_bytes(&glib::Bytes::from(bytes));
    msg.construct_from_input_stream_sync(&input, None::<&gio::Cancellable>)?;
    Ok(msg)
}

/// Sends `msg` from the account's outgoing server through the background
/// process.
pub async fn transmit(account: &str, msg: &Real, from: &str, to: &str) -> Result<(), glib::Error> {
    let bytes = mime_bytes(msg)?;
    call("transmit", args((account.to_string(), glib::Variant::array_from_fixed_array(&bytes), from.to_string(), to.to_string())))
        .await
        .map(|_| ())
}

/// Sends `out`: checked and signed or encrypted here, sent by the
/// background process. Returns the message as sent, for Sent.
pub async fn send_message(
    gpg: &salutations_core::camel::GpgContext,
    account: &Account,
    out: &salutations_core::compose::Outgoing,
    keys: Option<salutations_core::send::Keys>,
) -> Result<Real, glib::Error> {
    use salutations_core::send;
    let recipients = send::recipients(out)?;
    // Signing and encryption failures (a missing key, a cancelled
    // passphrase) are the message's problem, not the server's.
    let msg = send::prepare(gpg.clone(), out, keys, false)
        .await
        .map_err(|e| glib::Error::new(gio::IOErrorEnum::InvalidArgument, e.message()))?;
    let from = out.from_address().encode().map(|s| s.to_string()).unwrap_or_default();
    let to = recipients.encode().map(|s| s.to_string()).unwrap_or_default();
    transmit(&account.uid, &msg, &from, &to).await?;
    Ok(msg)
}
