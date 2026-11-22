//! The background process's state: the engine, notifications, the window
//! process and its private connection, and the Camel calls the window
//! makes through it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::rc::Rc;

use gio::prelude::*;
use salutations_core::camel::{self, prelude::*};
use salutations_core::debug::log;
use salutations_core::engine::{self, AccountState, Engine};
use salutations_core::ipc::{self, AccountInfo, Changes, Info, Node, Special, Thread};
use salutations_core::{Settings, send};

use crate::notify::Notifier;
use crate::people::People;

pub(crate) const SEEN: u32 = camel::MessageFlags::SEEN.bits();
pub(crate) const FLAGGED: u32 = camel::MessageFlags::FLAGGED.bits();
pub(crate) const DELETED: u32 = camel::MessageFlags::DELETED.bits();
pub(crate) const JUNK: u32 = camel::MessageFlags::JUNK.bits();

/// A message as the rules and notifications compare it: subject, date
/// sent, and sender. Moves into a watched folder made by Salutations
/// register theirs, so they are not taken for new mail.
pub(crate) type MessageKey = (String, i64, String);

pub(crate) fn message_key(folder: &camel::Folder, uid: &str) -> Option<MessageKey> {
    let info = folder.message_info(uid)?;
    Some((
        info.subject().map(|s| s.to_string()).unwrap_or_default(),
        info.date_sent(),
        info.from().map(|s| s.to_string()).unwrap_or_default(),
    ))
}

/// Per account: what notifications and the outbox keep.
#[derive(Default)]
pub(crate) struct AccountExtra {
    /// Watched folders by full name.
    pub watches: RefCell<std::collections::HashMap<String, Rc<crate::inbox::Watch>>>,
    /// Folders being opened to be watched.
    pub opening: RefCell<std::collections::HashSet<String>>,
    /// The combined notification's messages: folder and UID.
    pub combined: RefCell<Vec<(String, String)>>,
    /// Messages Salutations is moving into a watched folder, by folder.
    pub expected: RefCell<Vec<(String, MessageKey)>>,
    pub unread: Cell<Option<i64>>,
    pub sending: Cell<bool>,
    pub sending_uid: RefCell<Option<String>>,
}

/// What the window says it shows, so mail arriving there does not notify.
#[derive(Default, Clone)]
pub(crate) struct Shown {
    pub active: bool,
    pub account: String,
    pub folder: String,
}

/// Carries Camel objects across `spawn_blocking`; only one thread touches
/// them at a time.
pub(crate) struct Handoff<T>(pub T);
unsafe impl<T> Send for Handoff<T> {}

pub struct Daemon {
    pub app: gio::Application,
    pub engine: Rc<Engine>,
    pub notifier: Rc<Notifier>,
    pub people: Rc<People>,
    pub(crate) gpg: camel::GpgContext,
    pub(crate) extra: RefCell<HashMap<String, Rc<AccountExtra>>>,
    pub(crate) shown: RefCell<Shown>,
    /// The window process and its connection, once it has said hello.
    window: RefCell<Option<gio::Subprocess>>,
    conn: RefCell<Option<gio::DBusConnection>>,
    ready: Cell<bool>,
    /// Events for a window that has not said hello yet.
    pending: RefCell<Vec<(String, glib::Variant)>>,
    /// Folders the window has opened, kept open until it closes.
    folders: RefCell<HashMap<(String, String), (camel::Folder, glib::SignalHandlerId)>>,
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    /// The window writes the settings file; changes apply here too.
    settings_monitor: RefCell<Vec<gio::FileMonitor>>,
    quitting: Cell<bool>,
}

impl Daemon {
    pub fn new(app: &gio::Application, engine: Rc<Engine>, people: Rc<People>) -> Rc<Daemon> {
        let this = Rc::new(Daemon {
            app: app.clone(),
            gpg: salutations_core::crypto::context(&engine.session),
            engine,
            notifier: Notifier::new(),
            people,
            extra: RefCell::new(HashMap::new()),
            shown: RefCell::new(Shown::default()),
            window: RefCell::new(None),
            conn: RefCell::new(None),
            ready: Cell::new(false),
            pending: RefCell::new(Vec::new()),
            folders: RefCell::new(HashMap::new()),
            hold: RefCell::new(None),
            settings_monitor: RefCell::new(Vec::new()),
            quitting: Cell::new(false),
        });
        let weak = Rc::downgrade(&this);
        this.engine.connect(move |event| {
            if let Some(this) = weak.upgrade() {
                this.engine_event(event);
            }
        });
        let weak = Rc::downgrade(&this);
        this.notifier.connect_action(move |action| {
            if let Some(this) = weak.upgrade() {
                this.notification_action(action);
            }
        });
        this.update_hold();
        this.watch_settings();
        this
    }

    /// Follows the settings and rules files, which the window writes: what
    /// keeps the process running, and which folders are watched.
    fn watch_settings(self: &Rc<Self>) {
        let mut monitors = Vec::new();
        for (path, settings) in [(salutations_core::settings::path(), true), (salutations_core::rules::path(), false)] {
            let file = gio::File::for_path(path);
            let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::NONE, None::<&gio::Cancellable>) else { continue };
            let weak = Rc::downgrade(self);
            monitor.connect_changed(move |_, _, _, event| {
                if !matches!(event, gio::FileMonitorEvent::ChangesDoneHint | gio::FileMonitorEvent::Created | gio::FileMonitorEvent::Deleted) {
                    return;
                }
                let Some(this) = weak.upgrade() else { return };
                if settings {
                    Settings::reload();
                    this.update_hold();
                }
                for a in this.engine.accounts() {
                    this.update_watches(&a);
                }
            });
            monitors.push(monitor);
        }
        *self.settings_monitor.borrow_mut() = monitors;
    }

    pub(crate) fn extra(&self, account: &str) -> Rc<AccountExtra> {
        self.extra.borrow_mut().entry(account.to_string()).or_default().clone()
    }

    /// Kept running while a window is open or running in background is on.
    pub fn update_hold(&self) {
        let want = (self.window.borrow().is_some() || Settings::get().run_in_background) && !self.quitting.get();
        let mut hold = self.hold.borrow_mut();
        match (want, hold.is_some()) {
            (true, false) => *hold = Some(self.app.hold()),
            (false, true) => *hold = None,
            _ => {}
        }
    }

    fn engine_event(self: &Rc<Self>, event: &engine::Event) {
        match event {
            engine::Event::Accounts => {
                let infos: Vec<AccountInfo> = self.engine.accounts().iter().map(|a| self.account_info(a)).collect();
                self.emit("accounts", infos.to_variant());
            }
            engine::Event::SyncState(uid) | engine::Event::Paused(uid) => {
                if let Some(a) = self.engine.account(uid) {
                    self.emit("account", self.account_info(&a).to_variant());
                }
            }
            engine::Event::Folders { account, nodes, rebuild } => {
                if let Some(a) = self.engine.account(account) {
                    // The tree may come with new special folders.
                    self.emit("account", self.account_info(&a).to_variant());
                }
                let nodes: Vec<Node> = ipc::flatten_nodes(nodes);
                self.emit("folders", (account.clone(), nodes, *rebuild).to_variant());
            }
            engine::Event::InboxReady(uid) => {
                if let Some(a) = self.engine.account(uid) {
                    self.update_watches(&a);
                    self.emit("account", self.account_info(&a).to_variant());
                }
            }
            engine::Event::Synced(uid) => {
                if let Some(a) = self.engine.account(uid) {
                    self.retry_waiting(&a);
                    self.refresh_watched(&a);
                }
                self.update_badge();
                self.emit("synced", uid.to_variant());
            }
        }
    }

    pub(crate) fn account_info(&self, a: &AccountState) -> AccountInfo {
        AccountInfo {
            uid: a.uid().to_string(),
            state: match a.state() {
                engine::SyncState::Idle => "idle",
                engine::SyncState::Syncing => "syncing",
                engine::SyncState::Offline => "offline",
                engine::SyncState::Error => "error",
            }
            .into(),
            message: a.message(),
            sign_in_failed: a.sign_in_failed(),
            paused: self.engine.paused(a),
            connected: a.connected(),
            special: Special::from_folders(a.special().as_ref()),
            inbox: a.inbox().is_some(),
        }
    }

    /// Sends an event to the window, or keeps it until the window is ready.
    pub(crate) fn emit(&self, name: &str, data: glib::Variant) {
        let conn = self.conn.borrow().clone();
        match conn {
            Some(conn) if self.ready.get() => {
                let _ = conn.emit_signal(
                    None,
                    ipc::PATH,
                    ipc::IFACE,
                    "Event",
                    Some(&glib::Variant::tuple_from_iter([name.to_variant(), glib::Variant::from_variant(&data)])),
                );
            }
            _ => {
                // State events are sent whole at hello; only requests wait.
                if matches!(name, "present" | "compose" | "import" | "notification") {
                    self.pending.borrow_mut().push((name.to_string(), data));
                }
            }
        }
    }

    pub fn window_open(&self) -> bool {
        self.window.borrow().is_some()
    }

    /// Shows the window, starting the window process when there is none.
    pub fn show_window(self: &Rc<Self>) {
        if self.window_open() {
            self.emit("present", ipc::unit());
            return;
        }
        if let Err(e) = self.spawn_window() {
            glib::g_warning!("salutations", "Cannot start the window: {}", e);
        }
    }

    fn spawn_window(self: &Rc<Self>) -> Result<(), glib::Error> {
        let failed = |e: std::io::Error| glib::Error::new(gio::IOErrorEnum::Failed, &e.to_string());
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().map_err(failed)?;
        let exe = std::env::current_exe().map_err(failed)?.with_file_name("salutations-window");
        let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::NONE);
        launcher.setenv(ipc::FD_ENV, ipc::FD.to_string(), true);
        launcher.take_fd(OwnedFd::from(theirs), unsafe { BorrowedFd::borrow_raw(ipc::FD) });
        let child = launcher.spawn(&[exe.as_os_str()])?;
        log("window process started");
        *self.window.borrow_mut() = Some(child.clone());
        self.update_hold();
        let socket = gio::Socket::from_fd(OwnedFd::from(ours))?;
        let stream = socket.connection_factory_create_connection();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let guid = gio::dbus_generate_guid();
            match gio::DBusConnection::new_future(&stream, Some(&guid), gio::DBusConnectionFlags::AUTHENTICATION_SERVER, None).await {
                Ok(conn) => this.serve(conn),
                Err(e) => glib::g_warning!("salutations", "Cannot talk to the window: {}", e),
            }
        });
        let this = self.clone();
        glib::spawn_future_local(async move {
            let _ = child.wait_future().await;
            this.window_closed();
        });
        Ok(())
    }

    fn serve(self: &Rc<Self>, conn: gio::DBusConnection) {
        let Ok(node) = gio::DBusNodeInfo::for_xml(ipc::XML) else { return };
        let Some(iface) = node.lookup_interface(ipc::IFACE) else { return };
        let weak = Rc::downgrade(self);
        let registered = conn
            .register_object(ipc::PATH, &iface)
            .method_call(move |_, _, _, _, method, params, invocation| {
                let Some(this) = weak.upgrade() else { return };
                if method != "Call" {
                    invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method);
                    return;
                }
                let Some((name, args)) = params.get::<(String, glib::Variant)>() else {
                    invocation.return_dbus_error("org.freedesktop.DBus.Error.InvalidArgs", "Call(sv)");
                    return;
                };
                glib::spawn_future_local(async move {
                    match this.call(&name, args).await {
                        Ok(v) => invocation.return_value(Some(&(v,).to_variant())),
                        Err(e) => invocation.return_dbus_error(&ipc::error_name(&e), e.message()),
                    }
                });
            })
            .build();
        if let Err(e) = registered {
            glib::g_warning!("salutations", "Cannot serve the window: {}", e);
        }
        *self.conn.borrow_mut() = Some(conn);
    }

    fn window_closed(self: &Rc<Self>) {
        log("window process ended");
        self.window.borrow_mut().take();
        self.ready.set(false);
        if let Some(conn) = self.conn.borrow_mut().take() {
            glib::spawn_future_local(async move {
                let _ = conn.close_future().await;
            });
        }
        // Folders only the window used are let go; INBOX stays with the
        // engine.
        for (_, (folder, handler)) in self.folders.borrow_mut().drain() {
            folder.disconnect(handler);
        }
        self.engine.set_open(None);
        *self.shown.borrow_mut() = Shown::default();
        self.pending.borrow_mut().clear();
        if self.quitting.get() || !Settings::get().run_in_background {
            self.quit();
        } else {
            self.update_hold();
            crate::trim_memory();
        }
    }

    pub fn quit(&self) {
        self.quitting.set(true);
        self.update_hold();
        self.app.quit();
    }

    pub fn shutdown(&self) {
        self.notifier.withdraw_all();
    }

    fn account(&self, uid: &str) -> Result<Rc<AccountState>, glib::Error> {
        self.engine.account(uid).ok_or_else(|| glib::Error::new(gio::IOErrorEnum::NotFound, "No such account"))
    }

    /// The account's folder `name`, opened when needed and watched for the
    /// window.
    /// Stops using `path` and its subfolders before they are renamed or
    /// deleted: unwatched (forgotten when deleted) and dropped from the
    /// folders kept open.
    fn let_go(&self, acct: &AccountState, path: &str, forget: bool) {
        let watched: Vec<String> = self.extra(acct.uid()).watches.borrow().keys().filter(|p| salutations_core::within(p, path)).cloned().collect();
        for p in watched {
            self.unwatch(acct, &p, forget);
        }
        let open: Vec<(String, String)> =
            self.folders.borrow().keys().filter(|(a, f)| a == acct.uid() && salutations_core::within(f, path)).cloned().collect();
        for key in open {
            if let Some((f, h)) = self.folders.borrow_mut().remove(&key) {
                f.disconnect(h);
            }
        }
    }

    /// Deletes `path` and its subfolders: first every message goes to
    /// Trash, then the folders, deepest first. A folder in Trash, or on an
    /// account without Trash, is deleted with its messages. Nothing is
    /// deleted unless every move worked. The number of messages moved.
    async fn delete_folder(self: &Rc<Self>, acct: &Rc<AccountState>, path: &str) -> Result<usize, glib::Error> {
        let store = self.engine.store_for(acct)?;
        let nodes = self.engine.session.folders_future(&store, false).await?;
        let mut all = Vec::new();
        fn walk(nodes: &[camel::FolderNode], out: &mut Vec<(String, bool)>) {
            for n in nodes {
                out.push((n.full_name.clone(), n.flags.contains(camel::FolderInfoFlags::NOSELECT)));
                walk(&n.children, out);
            }
        }
        walk(&nodes, &mut all);
        let mut doomed: Vec<(String, bool)> = all.into_iter().filter(|(f, _)| salutations_core::within(f, path)).collect();
        if doomed.is_empty() {
            return Err(glib::Error::new(camel::StoreError::NoFolder, &format!("No folder {path}")));
        }
        let special = acct.special().unwrap_or_default();
        if doomed.iter().any(|(f, _)| f.eq_ignore_ascii_case("INBOX") || special.role_of(f).is_some()) {
            return Err(glib::Error::new(gio::IOErrorEnum::PermissionDenied, "Inbox and special folders can't be deleted"));
        }
        let trash = special.trash.clone().filter(|t| !salutations_core::within(path, t));
        let mut moved = 0;
        if let Some(trash) = trash {
            let read = Settings::get().read_on_delete;
            for (f, noselect) in &doomed {
                if *noselect {
                    continue;
                }
                let folder = self.folder(acct.uid(), f).await?;
                folder.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT).await?;
                let uids: Vec<String> =
                    folder.uids().iter().map(|u| u.to_string()).filter(|u| folder.message_flags(u) & DELETED == 0).collect();
                if uids.is_empty() {
                    continue;
                }
                if read {
                    crate::inbox::set_flags(&folder, &uids, SEEN, SEEN);
                }
                if !self.move_now(acct, &folder, &uids, &trash).await {
                    return Err(glib::Error::new(gio::IOErrorEnum::Failed, &format!("Cannot move the messages in {f} to Trash")));
                }
                moved += uids.len();
            }
        }
        self.let_go(acct, path, true);
        doomed.sort_by_key(|(f, _)| std::cmp::Reverse(f.matches('/').count()));
        for (f, _) in doomed {
            let job = Handoff(store.clone());
            let name = f.clone();
            gio::spawn_blocking(move || {
                let Handoff(s) = job;
                s.delete_folder_sync(&name, None::<&gio::Cancellable>)
            })
            .await
            .map_err(|_| glib::Error::new(gio::IOErrorEnum::Failed, "Cannot delete the folder"))??;
            log(&format!("{}: deleted {f}", acct.uid()));
        }
        Ok(moved)
    }

    pub(crate) async fn folder(self: &Rc<Self>, account: &str, name: &str) -> Result<camel::Folder, glib::Error> {
        let key = (account.to_string(), name.to_string());
        if let Some((f, _)) = self.folders.borrow().get(&key) {
            return Ok(f.clone());
        }
        let acct = self.account(account)?;
        let folder = match acct.inbox().filter(|_| name.eq_ignore_ascii_case("INBOX")) {
            Some(f) => f,
            None => {
                let store = self.engine.store_for(&acct)?;
                self.engine.session.folder_future(&store, name).await?
            }
        };
        if let Some((f, _)) = self.folders.borrow().get(&key) {
            return Ok(f.clone());
        }
        let weak = glib::thread_guard::ThreadGuard::new(Rc::downgrade(self));
        let (acc, nm) = (account.to_string(), name.to_string());
        let handler = folder.connect_changed(move |folder, changes| {
            let Some(this) = weak.get_ref().upgrade() else { return };
            let info = |u: &String| folder.message_info(u).and_then(|i| Info::from_camel(&i));
            let c = Changes {
                account: acc.clone(),
                folder: nm.clone(),
                added: changes.added.iter().filter_map(info).collect(),
                changed: changes.changed.iter().filter_map(info).collect(),
                removed: changes.removed.clone(),
            };
            this.emit("changes", c.to_variant());
        });
        self.folders.borrow_mut().insert(key, (folder.clone(), handler));
        Ok(folder)
    }

    async fn call(self: &Rc<Self>, name: &str, args: glib::Variant) -> Result<glib::Variant, glib::Error> {
        let bad = || glib::Error::new(gio::IOErrorEnum::InvalidArgument, &format!("Bad arguments for {name}"));
        macro_rules! args {
            ($t:ty) => {
                args.get::<$t>().ok_or_else(bad)?
            };
        }
        let unit = ipc::unit();
        match name {
            "hello" => {
                let accounts: Vec<AccountInfo> = self.engine.accounts().iter().map(|a| self.account_info(a)).collect();
                self.ready.set(true);
                let this = self.clone();
                glib::idle_add_local_once(move || {
                    let pending: Vec<_> = this.pending.borrow_mut().drain(..).collect();
                    for (n, d) in pending {
                        this.emit(&n, d);
                    }
                    for a in this.engine.accounts() {
                        let this = this.clone();
                        glib::spawn_future_local(async move {
                            if let Some(store) = a.store()
                                && let Ok(nodes) = this.engine.session.folders_future(&store, false).await
                            {
                                let special = a.special().unwrap_or_default();
                                let nodes: Vec<Node> = ipc::flatten_nodes(&engine::sort_folders(&nodes, &special));
                                this.emit("folders", (a.uid().to_string(), nodes, true).to_variant());
                            }
                        });
                    }
                });
                Ok(accounts.to_variant())
            }
            "sync" => {
                let a = self.account(&args!(String))?;
                self.engine.sync(&a);
                Ok(unit)
            }
            "cancel_sync" => {
                let a = self.account(&args!(String))?;
                self.engine.cancel_sync(&a);
                Ok(unit)
            }
            "resync" => {
                let (uid, drop) = args!((String, bool));
                self.engine.resync(&self.account(&uid)?, drop);
                Ok(unit)
            }
            "account_changed" => {
                let (uid, kind) = args!((String, String));
                Settings::reload();
                if kind == "order" {
                    self.engine.load_accounts();
                    return Ok(unit);
                }
                let a = self.account(&uid)?;
                match kind.as_str() {
                    "offline" => {
                        self.engine.apply_conditions(&a);
                        self.engine.maintain(&a, true, None);
                    }
                    "refresh" => self.engine.schedule_refresh(&a),
                    "push" => {
                        self.engine.schedule_keepalive(&a);
                        if !self.engine.paused(&a) {
                            self.engine.reconnect(&a);
                        }
                    }
                    _ => {
                        self.engine.schedule_refresh(&a);
                        self.engine.apply_conditions(&a);
                        self.engine.maintain(&a, true, None);
                    }
                }
                Ok(unit)
            }
            "settings_changed" => {
                Settings::reload();
                self.update_hold();
                Ok(unit)
            }
            "folders" => {
                let a = self.account(&args!(String))?;
                self.engine.refresh_folders(&a).await;
                Ok(unit)
            }
            "set_special" => {
                let (uid, special) = args!((String, Special));
                let a = self.account(&uid)?;
                if let Some(s) = special.to_folders() {
                    self.engine.set_special(&a, s);
                }
                self.emit("account", self.account_info(&a).to_variant());
                Ok(unit)
            }
            "open_folder" => {
                let (uid, name) = args!((String, String));
                let f = self.folder(&uid, &name).await?;
                let infos: Vec<Info> =
                    f.uids().iter().filter_map(|u| f.message_info(u)).filter_map(|i| Info::from_camel(&i)).collect();
                let display = f.display_name().map(|s| s.to_string()).unwrap_or_else(|| name.clone());
                Ok((display, infos).to_variant())
            }
            "set_open" => {
                let (uid, name) = args!((String, String));
                let open = if name.is_empty() { None } else { Some(self.folder(&uid, &name).await?) };
                self.engine.set_open(open);
                Ok(unit)
            }
            "shown" => {
                let (active, account, folder) = args!((bool, String, String));
                *self.shown.borrow_mut() = Shown { active, account, folder };
                Ok(unit)
            }
            "set_flags" => {
                let (uid, name, list) = args!((String, String, Vec<(String, u32, u32)>));
                let f = self.folder(&uid, &name).await?;
                f.freeze();
                for (u, mask, set) in &list {
                    f.set_message_flags(u, *mask, *set);
                }
                f.thaw();
                Ok(unit)
            }
            "set_user_flag" => {
                let (uid, name, u, flag, on) = args!((String, String, String, String, bool));
                let f = self.folder(&uid, &name).await?;
                if let Some(info) = f.message_info(&u) {
                    camel::message_info_set_user_flag(&info, &flag, on);
                }
                Ok(unit)
            }
            "refresh" => {
                let (uid, name) = args!((String, String));
                let f = self.folder(&uid, &name).await?;
                f.refresh_info_future(glib::ffi::G_PRIORITY_DEFAULT).await?;
                Ok(unit)
            }
            "synchronize" => {
                let (uid, name, expunge) = args!((String, String, bool));
                let f = self.folder(&uid, &name).await?;
                f.synchronize_future(expunge, glib::ffi::G_PRIORITY_DEFAULT).await?;
                Ok(unit)
            }
            "search" => {
                let (uid, name, expr) = args!((String, String, String));
                let f = self.folder(&uid, &name).await?;
                let job = Handoff(f);
                let found = gio::spawn_blocking(move || {
                    let Handoff(f) = job;
                    f.search_sync(&expr, None::<&gio::Cancellable>).map(|v| v.into_iter().map(|u| u.to_string()).collect::<Vec<String>>())
                })
                .await
                .map_err(|_| glib::Error::new(gio::IOErrorEnum::Failed, "The search stopped unexpectedly"))??;
                Ok(found.to_variant())
            }
            "threads" => {
                let (uid, name) = args!((String, String));
                let f = self.folder(&uid, &name).await?;
                fn conv(n: &camel::ThreadNode) -> Option<Thread> {
                    Some(Thread { uid: n.info.uid()?.to_string(), children: n.children.iter().filter_map(conv).collect() })
                }
                let threads: Vec<Thread> = camel::thread_folder(&f).iter().filter_map(conv).collect();
                Ok(ipc::flatten_threads(&threads).to_variant())
            }
            "message" => {
                let (uid, name, u) = args!((String, String, String));
                let f = self.folder(&uid, &name).await?;
                match f.message_future(&u, glib::ffi::G_PRIORITY_DEFAULT).await? {
                    Some(msg) => Ok((true, glib::Variant::array_from_fixed_array(&mime_bytes(&msg)?)).to_variant()),
                    None => Ok((false, glib::Variant::array_from_fixed_array::<u8>(&[])).to_variant()),
                }
            }
            "append" => {
                let (uid, name, data, flags) = args!((String, String, glib::Variant, u32));
                let f = self.folder(&uid, &name).await?;
                let msg = parse_message(data.fixed_array::<u8>().map_err(|_| bad())?)?;
                if let Some(k) = key_of(&msg) {
                    self.expect(&uid, &name, [k]);
                }
                let new = send::append(&f, &msg, flags).await?;
                Ok(new.unwrap_or_default().to_variant())
            }
            "transfer" => {
                let (uid, name, uids, dest_account, dest, delete) = args!((String, String, Vec<String>, String, String, bool));
                let src = self.folder(&uid, &name).await?;
                let to = self.folder(&dest_account, &dest).await?;
                self.expect(&dest_account, &dest, uids.iter().filter_map(|u| message_key(&src, u)));
                let job = Handoff((src, to));
                let new = gio::spawn_blocking(move || {
                    let Handoff((src, to)) = job;
                    src.transfer_messages_to_sync(&uids, &to, delete, None::<&gio::Cancellable>)
                })
                .await
                .map_err(|_| glib::Error::new(gio::IOErrorEnum::Failed, "Transfer failed"))??;
                let new: Vec<String> = new.into_iter().map(|u| u.unwrap_or_default()).collect();
                Ok(new.to_variant())
            }
            "create_folder" => {
                let (uid, parent, folder) = args!((String, String, String));
                let a = self.account(&uid)?;
                let store = self.engine.store_for(&a)?;
                let job = Handoff(store);
                gio::spawn_blocking(move || {
                    let Handoff(s) = job;
                    let parent = (!parent.is_empty()).then_some(parent);
                    s.create_folder_sync(parent.as_deref(), &folder, None::<&gio::Cancellable>).map(|_| ())
                })
                .await
                .map_err(|_| glib::Error::new(gio::IOErrorEnum::Failed, "Cannot create the folder"))??;
                Ok(unit)
            }
            "rename_folder" => {
                let (uid, old, new) = args!((String, String, String));
                let a = self.account(&uid)?;
                let store = self.engine.store_for(&a)?;
                self.let_go(&a, &old, false);
                // IMAPX's rename drops its own reference to the folder and
                // then uses it, so one is held here until it is done.
                let held = self.engine.session.folder_future(&store, &old).await?;
                let job = Handoff((store, held));
                let (o, n) = (old.clone(), new.clone());
                let renamed = gio::spawn_blocking(move || {
                    let Handoff((s, _held)) = job;
                    s.rename_folder_sync(&o, &n, None::<&gio::Cancellable>)
                })
                .await
                .map_err(|_| glib::Error::new(gio::IOErrorEnum::Failed, "Cannot rename the folder"))?;
                if renamed.is_ok() {
                    crate::inbox::rename_state(&uid, &old, &new);
                    if let Some(mut special) = a.special() {
                        let before = special.clone();
                        special.renamed(&old, &new);
                        if special != before {
                            self.engine.set_special(&a, special);
                        }
                    }
                }
                self.engine.refresh_folders(&a).await;
                self.update_watches(&a);
                renamed?;
                Ok(unit)
            }
            "delete_folder" => {
                let (uid, path) = args!((String, String));
                let a = self.account(&uid)?;
                let moved = self.delete_folder(&a, &path).await;
                self.engine.refresh_folders(&a).await;
                self.update_watches(&a);
                Ok((moved? as u32).to_variant())
            }
            "cached_header" => {
                let (uid, name, u, header) = args!((String, String, String, String));
                let f = self.folder(&uid, &name).await?;
                Ok(cached_header(&f, &u, &header).unwrap_or_default().to_variant())
            }
            "folder_synced" => {
                let (uid, name) = args!((String, String));
                let f = self.folder(&uid, &name).await?;
                Ok(salutations_core::offline::folder_synced(&f).to_variant())
            }
            "set_folder_synced" => {
                let (uid, name, on) = args!((String, String, bool));
                let f = self.folder(&uid, &name).await?;
                salutations_core::offline::set_folder_synced(&f, on);
                Ok(unit)
            }
            "transmit" => {
                let (uid, data, from, to) = args!((String, glib::Variant, String, String));
                let a = self.account(&uid)?;
                let msg = parse_message(data.fixed_array::<u8>().map_err(|_| bad())?)?;
                let (f, r) = (camel::InternetAddress::new(), camel::InternetAddress::new());
                f.decode(&from);
                r.decode(&to);
                send::transmit(&self.engine.session, &a.account, &msg, f, r).await?;
                Ok(unit)
            }
            "apply_rule" => {
                let (uid, text) = args!((String, String));
                let rule = salutations_core::rules::from_text(&text).ok_or_else(bad)?;
                let a = self.account(&uid)?;
                let matched = self.apply_rule_now(&a, rule).await?;
                Ok((matched as u32).to_variant())
            }
            "outbox" => {
                let uid = args!(String);
                let sending = self.extra(&uid).sending_uid.borrow().clone().unwrap_or_default();
                Ok(sending.to_variant())
            }
            "quit" => {
                self.quitting.set(true);
                Ok(unit)
            }
            _ => Err(glib::Error::new(gio::IOErrorEnum::NotSupported, &format!("Unknown call {name}"))),
        }
    }
}

/// `msg` as written to a file.
pub(crate) fn mime_bytes(msg: &camel::MimeMessage) -> Result<Vec<u8>, glib::Error> {
    let out = gio::MemoryOutputStream::new_resizable();
    msg.write_to_output_stream_sync(&out, None::<&gio::Cancellable>)?;
    out.close(None::<&gio::Cancellable>)?;
    Ok(out.steal_as_bytes().to_vec())
}

pub(crate) fn parse_message(bytes: &[u8]) -> Result<camel::MimeMessage, glib::Error> {
    let msg = camel::MimeMessage::new();
    let input = gio::MemoryInputStream::from_bytes(&glib::Bytes::from(bytes));
    msg.construct_from_input_stream_sync(&input, None::<&gio::Cancellable>)?;
    Ok(msg)
}

fn key_of(msg: &camel::MimeMessage) -> Option<MessageKey> {
    Some((
        msg.subject().map(|s| s.to_string()).unwrap_or_default(),
        msg.date().0,
        msg.from().map(|a| a.format().map(|s| s.to_string()).unwrap_or_default()).unwrap_or_default(),
    ))
}

/// The first `name` header of message `uid` from the offline copy only.
fn cached_header(folder: &camel::Folder, uid: &str, name: &str) -> Option<String> {
    use glib::translate::*;
    let msg: Option<camel::MimeMessage> = unsafe {
        from_glib_full(camel::ffi::camel_folder_get_message_cached(folder.to_glib_none().0, uid.to_glib_none().0, std::ptr::null_mut()))
    };
    msg?.header(name).map(|h| h.to_string())
}
