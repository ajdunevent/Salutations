use std::path::Path;

use camel::{prelude::*, subclass::prelude::*};
use edataserver::prelude::*;
use edataserver::{SourceCamel, SourceCredentialsProvider, SourceRegistry};
use glib::subclass::types::ObjectSubclassIsExt;

pub(crate) mod imp {
    use std::sync::OnceLock;

    use super::*;

    #[derive(Default)]
    pub struct Session {
        pub registry: OnceLock<SourceRegistry>,
        pub credentials: OnceLock<SourceCredentialsProvider>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Session {
        const NAME: &'static str = "SalutationsSession";
        type Type = super::Session;
        type ParentType = camel::Session;
    }

    impl ObjectImpl for Session {}

    impl SessionImpl for Session {
        fn add_service(
            &self,
            uid: &str,
            protocol: &str,
            type_: camel::ProviderType,
        ) -> Result<camel::Service, glib::Error> {
            let service = self.parent_add_service(uid, protocol, type_)?;
            let registry = self.registry();
            if let Some(source) = registry.ref_source(uid) {
                // Provider settings live on the source carrying the
                // protocol's Camel extension, which can be an ancestor.
                let settings_source = SourceCamel::extension_name(protocol)
                    .and_then(|ext| registry.find_extension(&source, &ext))
                    .unwrap_or(source);
                settings_source.camel_configure_service(&service);
                settings_source
                    .bind_property("display-name", &service, "display-name")
                    .sync_create()
                    .build();
            }
            Ok(service)
        }

        fn authenticate_sync(
            &self,
            service: &camel::Service,
            mechanism: Option<&str>,
            cancellable: Option<&gio::Cancellable>,
        ) -> Result<(), glib::Error> {
            crate::auth::authenticate_sync(self, service, mechanism, cancellable)
        }

        fn trust_prompt(
            &self,
            service: &camel::Service,
            _certificate: &gio::TlsCertificate,
            errors: gio::TlsCertificateFlags,
        ) -> camel::ffi::CamelCertTrust {
            crate::trust::decide(self.registry(), service, errors)
        }

        fn oauth2_access_token_sync(
            &self,
            service: &camel::Service,
            cancellable: Option<&gio::Cancellable>,
        ) -> Result<(glib::GString, i32), glib::Error> {
            crate::auth::oauth2_access_token_sync(self, service, cancellable)
        }
    }

    impl Session {
        pub fn registry(&self) -> &SourceRegistry {
            self.registry.get().expect("registry is set in Session::new")
        }

        pub fn credentials(&self) -> &SourceCredentialsProvider {
            self.credentials
                .get()
                .expect("credentials provider is set in Session::new")
        }
    }
}

glib::wrapper! {
    /// The one `CamelSession` of the process. It owns every account's
    /// store and transport and resolves their settings and credentials
    /// through the EDS source registry.
    pub struct Session(ObjectSubclass<imp::Session>) @extends camel::Session;
}

// Camel locks its session internally and calls it from worker threads.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

impl Session {
    pub fn new(registry: &SourceRegistry, data_dir: &Path, cache_dir: &Path) -> Self {
        let session: Self = glib::Object::builder()
            .property("user-data-dir", data_dir.to_string_lossy().as_ref())
            .property("user-cache-dir", cache_dir.to_string_lossy().as_ref())
            .build();
        let imp = session.imp();
        imp.registry
            .set(registry.clone())
            .expect("new session has no registry");
        imp.credentials
            .set(SourceCredentialsProvider::new(registry))
            .expect("new session has no credentials provider");
        crate::downsync::install(&session);
        session
    }

    pub fn registry(&self) -> &SourceRegistry {
        self.imp().registry()
    }

    /// Returns the IMAP store for `account`, creating it on first use.
    pub fn store(&self, account: &crate::Account) -> Result<camel::Store, glib::Error> {
        let session = self.upcast_ref::<camel::Session>();
        let service = match session.ref_service(&account.uid) {
            Some(service) => service,
            None => session.add_service(
                &account.uid,
                &account.backend_name,
                camel::ProviderType::Store,
            )?,
        };
        service.downcast::<camel::Store>().map_err(|_| {
            glib::Error::new(
                camel::ServiceError::Invalid,
                &format!("{} is not a mail store", account.uid),
            )
        })
    }

    /// Connects `store`. Blocks; call from a worker thread.
    pub fn connect(
        &self,
        store: &camel::Store,
        cancellable: Option<&gio::Cancellable>,
    ) -> Result<(), glib::Error> {
        // An IMAPX store can start offline while the session is online;
        // connect_sync() on it then fails with "You must be working online".
        match store.dynamic_cast_ref::<camel::OfflineStore>() {
            Some(offline) => offline.set_online_sync(true, cancellable),
            None => store.connect_sync(cancellable),
        }
    }

    /// Opens INBOX of a connected store whose folder tree has been
    /// fetched. Blocks; call from a worker thread.
    pub fn inbox(
        &self,
        store: &camel::Store,
        cancellable: Option<&gio::Cancellable>,
    ) -> Result<camel::Folder, glib::Error> {
        store
            .folder_sync("INBOX", camel::StoreGetFolderFlags::empty(), cancellable)?
            .ok_or_else(|| glib::Error::new(camel::StoreError::NoFolder, "Account has no INBOX"))
    }

    /// Fetches the folder tree of a connected store. Blocks; call from a
    /// worker thread. Folders such as INBOX cannot be opened on a new
    /// account until this has run once.
    pub fn folders(
        &self,
        store: &camel::Store,
        cancellable: Option<&gio::Cancellable>,
    ) -> Result<Vec<camel::FolderNode>, glib::Error> {
        let flags = camel::StoreGetFolderInfoFlags::RECURSIVE
            | camel::StoreGetFolderInfoFlags::SUBSCRIBED;
        Ok(store
            .folder_info_sync(None, flags, cancellable)?
            .map(|fi| fi.to_nodes())
            .unwrap_or_default())
    }

    /// Connects `store` without the offline store's "go online" step,
    /// which opens every folder for offline sync and, before the folder
    /// list is known, makes IMAPX send a LIST per folder.
    ///
    /// An offline store counts as offline until Camel has checked that its
    /// host is reachable, and IMAPX then refuses to connect; the check is
    /// made first. A store left offline (created while the session was
    /// offline) still goes through set_online().
    pub async fn connect_future(&self, store: &camel::Store) -> Result<(), glib::Error> {
        if store.connection_status() == camel::ServiceConnectionStatus::Connected {
            return Ok(());
        }
        let Some(offline) = store.dynamic_cast_ref::<camel::OfflineStore>() else {
            return store.connect_future(PRIORITY).await;
        };
        if store.is::<camel::NetworkService>() {
            let s = store.clone();
            // The answer is also stored on the service; only that matters.
            let _ = gio::spawn_blocking(move || {
                use glib::translate::ToGlibPtr;
                let Some(net) = s.dynamic_cast_ref::<camel::NetworkService>() else { return };
                let mut error = std::ptr::null_mut();
                unsafe {
                    camel::ffi::camel_network_service_can_reach_sync(
                        net.to_glib_none().0,
                        std::ptr::null_mut(),
                        &mut error,
                    );
                    if !error.is_null() {
                        glib::ffi::g_error_free(error);
                    }
                }
            })
            .await;
        }
        if offline.is_online() {
            store.connect_future(PRIORITY).await
        } else {
            offline.set_online_future(true, PRIORITY).await
        }
    }

    /// Async form of [`Session::folders`]. Without `refresh` the cached tree
    /// is returned with no server traffic. With it, IMAPX lists every folder
    /// with its counts; it does that on any online call without FAST, so
    /// REFRESH alone would not avoid the round trip.
    pub async fn folders_future(
        &self,
        store: &camel::Store,
        refresh: bool,
    ) -> Result<Vec<camel::FolderNode>, glib::Error> {
        // NO_VIRTUAL: Camel otherwise adds its own virtual Trash and Junk
        // beside the server's.
        let mut flags = camel::StoreGetFolderInfoFlags::RECURSIVE
            | camel::StoreGetFolderInfoFlags::SUBSCRIBED
            | camel::StoreGetFolderInfoFlags::NO_VIRTUAL;
        flags |= if refresh {
            camel::StoreGetFolderInfoFlags::REFRESH
        } else {
            camel::StoreGetFolderInfoFlags::FAST
        };
        Ok(store
            .folder_info_future(None, flags, PRIORITY)
            .await?
            .map(|fi| fi.to_nodes())
            .unwrap_or_default())
    }

    /// Which folders the server marks as Sent, Drafts, Archive, Junk and
    /// Trash (RFC 6154 SPECIAL-USE, else well-known names). Camel's folder
    /// info carries no type for these on IMAP, only for INBOX.
    pub async fn special_folders(&self, store: &camel::Store) -> Result<SpecialFolders, glib::Error> {
        let store = store.clone();
        let setup = gio::spawn_blocking(move || store.initial_setup_sync(None::<&gio::Cancellable>))
            .await
            .map_err(|_| glib::Error::new(gio::IOErrorEnum::Failed, "Folder setup thread panicked"))??;
        let get = |key: &str| setup.get(key).map(|v| folder_path(v));
        Ok(SpecialFolders {
            sent: get("Submission:Mail Submission:sent-folder:f"),
            drafts: get("Submission:Mail Composition:drafts-folder:f"),
            archive: get("Account:Mail Account:archive-folder:f"),
            junk: get("Backend:Imapx Backend:real-junk-path:s"),
            trash: get("Backend:Imapx Backend:real-trash-path:s"),
            archive_chosen: false,
        })
    }

    /// Opens a folder by full name. INBOX must be opened through this,
    /// never `camel_store_get_inbox_folder_sync()` (see DEVELOPMENT.md).
    pub async fn folder_future(
        &self,
        store: &camel::Store,
        full_name: &str,
    ) -> Result<camel::Folder, glib::Error> {
        store
            .folder_future(full_name, camel::StoreGetFolderFlags::empty(), PRIORITY)
            .await?
            .ok_or_else(|| {
                glib::Error::new(camel::StoreError::NoFolder, &format!("No folder {full_name}"))
            })
    }
}

const PRIORITY: i32 = glib::ffi::G_PRIORITY_DEFAULT;

/// Full names of the account's special-use folders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpecialFolders {
    pub sent: Option<String>,
    pub drafts: Option<String>,
    pub archive: Option<String>,
    pub junk: Option<String>,
    pub trash: Option<String>,
    /// The user chose `archive`; the server's answer does not replace it.
    pub archive_chosen: bool,
}

fn special_path() -> std::path::PathBuf {
    crate::data_dir().join("special-folders.ini")
}

impl SpecialFolders {
    const KEYS: [&'static str; 5] = ["sent", "drafts", "archive", "junk", "trash"];

    fn fields(&mut self) -> [&mut Option<String>; 5] {
        [&mut self.sent, &mut self.drafts, &mut self.archive, &mut self.junk, &mut self.trash]
    }

    /// The server's answer, keeping an Archive folder the user chose.
    pub fn merged(mut self, saved: Option<&SpecialFolders>) -> SpecialFolders {
        if let Some(saved) = saved.filter(|s| s.archive_chosen) {
            self.archive = saved.archive.clone();
            self.archive_chosen = true;
        }
        self
    }

    /// The folders saved for `account_uid` by an earlier session, so the
    /// tree can be ordered and given icons before the server answers.
    pub fn load(account_uid: &str) -> Option<SpecialFolders> {
        let kf = glib::KeyFile::new();
        kf.load_from_file(special_path(), glib::KeyFileFlags::NONE).ok()?;
        if !kf.has_group(account_uid) {
            return None;
        }
        let mut s = SpecialFolders::default();
        for (key, field) in Self::KEYS.iter().zip(s.fields()) {
            *field = kf.string(account_uid, key).ok().map(|v| v.to_string()).filter(|v| !v.is_empty());
        }
        s.archive_chosen = kf.boolean(account_uid, "archive-chosen").unwrap_or(false);
        Some(s)
    }

    pub fn save(&self, account_uid: &str) -> Result<(), glib::Error> {
        let kf = glib::KeyFile::new();
        let path = special_path();
        let _ = kf.load_from_file(&path, glib::KeyFileFlags::KEEP_COMMENTS);
        let mut s = self.clone();
        for (key, field) in Self::KEYS.iter().zip(s.fields()) {
            kf.set_string(account_uid, key, field.as_deref().unwrap_or(""));
        }
        kf.set_boolean(account_uid, "archive-chosen", self.archive_chosen);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        kf.save_to_file(&path)
    }
}

/// Setup values of type `f` are `folder://ACCOUNT-UID/escaped/path`; type
/// `s` values are plain paths.
fn folder_path(value: &str) -> String {
    let Some(rest) = value.strip_prefix("folder://") else {
        return value.to_string();
    };
    let path = rest.split_once('/').map_or("", |(_, p)| p);
    glib::uri_unescape_string(path, None::<&str>)
        .map(|s| s.to_string())
        .unwrap_or_else(|| path.to_string())
}
