use edataserver::prelude::*;
use edataserver::{
    E_SOURCE_EXTENSION_GOA, E_SOURCE_EXTENSION_MAIL_ACCOUNT, E_SOURCE_EXTENSION_MAIL_IDENTITY,
    Source, SourceMailAccount, SourceMailIdentity, SourceRegistry,
};

/// An IMAP account from GNOME Online Accounts.
#[derive(Debug, Clone)]
pub struct Account {
    pub uid: String,
    /// Camel provider protocol; "imapx" for every listed account.
    pub backend_name: String,
    /// The account's own address, from its mail identity.
    pub address: Option<String>,
    pub source: Source,
    /// The mail identity source: name, address, aliases and the SMTP
    /// transport.
    pub identity: Option<Source>,
}

impl Account {
    /// The name shown for the account. Read from the source each time:
    /// the registry updates its source objects in place, and GOA renames
    /// reach the mail account through the collection.
    pub fn display_name(&self) -> String {
        self.source.display_name().map(|n| n.to_string()).unwrap_or_default()
    }
}

const IMAP_BACKEND: &str = "imapx";

impl Account {
    fn identity_ext(&self) -> Option<SourceMailIdentity> {
        self.identity
            .as_ref()?
            .extension::<SourceMailIdentity>(E_SOURCE_EXTENSION_MAIL_IDENTITY)
    }

    /// The name and address mail is sent from by default.
    pub fn sender(&self) -> crate::Address {
        let id = self.identity_ext();
        crate::Address {
            name: id.as_ref().and_then(|i| i.name()).map(|n| n.to_string()).filter(|n| !n.trim().is_empty()),
            email: id
                .as_ref()
                .and_then(|i| i.address())
                .map(|a| a.to_string())
                .or_else(|| self.address.clone())
                .unwrap_or_default(),
        }
    }

    /// Other From addresses the user set for the account (the identity's
    /// aliases).
    pub fn aliases(&self) -> Vec<crate::Address> {
        let raw = self.identity_ext().and_then(|i| i.aliases()).map(|a| a.to_string()).unwrap_or_default();
        crate::compose::parse_addresses(&raw).unwrap_or_default()
    }

    /// Replaces the aliases; the registry writes the source to disk.
    pub fn set_aliases(&self, aliases: &[crate::Address]) {
        if let Some(id) = self.identity_ext() {
            let text = crate::compose::format_addresses(aliases);
            id.set_aliases((!text.is_empty()).then_some(text.as_str()));
            if let Some(source) = &self.identity
                && let Err(e) = crate::offline::write_source(source)
            {
                glib::g_warning!("salutations", "Cannot save aliases: {}", e);
            }
        }
    }

    /// The account's address and its aliases, lowercased.
    pub fn own_addresses(&self) -> Vec<String> {
        let mut own: Vec<String> = self.address.iter().cloned().collect();
        own.push(self.sender().email.to_ascii_lowercase());
        own.extend(self.aliases().into_iter().map(|a| a.email.to_ascii_lowercase()));
        own.sort();
        own.dedup();
        own.retain(|a| !a.is_empty());
        own
    }

    /// The UID of the SMTP transport source, from the identity's Mail
    /// Submission extension.
    pub fn transport_uid(&self) -> Option<String> {
        let ext = self
            .identity
            .as_ref()?
            .extension::<edataserver::SourceExtension>(edataserver::E_SOURCE_EXTENSION_MAIL_SUBMISSION)?;
        ext.property::<Option<String>>("transport-uid").filter(|u| !u.is_empty())
    }
}

impl Account {
    fn from_source(registry: &SourceRegistry, source: Source) -> Option<Self> {
        let ext = source.extension::<SourceMailAccount>(E_SOURCE_EXTENSION_MAIL_ACCOUNT)?;
        let backend_name = ext.backend_name()?;
        if backend_name != IMAP_BACKEND {
            return None;
        }
        // Only GOA accounts are listed; their collection source carries
        // the GOA extension. SALUTATIONS_ANY_ACCOUNT=1 lifts this for
        // testing against hand-written EDS sources.
        if registry.find_extension(&source, E_SOURCE_EXTENSION_GOA).is_none()
            && std::env::var_os("SALUTATIONS_ANY_ACCOUNT").is_none()
        {
            return None;
        }
        let identity = ext.identity_uid().and_then(|uid| registry.ref_source(&uid));
        let address = identity
            .as_ref()
            .and_then(|id| {
                id.extension::<SourceMailIdentity>(E_SOURCE_EXTENSION_MAIL_IDENTITY)
            })
            .and_then(|id| id.address())
            .map(|a| a.to_ascii_lowercase());
        Some(Self {
            uid: source.uid()?.into(),
            backend_name: backend_name.into(),
            address,
            source,
            identity,
        })
    }
}

/// Enabled GOA mail accounts. GOA disables an account's mail source when
/// its Mail switch is off, so those are excluded.
pub fn mail_accounts(registry: &SourceRegistry) -> Vec<Account> {
    let mut accounts: Vec<_> = registry
        .list_enabled(Some(E_SOURCE_EXTENSION_MAIL_ACCOUNT))
        .into_iter()
        .filter_map(|source| Account::from_source(registry, source))
        .collect();
    accounts.sort_by_key(|a| a.display_name().to_lowercase());
    accounts
}

/// Calls `f` whenever a mail account source is added, removed, enabled,
/// disabled or changed. Signals arrive on the registry's main context.
pub fn watch_mail_accounts<F: Fn(&SourceRegistry) + Send + Sync + 'static>(
    registry: &SourceRegistry,
    f: F,
) -> Vec<glib::SignalHandlerId> {
    let f = std::sync::Arc::new(f);
    let filter = move |f: std::sync::Arc<F>| {
        move |registry: &SourceRegistry, source: &Source| {
            if source.has_extension(E_SOURCE_EXTENSION_MAIL_ACCOUNT) {
                f(registry);
            }
        }
    };
    vec![
        registry.connect_source_added(filter(f.clone())),
        registry.connect_source_removed(filter(f.clone())),
        registry.connect_source_enabled(filter(f.clone())),
        registry.connect_source_disabled(filter(f.clone())),
        registry.connect_source_changed(filter(f)),
    ]
}
