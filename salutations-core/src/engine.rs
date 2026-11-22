//! Everything that keeps running while Salutations is open: accounts,
//! their stores and syncing, push checks, retries, Power Saver and metered
//! connections, and offline downloads and pruning. The window reads its state
//! and hears about changes through `Event`s.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};

use camel::prelude::*;
use edataserver::SourceRegistry;

use crate::debug::log;
use crate::{Account, Session, Settings, SpecialFolders, SyncPrefs, offline};

/// Network changes come in bursts; accounts are synced at most this often
/// because of them.
const NETWORK_RESYNC_SECONDS: u64 = 30;
/// Connecting and listing folders take at most this long; a connection
/// left dead by sleep or a network change otherwise hangs a sync for
/// minutes, and every later sync waits on it.
pub const STEP_LIMIT: std::time::Duration = std::time::Duration::from_secs(45);
/// Seconds before retrying a failed sync, by failures in a row; the last
/// repeats.
const RETRY_SECONDS: [u32; 4] = [30, 60, 120, 300];
/// Minutes between push connection checks, off and on metered connections.
const KEEPALIVE_MINUTES: u32 = 15;
const KEEPALIVE_METERED_MINUTES: u32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    Idle,
    Syncing,
    Offline,
    Error,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Accounts were added, removed, renamed, or reordered; see
    /// `Engine::accounts`.
    Accounts,
    SyncState(String),
    /// The account's folder tree, sorted; `rebuild` when the special
    /// folders changed, so icons and order must be redone.
    Folders { account: String, nodes: Vec<camel::FolderNode>, rebuild: bool },
    /// Paused for Power Saver, or resumed.
    Paused(String),
    /// INBOX is open and watched; see `AccountState::inbox`.
    InboxReady(String),
    /// A sync finished without error.
    Synced(String),
}

pub struct AccountState {
    pub account: Account,
    store: RefCell<Option<camel::Store>>,
    state: Cell<SyncState>,
    message: RefCell<String>,
    /// Ends the running sync; see `resync`.
    abort: RefCell<Option<futures_channel::oneshot::Sender<()>>>,
    /// A sync was asked for while one ran; it starts when that one ends.
    again: Cell<bool>,
    /// Failed syncs in a row, for the retry delay.
    failures: Cell<u32>,
    retry: RefCell<Option<glib::SourceId>>,
    watching: Cell<bool>,
    sync_started: Cell<Option<std::time::Instant>>,
    /// Saved by the last session, then checked against the server once
    /// per session.
    special: RefCell<Option<SpecialFolders>>,
    special_checked: Cell<bool>,
    sign_in_failed: Cell<bool>,
    /// The check-interval timer.
    timer: RefCell<Option<glib::SourceId>>,
    /// Paused for Power Saver, as last applied.
    paused: Cell<bool>,
    /// With push on: the periodic check that notices a dead push
    /// connection; see `schedule_keepalive`.
    keepalive: RefCell<Option<glib::SourceId>>,
    /// Folders kept offline as of the last maintenance run, to find the
    /// ones switched off since. Empty until the first run.
    synced: RefCell<HashSet<String>>,
    /// The window's edge at the last maintenance run; later runs prune
    /// only messages that crossed it since. None means check everything.
    last_cutoff: Cell<Option<i64>>,
    maintaining: Cell<bool>,
    /// Requested while a run was under way: Some(full).
    maintain_again: Cell<Option<bool>>,
    last_sync: Cell<Option<std::time::Instant>>,
    inbox: RefCell<Option<camel::Folder>>,
}

impl AccountState {
    pub fn uid(&self) -> &str {
        &self.account.uid
    }

    pub fn store(&self) -> Option<camel::Store> {
        self.store.borrow().clone()
    }

    pub fn state(&self) -> SyncState {
        self.state.get()
    }

    /// The last sync's error.
    pub fn message(&self) -> String {
        self.message.borrow().clone()
    }

    /// The last sync failed to sign in, so the error offers Online Accounts.
    pub fn sign_in_failed(&self) -> bool {
        self.sign_in_failed.get()
    }

    pub fn special(&self) -> Option<SpecialFolders> {
        self.special.borrow().clone()
    }

    pub fn inbox(&self) -> Option<camel::Folder> {
        self.inbox.borrow().clone()
    }

    pub fn connected(&self) -> bool {
        self.store.borrow().as_ref().is_some_and(|s| s.connection_status() == camel::ServiceConnectionStatus::Connected)
    }
}

type Listener = Box<dyn Fn(&Event)>;
type Waiter = Box<dyn Fn() -> Pin<Box<dyn Future<Output = ()>>>>;

pub struct Engine {
    pub registry: SourceRegistry,
    pub session: Session,
    accounts: RefCell<Vec<Rc<AccountState>>>,
    listeners: RefCell<Vec<Listener>>,
    /// The folder the window shows, refreshed by every sync of its account.
    open: RefCell<Option<camel::Folder>>,
    /// Awaited after each cached tree at startup, so the window can open a
    /// folder from it before the next account's tree is read.
    settle: RefCell<Option<Waiter>>,
    power_monitor: RefCell<Option<gio::PowerProfileMonitor>>,
    sleep_watch: RefCell<Option<gio::SignalSubscription>>,
}

impl Engine {
    pub fn new(registry: SourceRegistry, session: Session) -> Rc<Self> {
        Rc::new(Engine {
            registry,
            session,
            accounts: RefCell::new(Vec::new()),
            listeners: RefCell::new(Vec::new()),
            open: RefCell::new(None),
            settle: RefCell::new(None),
            power_monitor: RefCell::new(None),
            sleep_watch: RefCell::new(None),
        })
    }

    /// Loads the accounts, starts syncing them, and follows the registry, the
    /// network, sleep, and Power Saver.
    pub fn start(self: &Rc<Self>) {
        self.load_accounts();
        // The registry emits on the main context it was created on.
        let weak = glib::thread_guard::ThreadGuard::new(Rc::downgrade(self));
        crate::watch_mail_accounts(&self.registry, move |_| {
            if let Some(this) = weak.get_ref().upgrade() {
                this.load_accounts();
            }
        });
        self.watch_network_and_sleep();
        self.watch_power_and_metering();
    }

    pub fn connect(&self, f: impl Fn(&Event) + 'static) {
        self.listeners.borrow_mut().push(Box::new(f));
    }

    pub fn set_settle(&self, f: impl Fn() -> Pin<Box<dyn Future<Output = ()>>> + 'static) {
        *self.settle.borrow_mut() = Some(Box::new(f));
    }

    fn emit(&self, event: Event) {
        for l in self.listeners.borrow().iter() {
            l(&event);
        }
    }

    pub fn accounts(&self) -> Vec<Rc<AccountState>> {
        self.accounts.borrow().clone()
    }

    pub fn account(&self, uid: &str) -> Option<Rc<AccountState>> {
        self.accounts.borrow().iter().find(|a| a.account.uid == uid).cloned()
    }

    pub fn set_open(&self, folder: Option<camel::Folder>) {
        *self.open.borrow_mut() = folder;
    }

    /// Re-reads the accounts, after a change in the registry or of their
    /// order.
    pub fn load_accounts(self: &Rc<Self>) {
        let found = crate::mail_accounts(&self.registry);
        let mut accounts = self.accounts.borrow_mut();
        accounts.retain(|a| found.iter().any(|f| f.uid == a.account.uid));
        let mut new = Vec::new();
        // The user's order first, then the rest by name (as found).
        let saved = Settings::get().account_order;
        let mut order: Vec<String> = found.iter().map(|a| a.uid.clone()).collect();
        order.sort_by_key(|u| saved.iter().position(|s| s == u).unwrap_or(usize::MAX));
        for a in found {
            if accounts.iter().any(|c| c.account.uid == a.uid) {
                continue;
            }
            let special = SpecialFolders::load(&a.uid);
            let acct = Rc::new(AccountState {
                account: a,
                store: RefCell::new(None),
                state: Cell::new(SyncState::Idle),
                message: RefCell::new(String::new()),
                abort: RefCell::new(None),
                again: Cell::new(false),
                failures: Cell::new(0),
                retry: RefCell::new(None),
                watching: Cell::new(false),
                sync_started: Cell::new(None),
                special: RefCell::new(special),
                special_checked: Cell::new(false),
                sign_in_failed: Cell::new(false),
                timer: RefCell::new(None),
                paused: Cell::new(false),
                keepalive: RefCell::new(None),
                synced: RefCell::new(HashSet::new()),
                last_cutoff: Cell::new(None),
                maintaining: Cell::new(false),
                maintain_again: Cell::new(None),
                last_sync: Cell::new(None),
                inbox: RefCell::new(None),
            });
            accounts.push(acct.clone());
            new.push(acct);
        }
        accounts.sort_by_key(|c| order.iter().position(|u| *u == c.account.uid));
        drop(accounts);
        self.emit(Event::Accounts);
        let this = self.clone();
        glib::spawn_future_local(async move {
            // A paused account syncs when the pause ends, or when asked.
            for acct in &new {
                acct.paused.set(this.paused(acct));
                this.apply_holds(acct);
            }
            this.show_cached(&new).await;
            for acct in new {
                if !acct.paused.get() {
                    this.sync(&acct);
                }
            }
        });
    }

    /// Shows the cached folder trees before any connection. The session is
    /// offline meanwhile, since Camel connects a store on any folder call
    /// while it is online. Skipped when a store is already connecting or
    /// connected, as taking the session offline would fail its operations.
    async fn show_cached(self: &Rc<Self>, new: &[Rc<AccountState>]) {
        let camel_session = self.session.upcast_ref::<camel::Session>();
        // Stores are created online so they can later connect without
        // Camel's go-online step (see Session::connect_future()).
        let stores: Vec<_> = new.iter().filter_map(|c| self.store_for(c).ok().map(|s| (c.clone(), s))).collect();
        let busy = camel_session
            .list_services()
            .iter()
            .any(|s| s.connection_status() != camel::ServiceConnectionStatus::Disconnected);
        if busy || !camel_session.is_online() {
            return;
        }
        camel_session.set_online(false);
        for (acct, store) in &stores {
            if let Ok(cached) = self.session.folders_future(store, false).await {
                log(&format!("{}: cached tree, {} top-level folders", acct.account.uid, cached.len()));
                self.emit_folders(acct, &cached, false);
                let settle = self.settle.borrow().as_ref().map(|f| f());
                if let Some(s) = settle {
                    s.await;
                }
            }
        }
        camel_session.set_online(true);
    }

    fn emit_folders(&self, acct: &AccountState, nodes: &[camel::FolderNode], rebuild: bool) {
        let special = acct.special().unwrap_or_default();
        let nodes = sort_folders(nodes, &special);
        self.emit(Event::Folders { account: acct.account.uid.clone(), nodes, rebuild });
    }

    /// Lists the account's folders again and sends the tree, rebuilt.
    pub async fn refresh_folders(&self, acct: &AccountState) {
        let Some(store) = acct.store() else { return };
        if let Ok(nodes) = self.session.folders_future(&store, true).await {
            self.emit_folders(acct, &nodes, true);
        }
    }

    pub fn set_special(&self, acct: &AccountState, special: SpecialFolders) {
        if let Err(e) = special.save(&acct.account.uid) {
            glib::g_warning!("salutations", "Cannot save special folders: {}", e);
        }
        *acct.special.borrow_mut() = Some(special);
    }

    pub fn store_for(&self, acct: &AccountState) -> Result<camel::Store, glib::Error> {
        if let Some(s) = acct.store.borrow().as_ref() {
            return Ok(s.clone());
        }
        offline::keep_check_all_off(&self.registry, &acct.account);
        let s = self.session.store(&acct.account)?;
        *acct.store.borrow_mut() = Some(s.clone());
        self.apply_holds(acct);
        Ok(s)
    }

    fn set_state(&self, acct: &AccountState, state: SyncState) {
        acct.state.set(state);
        self.emit(Event::SyncState(acct.account.uid.clone()));
    }

    /// Ends the running sync, if any.
    pub fn cancel_sync(&self, acct: &AccountState) {
        if let Some(tx) = acct.abort.borrow_mut().take() {
            let _ = tx.send(());
        }
    }

    /// A connection dropped outside a sync (the server closed it, or push
    /// ended) is restored by syncing again shortly.
    fn watch_connection(self: &Rc<Self>, acct: &Rc<AccountState>, store: &camel::Store) {
        if acct.watching.replace(true) {
            return;
        }
        let refs = std::sync::Arc::new(glib::thread_guard::ThreadGuard::new((Rc::downgrade(self), Rc::downgrade(acct))));
        store.connect_notify(Some("connection-status"), move |store, _| {
            if store.connection_status() != camel::ServiceConnectionStatus::Disconnected {
                return;
            }
            // Camel notifies from its worker threads too; the references are
            // only touched on the main thread.
            let refs = refs.clone();
            glib::MainContext::default().invoke(move || {
                let (Some(this), Some(acct)) = (refs.get_ref().0.upgrade(), refs.get_ref().1.upgrade()) else { return };
                // Only a drop after a good sync: failed attempts have a growing
                // retry delay.
                if acct.state.get() == SyncState::Idle
                    && acct.retry.borrow().is_none()
                    && gio::NetworkMonitor::default().is_network_available()
                {
                    this.schedule_retry(&acct, 5);
                }
            });
        });
    }

    /// Disconnects and syncs again, so a changed connection setting applies.
    pub fn reconnect(self: &Rc<Self>, acct: &Rc<AccountState>) {
        self.resync(acct, true);
    }

    /// Ends a running sync and starts another; `drop_connection` first
    /// closes the connection without the polite logout, for sockets that
    /// sleep or a network change may have left dead.
    pub fn resync(self: &Rc<Self>, acct: &Rc<AccountState>, drop_connection: bool) {
        log(&format!("{}: resync (dropping the connection: {drop_connection})", acct.account.uid));
        self.cancel_sync(acct);
        let Some(store) = acct.store() else {
            self.sync(acct);
            return;
        };
        let (this, acct) = (self.clone(), acct.clone());
        glib::spawn_future_local(async move {
            if drop_connection {
                let _ = glib::future_with_timeout(
                    std::time::Duration::from_secs(5),
                    store.disconnect_future(false, glib::ffi::G_PRIORITY_DEFAULT),
                )
                .await;
            }
            // The ended sync gives up its state when it notices the abort,
            // which is after this point.
            if acct.state.get() == SyncState::Syncing {
                acct.again.set(true);
            } else {
                this.sync(&acct);
            }
        });
    }

    /// Tries again after `seconds`, replacing any retry already waiting.
    /// Without a network, the network's return syncs instead.
    fn schedule_retry(self: &Rc<Self>, acct: &Rc<AccountState>, seconds: u32) {
        if let Some(id) = acct.retry.borrow_mut().take() {
            id.remove();
        }
        if self.paused(acct) || !gio::NetworkMonitor::default().is_network_available() {
            return;
        }
        let (weak, a) = (Rc::downgrade(self), Rc::downgrade(acct));
        let id = glib::timeout_add_seconds_local_once(seconds, move || {
            let (Some(this), Some(acct)) = (weak.upgrade(), a.upgrade()) else { return };
            acct.retry.borrow_mut().take();
            this.sync(&acct);
        });
        *acct.retry.borrow_mut() = Some(id);
    }

    /// Retries a failed sync, less often each time but never less often
    /// than the account is checked. An account never checked stops
    /// retrying after the longest step.
    fn retry_failed(self: &Rc<Self>, acct: &Rc<AccountState>) {
        let n = acct.failures.get() as usize;
        acct.failures.set(acct.failures.get().saturating_add(1));
        let interval = match self.refresh_now(acct) {
            offline::Refresh::Minutes(m) => m.saturating_mul(60),
            offline::Refresh::Never if n >= RETRY_SECONDS.len() => return,
            offline::Refresh::Never => u32::MAX,
        };
        let delay = RETRY_SECONDS[n.min(RETRY_SECONDS.len() - 1)].min(interval);
        self.schedule_retry(acct, delay);
    }

    /// Connects, refreshes the folder tree and the open folder.
    pub fn sync(self: &Rc<Self>, acct: &Rc<AccountState>) {
        if acct.state.get() == SyncState::Syncing {
            acct.again.set(true);
            return;
        }
        if let Some(id) = acct.retry.borrow_mut().take() {
            id.remove();
        }
        let (tx, rx) = futures_channel::oneshot::channel::<()>();
        *acct.abort.borrow_mut() = Some(tx);
        acct.sync_started.set(Some(std::time::Instant::now()));
        self.set_state(acct, SyncState::Syncing);
        let (this, acct) = (self.clone(), acct.clone());
        glib::spawn_future_local(async move {
            // Dropping the sync's future when aborted cancels its Camel calls.
            let result = match race(this.sync_inner(&acct), rx).await {
                Either::Left(r) => Some(r),
                Either::Right(_) => None,
            };
            acct.abort.borrow_mut().take();
            match result {
                None => this.set_state(&acct, SyncState::Idle),
                Some(Ok(())) => {
                    acct.failures.set(0);
                    this.set_state(&acct, SyncState::Idle);
                    this.emit(Event::Synced(acct.account.uid.clone()));
                }
                Some(Err(e)) => {
                    log(&format!("{}: sync failed: {}", acct.account.uid, e.message()));
                    let auth = e.matches(camel::ServiceError::CantAuthenticate);
                    acct.sign_in_failed.set(auth);
                    *acct.message.borrow_mut() = e.message().to_string();
                    this.set_state(&acct, if is_offline(&e) { SyncState::Offline } else { SyncState::Error });
                    // A rejected sign-in waits for the user; anything else
                    // is tried again, less often each time.
                    if !auth {
                        this.retry_failed(&acct);
                    }
                }
            }
            if acct.again.replace(false) {
                this.sync(&acct);
            }
        });
    }

    async fn sync_inner(self: &Rc<Self>, acct: &Rc<AccountState>) -> Result<(), glib::Error> {
        let store = self.store_for(acct)?;
        log(&format!("{}: sync start", acct.account.uid));
        self.watch_connection(acct, &store);
        within(STEP_LIMIT, self.session.connect_future(&store)).await?;
        log(&format!("{}: connected", acct.account.uid));
        let fresh = within(STEP_LIMIT, self.session.folders_future(&store, true)).await?;
        log(&format!("{}: folder list refreshed", acct.account.uid));
        self.emit_folders(acct, &fresh, false);
        let open = self.open.borrow().clone().filter(|f| f.parent_store().as_ref() == Some(&store));
        let refreshed = open.as_ref().and_then(|f| f.full_name()).map(|n| n.to_string());
        if let Some(folder) = open {
            folder.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT).await?;
            log(&format!("{}: open folder refreshed", acct.account.uid));
        }
        if !acct.special_checked.get()
            && let Ok(special) = self.session.special_folders(&store).await
        {
            let special = special.merged(acct.special.borrow().as_ref());
            log(&format!("{}: special folders checked", acct.account.uid));
            acct.special_checked.set(true);
            if acct.special.borrow().as_ref() != Some(&special) {
                self.set_special(acct, special);
                self.emit_folders(acct, &fresh, true);
            }
        }
        if acct.timer.borrow().is_none() {
            self.first_sync_setup(acct, &store).await;
            self.schedule_refresh(acct);
        }
        self.open_inbox(acct, &store).await;
        acct.last_sync.set(Some(std::time::Instant::now()));
        // The first run of a session checks every message against the window.
        let full = acct.last_cutoff.get().is_none();
        self.maintain(acct, full, refreshed);
        Ok(())
    }

    /// Opens INBOX once per session and keeps it open for new mail.
    async fn open_inbox(self: &Rc<Self>, acct: &Rc<AccountState>, store: &camel::Store) {
        if acct.inbox.borrow().is_some() {
            return;
        }
        let Ok(folder) = self.session.folder_future(store, "INBOX").await else { return };
        if acct.inbox.borrow().is_some() {
            return;
        }
        let refs = glib::thread_guard::ThreadGuard::new((Rc::downgrade(self), Rc::downgrade(acct)));
        folder.connect_changed(move |folder, changes| {
            let (Some(this), Some(acct)) = (refs.get_ref().0.upgrade(), refs.get_ref().1.upgrade()) else { return };
            if !changes.added.is_empty() {
                this.download_arrivals(&acct, folder);
            }
        });
        *acct.inbox.borrow_mut() = Some(folder);
        self.emit(Event::InboxReady(acct.account.uid.clone()));
    }

    /// Brings the account's offline copies in line with its settings, in
    /// the background: refreshes and downloads the folders kept offline,
    /// deletes bodies that fell out of the window, and deletes all bodies of
    /// folders no longer kept. `full` (after a settings change) checks every
    /// message against the window instead of only those that crossed its
    /// edge since the last run.
    /// `refreshed` names a folder the caller has just refreshed, which the
    /// run does not refresh again.
    pub fn maintain(self: &Rc<Self>, acct: &Rc<AccountState>, full: bool, refreshed: Option<String>) {
        let Some(store) = acct.store() else { return };
        if store.connection_status() != camel::ServiceConnectionStatus::Connected || self.paused(acct) {
            return;
        }
        if acct.maintaining.replace(true) {
            // The rerun refreshes everything, the skipped folder included.
            let again = acct.maintain_again.get().unwrap_or(false) || full;
            acct.maintain_again.set(Some(again));
            return;
        }
        let settings = offline::read(&self.registry, &acct.account).unwrap_or(offline::DEFAULT);
        let prefs = Settings::get().sync_for(&acct.account.uid);
        let metered = metered();
        let now = glib::DateTime::now_utc().ok();
        let expression = self.download_plan(acct, &prefs, settings.window);
        // Only the window itself deletes, and not on metered connections,
        // where a narrower window only limits downloading.
        let cutoff = if metered || prefs.never_download {
            None
        } else {
            now.as_ref().and_then(|now| settings.window.cutoff(now))
        };
        let since = if full { None } else { acct.last_cutoff.get() };
        let previous = acct.synced.borrow().clone();
        let (this, acct) = (self.clone(), acct.clone());
        glib::spawn_future_local(async move {
            let s = store.clone();
            let result = gio::spawn_blocking(move || {
                let Some(o) = s.dynamic_cast_ref::<camel::OfflineStore>() else { return (HashSet::new(), 0, 0) };
                let folders = o.dup_downsync_folders();
                let names: HashSet<String> =
                    folders.iter().filter_map(|f| f.full_name().map(|n| n.to_string())).collect();
                let mut pruned = 0;
                for f in &folders {
                    // Camel downloads only messages its summary knows, so
                    // the folder is refreshed first.
                    if let Some(of) = f.dynamic_cast_ref::<camel::OfflineFolder>()
                        && let Some(expression) = &expression
                    {
                        let fresh = refreshed.is_some() && f.full_name().as_deref() == refreshed.as_deref();
                        let refresh = if fresh { Ok(()) } else { f.refresh_info_sync(None::<&gio::Cancellable>) };
                        let _ = refresh.and_then(|()| of.downsync_sync(expression.as_deref(), None::<&gio::Cancellable>));
                    }
                    if let Some(c) = cutoff {
                        pruned += offline::prune(f, c, since);
                    }
                }
                let mut cleared = 0;
                for name in previous.difference(&names) {
                    if let Ok(Some(f)) = s.folder_sync(name, camel::StoreGetFolderFlags::empty(), None::<&gio::Cancellable>) {
                        offline::clear_folder(&f);
                        cleared += 1;
                    }
                }
                (names, pruned, cleared)
            })
            .await;
            acct.maintaining.set(false);
            if let Ok((names, pruned, cleared)) = result {
                log(&format!(
                    "{}: offline maintenance: {} folders kept, {pruned} bodies pruned, {cleared} folders cleared",
                    acct.account.uid,
                    names.len()
                ));
                *acct.synced.borrow_mut() = names;
                // Unpruned runs leave the edge where the last pruning put it.
                if cutoff.is_some() {
                    acct.last_cutoff.set(cutoff);
                }
            }
            if let Some(full) = acct.maintain_again.take() {
                this.maintain(&acct, full, None);
            }
        });
    }

    /// First sync of the session: applies the defaults to an account
    /// Salutations has not configured, then starts the check timer.
    async fn first_sync_setup(&self, acct: &AccountState, store: &camel::Store) {
        let uid = &acct.account.uid;
        if !offline::configured(uid, "offline") {
            let written = offline::write(&self.registry, &acct.account, &offline::DEFAULT);
            if let Ok(inbox) = self.session.folder_future(store, "INBOX").await {
                offline::set_folder_synced(&inbox, true);
            }
            match written {
                Ok(()) => offline::mark_configured(uid, "offline"),
                Err(e) => glib::g_warning!("salutations", "Cannot save offline settings: {}", e),
            }
        }
        if !offline::configured(uid, "refresh") {
            match offline::write_refresh(&self.registry, &acct.account, offline::DEFAULT_REFRESH) {
                Ok(()) => offline::mark_configured(uid, "refresh"),
                Err(e) => glib::g_warning!("salutations", "Cannot save the check interval: {}", e),
            }
        }
    }

    /// (Re)starts the account's check timer from its setting.
    pub fn schedule_refresh(self: &Rc<Self>, acct: &Rc<AccountState>) {
        if let Some(id) = acct.timer.borrow_mut().take() {
            id.remove();
        }
        self.schedule_keepalive(acct);
        if self.paused(acct) {
            return;
        }
        let offline::Refresh::Minutes(m) = self.refresh_now(acct) else { return };
        let (weak, a) = (Rc::downgrade(self), Rc::downgrade(acct));
        let id = glib::timeout_add_seconds_local(m.saturating_mul(60), move || {
            let (Some(this), Some(acct)) = (weak.upgrade(), a.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            this.sync(&acct);
            glib::ControlFlow::Continue
        });
        *acct.timer.borrow_mut() = Some(id);
    }

    /// Whether the account does nothing automatic now: Power Saver is on and
    /// the account pauses for it.
    pub fn paused(&self, acct: &AccountState) -> bool {
        power_saver() && Settings::get().sync_for(&acct.account.uid).pause_power_save
    }

    /// The check interval for the connection in use.
    fn refresh_now(&self, acct: &AccountState) -> offline::Refresh {
        let normal = offline::read_refresh(&self.registry, &acct.account).unwrap_or(offline::DEFAULT_REFRESH);
        let metered_refresh = Settings::get().sync_for(&acct.account.uid).metered_refresh;
        match metered_refresh {
            Some(r) if metered() => r,
            _ => normal,
        }
    }

    /// "Download From" for the connection in use.
    fn download_now(&self, acct: &AccountState, prefs: &SyncPrefs) -> offline::Download {
        let normal = if prefs.never_download {
            offline::Download::Never
        } else {
            offline::Download::Within(offline::read(&self.registry, &acct.account).unwrap_or(offline::DEFAULT).window)
        };
        // A metered window never reaches past the normal one.
        match (prefs.metered_download, glib::DateTime::now_utc()) {
            (Some(d), Ok(now)) if metered() => d.min(normal, &now),
            _ => normal,
        }
    }

    /// Applies Power Saver and the connection to the account: holds push
    /// and Camel's downloads while it is paused, and Camel's downloads
    /// while it downloads nothing. Leaving the pause syncs once.
    pub fn apply_conditions(self: &Rc<Self>, acct: &Rc<AccountState>) {
        let paused = self.paused(acct);
        let was = acct.paused.replace(paused);
        self.apply_holds(acct);
        if paused && !was {
            log(&format!("{}: paused for Power Saver", acct.account.uid));
            for slot in [&acct.retry, &acct.timer, &acct.keepalive] {
                if let Some(id) = slot.borrow_mut().take() {
                    id.remove();
                }
            }
            self.cancel_sync(acct);
            // Closing the connection ends push.
            if let Some(store) = acct.store() {
                glib::spawn_future_local(async move {
                    let _ = glib::future_with_timeout(
                        std::time::Duration::from_secs(5),
                        store.disconnect_future(true, glib::ffi::G_PRIORITY_DEFAULT),
                    )
                    .await;
                });
            }
        } else if was && !paused {
            log(&format!("{}: resumed after Power Saver", acct.account.uid));
            self.schedule_refresh(acct);
            self.resync(acct, true);
        }
        self.emit(Event::Paused(acct.account.uid.clone()));
    }

    /// What maintenance downloads: None for nothing, Some(None) for Camel's
    /// window, Some(expression) for a narrower downsync expression.
    fn download_plan(&self, acct: &AccountState, prefs: &SyncPrefs, window: offline::Window) -> Option<Option<String>> {
        match self.download_now(acct, prefs) {
            offline::Download::Never => None,
            offline::Download::Within(w) => {
                let now = glib::DateTime::now_utc().ok()?;
                Some(w.cutoff(&now).and_then(|c| offline::narrower(window, c, &now)))
            }
        }
    }

    /// Camel downloads by the account's window, so its downloads are held
    /// whenever another applies; Salutations downloads instead.
    fn camel_downloads_held(&self, acct: &AccountState, prefs: &SyncPrefs) -> bool {
        self.paused(acct)
            || self.download_now(acct, prefs) == offline::Download::Never
            || (metered() && prefs.metered_download.is_some())
    }

    fn apply_holds(&self, acct: &AccountState) {
        let paused = self.paused(acct);
        offline::hold_push(&self.registry, &acct.account, paused);
        let prefs = Settings::get().sync_for(&acct.account.uid);
        let hold = self.camel_downloads_held(acct, &prefs);
        if let Some(store) = acct.store.borrow().as_ref() {
            crate::downsync::hold(store, hold);
        }
    }

    /// New INBOX messages, downloaded by the metered window while Camel's
    /// downloads are held for it. Other folders wait for the next sync.
    fn download_arrivals(&self, acct: &AccountState, folder: &camel::Folder) {
        let prefs = Settings::get().sync_for(&acct.account.uid);
        if self.paused(acct) || !self.camel_downloads_held(acct, &prefs) {
            return;
        }
        let window = offline::read(&self.registry, &acct.account).unwrap_or(offline::DEFAULT).window;
        let Some(expression) = self.download_plan(acct, &prefs, window) else { return };
        let Some(of) = folder.dynamic_cast_ref::<camel::OfflineFolder>().cloned() else { return };
        gio::spawn_blocking(move || {
            let _ = of.downsync_sync(expression.as_deref(), None::<&gio::Cancellable>);
        });
    }

    /// Power Saver and metered connections change how each account syncs.
    fn watch_power_and_metering(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let power = gio::PowerProfileMonitor::get_default();
        power.connect_power_saver_enabled_notify(move |_| {
            let Some(this) = weak.upgrade() else { return };
            for acct in &this.accounts() {
                this.apply_conditions(acct);
            }
        });
        *self.power_monitor.borrow_mut() = Some(power);
        let weak = Rc::downgrade(self);
        gio::NetworkMonitor::default().connect_network_metered_notify(move |monitor| {
            let Some(this) = weak.upgrade() else { return };
            log(&format!("network metered: {}", monitor.is_network_metered()));
            for acct in &this.accounts() {
                this.schedule_refresh(acct);
                this.apply_conditions(acct);
                // Off a metered connection, the account's window downloads and
                // deletes again.
                if !monitor.is_network_metered() {
                    this.maintain(acct, false, None);
                }
            }
        });
    }

    /// Camel does not notice a push connection that went silent (a dropped
    /// route, a NAT that forgot it), and no mail arrives until the next
    /// check. While push is on, INBOX is checked every so often, which
    /// fails on a dead connection and so reconnects; a sync in the
    /// meantime counts as the check.
    pub fn schedule_keepalive(self: &Rc<Self>, acct: &Rc<AccountState>) {
        if let Some(id) = acct.keepalive.borrow_mut().take() {
            id.remove();
        }
        if self.paused(acct) || !offline::read_push(&self.registry, &acct.account) {
            return;
        }
        let minutes = if metered() { KEEPALIVE_METERED_MINUTES } else { KEEPALIVE_MINUTES };
        let (weak, a) = (Rc::downgrade(self), Rc::downgrade(acct));
        let id = glib::timeout_add_seconds_local(minutes * 60, move || {
            let (Some(this), Some(acct)) = (weak.upgrade(), a.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            let recent = acct.last_sync.get().is_some_and(|t| t.elapsed().as_secs() < u64::from(minutes) * 60 - 5);
            if !recent
                && acct.state.get() == SyncState::Idle
                && let Some(inbox) = acct.inbox()
            {
                glib::spawn_future_local(async move {
                    let check = within(STEP_LIMIT, inbox.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT)).await;
                    if let Err(e) = check {
                        log(&format!("{}: push check failed: {}", acct.account.uid, e.message()));
                        this.resync(&acct, true);
                    }
                });
            }
            glib::ControlFlow::Continue
        });
        *acct.keepalive.borrow_mut() = Some(id);
    }

    /// Push stops when the connection drops, so every account is synced when
    /// the network returns and after the computer wakes, without waiting for
    /// its timer.
    fn watch_network_and_sleep(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        gio::NetworkMonitor::default().connect_network_changed(move |_, available| {
            let Some(this) = weak.upgrade().filter(|_| available) else { return };
            for acct in this.accounts().into_iter().filter(|a| !this.paused(a)) {
                let recent = |t: Option<std::time::Instant>| t.is_some_and(|t| t.elapsed().as_secs() < NETWORK_RESYNC_SECONDS);
                // A sync that just started (at launch, say) already uses the
                // network as it is now.
                let starting = acct.state.get() == SyncState::Syncing && recent(acct.sync_started.get());
                // Before the first sync (the monitor reports at launch too),
                // the sync on its way covers it.
                let first = acct.last_sync.get().is_none() && !matches!(acct.state.get(), SyncState::Offline | SyncState::Error);
                if !recent(acct.last_sync.get()) && !starting && !first {
                    this.resync(&acct, true);
                }
            }
        });
        // logind's PrepareForSleep(false) is sent on waking. A Flatpak needs
        // --system-talk-name=org.freedesktop.login1 for it.
        let weak: Weak<Engine> = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(bus) = gio::bus_get_future(gio::BusType::System).await else { return };
            let watch_weak = weak.clone();
            let sub = bus.subscribe_to_signal(
                Some("org.freedesktop.login1"),
                Some("org.freedesktop.login1.Manager"),
                Some("PrepareForSleep"),
                Some("/org/freedesktop/login1"),
                None,
                gio::DBusSignalFlags::NONE,
                move |signal| {
                    let sleeping = signal.parameters.get::<(bool,)>().is_some_and(|(s,)| s);
                    if sleeping {
                        return;
                    }
                    if let Some(this) = watch_weak.upgrade() {
                        for acct in this.accounts().into_iter().filter(|a| !this.paused(a)) {
                            this.resync(&acct, true);
                        }
                    }
                },
            );
            if let Some(this) = weak.upgrade() {
                *this.sleep_watch.borrow_mut() = Some(sub);
            }
        });
    }
}

pub fn sort_folders(nodes: &[camel::FolderNode], s: &SpecialFolders) -> Vec<camel::FolderNode> {
    let rank = |n: &camel::FolderNode| -> usize {
        let is = |f: &Option<String>| f.as_deref() == Some(n.full_name.as_str());
        match () {
            _ if n.full_name.eq_ignore_ascii_case("INBOX") => 0,
            _ if is(&s.drafts) => 1,
            _ if is(&s.sent) => 2,
            _ if is(&s.junk) => 3,
            _ if is(&s.trash) => 4,
            _ if is(&s.archive) => 5,
            _ => 6,
        }
    };
    let mut out = nodes.to_vec();
    out.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()))
    });
    out
}

pub fn metered() -> bool {
    gio::NetworkMonitor::default().is_network_metered()
}

fn power_saver() -> bool {
    gio::PowerProfileMonitor::get_default().is_power_saver_enabled()
}

pub enum Either<A, B> {
    Left(A),
    Right(B),
}

/// Whichever of `a` and `b` finishes first; the other is dropped.
pub async fn race<A: Future, B: Future>(a: A, b: B) -> Either<A::Output, B::Output> {
    let (mut a, mut b) = (std::pin::pin!(a), std::pin::pin!(b));
    std::future::poll_fn(move |cx| {
        if let std::task::Poll::Ready(v) = a.as_mut().poll(cx) {
            return std::task::Poll::Ready(Either::Left(v));
        }
        if let std::task::Poll::Ready(v) = b.as_mut().poll(cx) {
            return std::task::Poll::Ready(Either::Right(v));
        }
        std::task::Poll::Pending
    })
    .await
}

/// `step`, failing as timed out after `limit`.
pub async fn within<T>(limit: std::time::Duration, step: impl Future<Output = Result<T, glib::Error>>) -> Result<T, glib::Error> {
    glib::future_with_timeout(limit, step)
        .await
        .unwrap_or_else(|_| Err(glib::Error::new(gio::IOErrorEnum::TimedOut, "The server did not answer in time")))
}

fn is_offline(e: &glib::Error) -> bool {
    use gio::IOErrorEnum as E;
    [E::NetworkUnreachable, E::HostUnreachable, E::HostNotFound, E::TimedOut, E::ConnectionRefused]
        .iter()
        .any(|k| e.matches(*k))
        || e.matches(camel::ServiceError::Unavailable)
}
