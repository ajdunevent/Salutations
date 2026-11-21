//! Who a sender is, for notifications and rules: an index of every
//! address-book contact with an address, kept current by live views of the
//! books, and the sender's picture from the same sources the window uses
//! (address book, OpenPGP photo ID, Face header seen before, then BIMI,
//! Libravatar and Gravatar as turned on). The window's disk cache of online
//! pictures is shared.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use salutations_core::avatars;
use salutations_core::contacts::{self, Card, Contacts};
use salutations_core::debug::log;
use salutations_core::{Settings, links};

use crate::images::{self, Picture};

const KEEP_FOUND: Duration = Duration::from_secs(7 * 24 * 3600);
const KEEP_MISSING: Duration = Duration::from_secs(24 * 3600);
/// How long a notification waits for an online picture before going out
/// without one.
const NOTE_WAIT: Duration = Duration::from_secs(5);

type Key = (String, String);

#[derive(Default)]
pub struct People {
    contacts: Option<Contacts>,
    ready: Cell<bool>,
    pending: Cell<usize>,
    cards: RefCell<HashMap<Key, Card>>,
    by_email: RefCell<HashMap<String, Key>>,
    views: RefCell<Vec<contacts::View>>,
    /// Addresses whose keys carry a photo ID; None until gpg has listed
    /// them.
    pgp: RefCell<Option<HashSet<String>>>,
}

impl People {
    pub fn start(contacts: Contacts) -> Rc<People> {
        let this = Rc::new(People { contacts: Some(contacts.clone()), ..Default::default() });
        let idx = this.clone();
        glib::spawn_future_local(async move {
            let views = gio::spawn_blocking(move || contacts.views_sync()).await.unwrap_or_default();
            log(&format!("people: {} address book views", views.len()));
            idx.pending.set(views.len());
            if views.is_empty() {
                idx.ready.set(true);
            }
            for contacts::SendView(view) in views {
                idx.connect(&view);
                contacts::start(&view);
                idx.views.borrow_mut().push(view);
            }
        });
        let idx = this.clone();
        glib::spawn_future_local(async move {
            let listed = gpg(&["--with-colons", "--list-keys"]).await;
            let found = listed.map(|out| avatars::photo_addresses(&String::from_utf8_lossy(&out))).unwrap_or_default();
            *idx.pgp.borrow_mut() = Some(found);
        });
        this
    }

    fn connect(self: &Rc<Self>, view: &contacts::View) {
        use glib::prelude::*;
        for (signal, removed) in [("objects-added", false), ("objects-modified", false), ("objects-removed", true)] {
            let (weak, book) = (Rc::downgrade(self), view.book.clone());
            view.view.connect_local(signal, false, move |args| {
                let this = weak.upgrade()?;
                let list = args.get(1)?.get::<glib::Pointer>().ok()?;
                if removed {
                    let uids = unsafe { contacts::uids_from(list) };
                    this.remove(&book, &uids);
                } else {
                    let cards = unsafe { contacts::cards_from(&book, list) };
                    this.put(cards);
                }
                None
            });
        }
        let weak = Rc::downgrade(self);
        view.view.connect_local("complete", false, move |_| {
            let this = weak.upgrade()?;
            let left = this.pending.get().saturating_sub(1);
            this.pending.set(left);
            if left == 0 {
                this.ready.set(true);
            }
            None
        });
    }

    fn put(&self, cards: Vec<Card>) {
        let mut all = self.cards.borrow_mut();
        let mut by_email = self.by_email.borrow_mut();
        for card in cards {
            let key = (card.book.clone(), card.uid.clone());
            if let Some(old) = all.remove(&key) {
                for e in &old.emails {
                    by_email.remove(&e.trim().to_lowercase());
                }
            }
            for e in &card.emails {
                by_email.insert(e.trim().to_lowercase(), key.clone());
            }
            all.insert(key, card);
        }
    }

    fn remove(&self, book: &str, uids: &[String]) {
        let mut all = self.cards.borrow_mut();
        let mut by_email = self.by_email.borrow_mut();
        for uid in uids {
            let key = (book.to_string(), uid.clone());
            if let Some(old) = all.remove(&key) {
                for e in &old.emails {
                    let e = e.trim().to_lowercase();
                    if by_email.get(&e) == Some(&key) {
                        by_email.remove(&e);
                    }
                }
            }
        }
    }

    fn card(&self, email: &str) -> Option<Card> {
        let key = self.by_email.borrow().get(&email.trim().to_lowercase()).cloned()?;
        self.cards.borrow().get(&key).cloned()
    }

    /// Whether an address book has `email`; None until the books are listed.
    pub fn known(&self, email: &str) -> Option<bool> {
        if self.card(email).is_some() {
            return Some(true);
        }
        self.ready.get().then_some(false)
    }

    /// The contact's name and picture for `email`, for a notification.
    pub async fn lookup(&self, email: &str, verified: Option<&str>) -> (Option<String>, Option<Picture>) {
        let email = email.trim().to_lowercase();
        let card = self.card(&email);
        let name = card.as_ref().and_then(|c| c.name.clone());
        let mut picture = self.local_picture(&email, card.as_ref()).await;
        if picture.is_none() {
            picture = glib::future_with_timeout(NOTE_WAIT, online_picture(&email, verified)).await.ok().flatten();
        }
        (name, picture)
    }

    async fn local_picture(&self, email: &str, card: Option<&Card>) -> Option<Picture> {
        if let (Some(card), Some(contacts)) = (card, self.contacts.clone()) {
            let (book, uid) = (card.book.clone(), card.uid.clone());
            let found = gio::spawn_blocking(move || contacts.photo_sync(&book, &uid).ok().flatten().and_then(|b| images::decode(&b)))
                .await
                .ok()
                .flatten();
            if found.is_some() {
                return found;
            }
        }
        let has_key = self.pgp.borrow().as_ref().is_some_and(|set| set.contains(email));
        if has_key
            && let Some(key) = gpg(&["--export", &format!("<{email}>")]).await
            && let Some(jpeg) = avatars::photo_jpeg(&key)
            && let Some(p) = decoded(jpeg).await
        {
            return Some(p);
        }
        if let Some(Some(png)) = read_cache(&format!("{}.face", avatars::address_hash(email)), None) {
            return decoded(png).await;
        }
        None
    }
}

async fn decoded(bytes: Vec<u8>) -> Option<Picture> {
    gio::spawn_blocking(move || images::decode(&bytes)).await.ok().flatten()
}

/// The brand logo for a message that passed DMARC for `verified`, else
/// Libravatar or Gravatar, as turned on.
async fn online_picture(email: &str, verified: Option<&str>) -> Option<Picture> {
    let settings = Settings::get();
    if settings.avatar_bimi
        && let Some(domain) = verified
        && let Some(bytes) = bimi_logo(domain).await
        && let Some(p) = gio::spawn_blocking(move || images::svg(&bytes)).await.ok().flatten()
    {
        return Some(p);
    }
    let hash = avatars::address_hash(email);
    if settings.avatar_libravatar {
        let name = format!("{hash}.libravatar");
        let bytes = match read_cache(&name, Some(KEEP_FOUND)) {
            Some(hit) => hit,
            None => {
                let domain = email.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase()).unwrap_or_default();
                let base = avatars::libravatar_base(&domain).await;
                store_result(&name, avatars::fetch(&format!("{base}/avatar/{hash}?s=96&d=404")).await)
            }
        };
        if let Some(bytes) = bytes {
            return decoded(bytes).await;
        }
    }
    if settings.avatar_gravatar {
        let name = format!("{hash}.gravatar");
        let bytes = match read_cache(&name, Some(KEEP_FOUND)) {
            Some(hit) => hit,
            None => store_result(&name, avatars::fetch(&format!("https://gravatar.com/avatar/{hash}?s=96&d=404")).await),
        };
        if let Some(bytes) = bytes {
            return decoded(bytes).await;
        }
    }
    None
}

async fn bimi_logo(domain: &str) -> Option<Vec<u8>> {
    let name = format!("{domain}.bimi");
    match read_cache(&name, Some(KEEP_FOUND)) {
        Some(hit) => hit,
        None => {
            let org = links::base_domain(domain);
            let got = match avatars::bimi_url(domain, Some(&org)).await {
                Ok(Some(url)) => avatars::fetch(&url).await,
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            };
            store_result(&name, got)
        }
    }
}

/// Runs gpg with `args`; its output, or None when it fails.
async fn gpg(args: &[&str]) -> Option<Vec<u8>> {
    let mut argv: Vec<&std::ffi::OsStr> = vec!["gpg".as_ref(), "--batch".as_ref(), "--no-tty".as_ref()];
    argv.extend(args.iter().map(|a| std::ffi::OsStr::new(*a)));
    let p = gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE).ok()?;
    let (out, _) = p.communicate_future(None).await.ok()?;
    p.is_successful().then(|| out.map(|b| b.to_vec()).unwrap_or_default())
}

fn cache_dir() -> std::path::PathBuf {
    glib::user_cache_dir().join("salutations").join("avatars")
}

/// A cached result: Some(Some(bytes)) found, Some(None) known missing,
/// None when absent or expired.
fn read_cache(name: &str, keep: Option<Duration>) -> Option<Option<Vec<u8>>> {
    let path = cache_dir().join(name);
    let age = std::fs::metadata(&path).ok()?.modified().ok()?.elapsed().unwrap_or_default();
    let bytes = std::fs::read(&path).ok()?;
    if bytes.is_empty() {
        return (age < KEEP_MISSING).then_some(None);
    }
    if keep.is_some_and(|k| age >= k) {
        return None;
    }
    Some(Some(bytes))
}

fn store_result(name: &str, got: Result<Option<Vec<u8>>, glib::Error>) -> Option<Vec<u8>> {
    match got {
        Ok(found) => {
            let dir = cache_dir();
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(name), found.as_deref().unwrap_or_default());
            found
        }
        Err(e) => {
            log(&format!("people: {name}: {}", e.message()));
            None
        }
    }
}
