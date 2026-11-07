//! People for avatars and the contact menu: an index of every address-book
//! contact with an address, kept current by live views of the books, and
//! the images avatars show.
//!
//! An avatar's image comes from the first source that has one: the
//! address book, an OpenPGP photo ID in the user's keyring, a `Face:`
//! header seen in the sender's mail; then, each only when turned on, a
//! BIMI logo (message view, verified senders only), Libravatar and
//! Gravatar. Images are fetched when an avatar first needs one. A contact's
//! name gives the initials. Avatars made or bound here change when the
//! address book or the settings do.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use salutations_core::Settings;
use salutations_core::avatars;
use salutations_core::contacts::{self, Card, Contacts};

/// How long a found image and a known absence stay cached on disk.
const KEEP_FOUND: Duration = Duration::from_secs(7 * 24 * 3600);
const KEEP_MISSING: Duration = Duration::from_secs(24 * 3600);
/// How long a notification waits for an online picture before going out
/// without one.
const NOTE_WAIT: Duration = Duration::from_secs(5);
/// A notification's picture, in pixels; brand logos are drawn at this size.
const NOTE_SIZE: i32 = 96;

/// Fetching, then the image or its absence.
#[derive(Clone)]
enum Slot<T> {
    Pending,
    Done(Option<T>),
}

/// An avatar to keep current, and what it shows.
struct Bound {
    avatar: glib::WeakRef<adw::Avatar>,
    name: String,
    email: String,
    /// Online sources may be asked; false for Junk.
    network: bool,
    /// The sender's domain when this message passed verification, for
    /// BIMI.
    verified: Option<String>,
}

/// Photos are scaled to this many pixels square once; avatars show them at
/// 32 to 64 px.
const PHOTO_SIZE: i32 = 96;

type Key = (String, String);

#[derive(Default)]
struct Index {
    contacts: Option<Contacts>,
    /// False until every book has listed its contacts.
    ready: Cell<bool>,
    pending: Cell<usize>,
    cards: RefCell<HashMap<Key, Card>>,
    /// Lowercased address to contact.
    by_email: RefCell<HashMap<String, Key>>,
    /// None while fetching, then the photo or its absence.
    photos: RefCell<HashMap<Key, Option<Option<gdk::Texture>>>>,
    avatars: RefCell<Vec<Bound>>,
    views: RefCell<Vec<contacts::View>>,
    /// By lowercased address: the address book, OpenPGP and Face.
    local: RefCell<HashMap<String, Slot<gdk::Texture>>>,
    /// By lowercased address: Libravatar and Gravatar.
    online: RefCell<HashMap<String, Slot<gdk::Texture>>>,
    /// By domain.
    bimi: RefCell<HashMap<String, Slot<gdk::Paintable>>>,
    /// Addresses whose keys carry a photo ID; None until gpg has listed
    /// them.
    pgp: RefCell<Option<HashSet<String>>>,
}

thread_local! {
    static INDEX: RefCell<Rc<Index>> = RefCell::new(Rc::default());
}

fn index() -> Rc<Index> {
    INDEX.with_borrow(|i| i.clone())
}

/// Starts the index on `contacts`. Call once.
pub fn start(contacts: Contacts) {
    let idx = Rc::new(Index { contacts: Some(contacts.clone()), ..Default::default() });
    INDEX.with_borrow_mut(|i| *i = idx.clone());
    glib::spawn_future_local(async move {
        let views = gio::spawn_blocking(move || contacts.views_sync()).await.unwrap_or_default();
        crate::debug::log(&format!("people: {} address book views", views.len()));
        idx.pending.set(views.len());
        if views.is_empty() {
            idx.ready.set(true);
        }
        for contacts::SendView(view) in views {
            connect(&idx, &view);
            contacts::start(&view);
            idx.views.borrow_mut().push(view);
        }
    });
    glib::spawn_future_local(async {
        let listed = gpg(&["--with-colons", "--list-keys"]).await;
        let found = listed.map(|out| avatars::photo_addresses(&String::from_utf8_lossy(&out))).unwrap_or_default();
        crate::debug::log(&format!("people: {} addresses with OpenPGP photos", found.len()));
        let idx = index();
        *idx.pgp.borrow_mut() = Some(found);
        idx.local.borrow_mut().retain(|_, s| matches!(s, Slot::Done(Some(_))));
        idx.refresh_avatars();
    });
    crate::preferences::connect_changed(|| {
        let idx = index();
        idx.online.borrow_mut().clear();
        idx.bimi.borrow_mut().clear();
        idx.refresh_avatars();
    });
}

/// Runs gpg with `args`; its output, or None when it fails.
async fn gpg(args: &[&str]) -> Option<Vec<u8>> {
    let mut argv: Vec<&std::ffi::OsStr> = vec!["gpg".as_ref(), "--batch".as_ref(), "--no-tty".as_ref()];
    argv.extend(args.iter().map(|a| std::ffi::OsStr::new(*a)));
    let p = gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE).ok()?;
    let (out, _) = p.communicate_future(None).await.ok()?;
    p.is_successful().then(|| out.map(|b| b.to_vec()).unwrap_or_default())
}

fn connect(idx: &Rc<Index>, view: &contacts::View) {
    for (signal, removed) in [("objects-added", false), ("objects-modified", false), ("objects-removed", true)] {
        let (weak, book) = (Rc::downgrade(idx), view.book.clone());
        view.view.connect_local(signal, false, move |args| {
            let idx = weak.upgrade()?;
            let list = args.get(1)?.get::<glib::Pointer>().ok()?;
            if removed {
                let uids = unsafe { contacts::uids_from(list) };
                idx.remove(&book, &uids);
            } else {
                let cards = unsafe { contacts::cards_from(&book, list) };
                idx.put(cards);
            }
            None
        });
    }
    let weak = Rc::downgrade(idx);
    view.view.connect_local("complete", false, move |_| {
        let idx = weak.upgrade()?;
        let left = idx.pending.get().saturating_sub(1);
        idx.pending.set(left);
        if left == 0 {
            idx.ready.set(true);
        }
        None
    });
}

impl Index {
    fn put(self: &Rc<Self>, cards: Vec<Card>) {
        crate::debug::log(&format!("people: {} contacts from the books", cards.len()));
        {
            let mut all = self.cards.borrow_mut();
            let mut by_email = self.by_email.borrow_mut();
            let mut photos = self.photos.borrow_mut();
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
                // A change may be the photo; fetched again when shown.
                photos.remove(&key);
                let mut local = self.local.borrow_mut();
                for e in &card.emails {
                    local.remove(&e.trim().to_lowercase());
                }
                all.insert(key, card);
            }
        }
        self.refresh_avatars();
    }

    fn remove(self: &Rc<Self>, book: &str, uids: &[String]) {
        {
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
                        self.local.borrow_mut().remove(&e);
                    }
                }
                self.photos.borrow_mut().remove(&key);
            }
        }
        self.refresh_avatars();
    }

    fn card(&self, email: &str) -> Option<Card> {
        let key = self.by_email.borrow().get(&email.trim().to_lowercase()).cloned()?;
        self.cards.borrow().get(&key).cloned()
    }

    /// The address book's photo for `card`.
    async fn book_photo(self: &Rc<Self>, card: &Card) -> Option<gdk::Texture> {
        let key = (card.book.clone(), card.uid.clone());
        if let Some(Some(photo)) = self.photos.borrow().get(&key) {
            return photo.clone();
        }
        let contacts = self.contacts.clone()?;
        let (book, uid) = key.clone();
        let photo = fetch(contacts, book, uid).await;
        crate::debug::log(&format!("people: photo for {} {}", key.1, if photo.is_some() { "found" } else { "absent" }));
        self.photos.borrow_mut().insert(key, Some(photo.clone()));
        photo
    }

    /// The first local image for `email`: address book, OpenPGP, Face.
    async fn local_image(self: &Rc<Self>, email: &str) -> Option<gdk::Texture> {
        if let Some(card) = self.card(email)
            && let Some(photo) = self.book_photo(&card).await
        {
            return Some(photo);
        }
        let has_key = self.pgp.borrow().as_ref().is_some_and(|set| set.contains(email));
        if has_key
            && let Some(key) = gpg(&["--export", &format!("<{email}>")]).await
            && let Some(jpeg) = avatars::photo_jpeg(&key)
            && let Some(t) = texture(jpeg).await
        {
            return Some(t);
        }
        if let Some(Some(png)) = read_cache(&format!("{}.face", avatars::address_hash(email)), None) {
            return texture(png).await;
        }
        None
    }

    /// Libravatar, then Gravatar, as turned on.
    async fn online_image(email: &str) -> Option<gdk::Texture> {
        let settings = Settings::get();
        let hash = avatars::address_hash(email);
        if settings.avatar_libravatar {
            let name = format!("{hash}.libravatar");
            let bytes = match read_cache(&name, Some(KEEP_FOUND)) {
                Some(hit) => hit,
                None => {
                    let domain = email.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase()).unwrap_or_default();
                    let base = avatars::libravatar_base(&domain).await;
                    let got = avatars::fetch(&format!("{base}/avatar/{hash}?s=96&d=404")).await;
                    store_result(&name, got)
                }
            };
            if let Some(bytes) = bytes {
                return texture(bytes).await;
            }
        }
        if settings.avatar_gravatar {
            let name = format!("{hash}.gravatar");
            let bytes = match read_cache(&name, Some(KEEP_FOUND)) {
                Some(hit) => hit,
                None => store_result(&name, avatars::fetch(&format!("https://gravatar.com/avatar/{hash}?s=96&d=404")).await),
            };
            if let Some(bytes) = bytes {
                return texture(bytes).await;
            }
        }
        None
    }

    async fn bimi_logo(domain: &str) -> Option<gdk::Paintable> {
        let name = format!("{domain}.bimi");
        let bytes = match read_cache(&name, Some(KEEP_FOUND)) {
            Some(hit) => hit,
            None => {
                let org = salutations_core::links::base_domain(domain);
                let got = match avatars::bimi_url(domain, Some(&org)).await {
                    Ok(Some(url)) => avatars::fetch(&url).await,
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                };
                store_result(&name, got)
            }
        }?;
        // Only SVG; no animation and nothing loaded from elsewhere.
        if !String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).contains("<svg") {
            return None;
        }
        let svg = gtk::Svg::new();
        svg.set_features(gtk::SvgFeatures::empty());
        svg.load_from_bytes(&glib::Bytes::from_owned(bytes));
        Some(svg.upcast())
    }

    fn refresh_avatars(self: &Rc<Self>) {
        let list: Vec<_> = {
            let mut avatars = self.avatars.borrow_mut();
            avatars.retain(|b| b.avatar.upgrade().is_some());
            avatars.iter().filter_map(|b| Some((b.avatar.upgrade()?, b.name.clone(), b.email.clone(), b.network, b.verified.clone()))).collect()
        };
        for (avatar, name, email, network, verified) in list {
            self.apply(&avatar, &name, &email, network, verified.as_deref());
        }
    }

    /// Starts `fut` for `key` in `map` unless it has started; the slot's
    /// state now.
    fn slot<T: Clone + 'static>(
        self: &Rc<Self>,
        map: fn(&Index) -> &RefCell<HashMap<String, Slot<T>>>,
        key: &str,
        fut: impl std::future::Future<Output = Option<T>> + 'static,
    ) -> Slot<T> {
        if let Some(s) = map(self).borrow().get(key) {
            return s.clone();
        }
        map(self).borrow_mut().insert(key.to_string(), Slot::Pending);
        let (this, key) = (self.clone(), key.to_string());
        glib::spawn_future_local(async move {
            let found = fut.await;
            // A change meanwhile dropped the slot; a newer fetch follows.
            if let Some(s) = map(&this).borrow_mut().get_mut(&key) {
                *s = Slot::Done(found);
            }
            this.refresh_avatars();
        });
        Slot::Pending
    }

    fn apply(self: &Rc<Self>, avatar: &adw::Avatar, name: &str, email: &str, network: bool, verified: Option<&str>) {
        let email = email.trim().to_lowercase();
        let card = self.card(&email);
        let text = card.as_ref().and_then(|c| c.name.clone()).unwrap_or_else(|| if name.is_empty() { email.clone() } else { name.to_string() });
        avatar.set_text(Some(&text));
        let image = self.image(&email, network, verified);
        avatar.set_custom_image(image.as_ref());
    }

    /// The image for `email` now, starting fetches as needed.
    fn image(self: &Rc<Self>, email: &str, network: bool, verified: Option<&str>) -> Option<gdk::Paintable> {
        let this = self.clone();
        let e = email.to_string();
        match self.slot(|i| &i.local, email, async move { this.local_image(&e).await }) {
            Slot::Done(Some(t)) => return Some(t.upcast()),
            Slot::Pending => return None,
            Slot::Done(None) => {}
        }
        if !network {
            return None;
        }
        let settings = Settings::get();
        if settings.avatar_bimi
            && let Some(domain) = verified
        {
            let d = domain.to_string();
            match self.slot(|i| &i.bimi, domain, async move { Index::bimi_logo(&d).await }) {
                Slot::Done(Some(p)) => return Some(p),
                Slot::Pending => return None,
                Slot::Done(None) => {}
            }
        }
        if settings.avatar_libravatar || settings.avatar_gravatar {
            let e = email.to_string();
            if let Slot::Done(Some(t)) = self.slot(|i| &i.online, email, async move { Index::online_image(&e).await }) {
                return Some(t.upcast());
            }
        }
        None
    }
}

fn cache_dir() -> std::path::PathBuf {
    glib::user_cache_dir().join("salutations").join("avatars")
}

/// A cached result: Some(Some(bytes)) found, Some(None) known missing,
/// None when absent or expired. Found entries older than `keep` expire;
/// with no `keep` they never do. Missing ones expire after KEEP_MISSING.
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

/// Stores a fetch's result; an empty file records a known absence. Errors
/// are not stored, so the next view asks again.
fn store_result(name: &str, got: Result<Option<Vec<u8>>, glib::Error>) -> Option<Vec<u8>> {
    match got {
        Ok(found) => {
            let dir = cache_dir();
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(name), found.as_deref().unwrap_or_default());
            found
        }
        Err(e) => {
            crate::debug::log(&format!("people: {name}: {}", e.message()));
            None
        }
    }
}

/// Decodes and scales an image off the main thread.
async fn texture(bytes: Vec<u8>) -> Option<gdk::Texture> {
    let scaled = gio::spawn_blocking(move || scale(&bytes)).await.ok().flatten()?;
    let (w, h, stride, pixels) = scaled;
    Some(gdk::MemoryTexture::new(w, h, gdk::MemoryFormat::R8g8b8a8, &glib::Bytes::from_owned(pixels), stride).upcast())
}

/// Decodes `bytes` and scales it to at most PHOTO_SIZE square, cropped to
/// a square from the middle: width, height, stride, RGBA pixels. GTK's
/// decoder (PNG, JPEG, TIFF) is used: it is safe off the main thread, and
/// gdk-pixbuf's loaders were not available in every sandbox.
fn scale(bytes: &[u8]) -> Option<(i32, i32, usize, Vec<u8>)> {
    let texture = gdk::Texture::from_bytes(&glib::Bytes::from(bytes)).ok()?;
    let (w, h) = (texture.width() as usize, texture.height() as usize);
    let side = w.min(h);
    if side == 0 {
        return None;
    }
    let mut down = gdk::TextureDownloader::new(&texture);
    down.set_format(gdk::MemoryFormat::R8g8b8a8);
    let (pixels, stride) = down.download_bytes();
    let (x0, y0) = ((w - side) / 2, (h - side) / 2);
    let size = side.min(PHOTO_SIZE as usize);
    // Each output pixel averages the block of source pixels it covers.
    let mut out = vec![0u8; size * size * 4];
    for oy in 0..size {
        let (sy0, sy1) = (y0 + oy * side / size, y0 + ((oy + 1) * side / size).max(oy * side / size + 1));
        for ox in 0..size {
            let (sx0, sx1) = (x0 + ox * side / size, x0 + ((ox + 1) * side / size).max(ox * side / size + 1));
            let mut sum = [0u32; 4];
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let i = sy * stride + sx * 4;
                    for c in 0..4 {
                        sum[c] += pixels[i + c] as u32;
                    }
                }
            }
            let n = ((sy1 - sy0) * (sx1 - sx0)) as u32;
            let o = (oy * size + ox) * 4;
            for c in 0..4 {
                out[o + c] = (sum[c] / n) as u8;
            }
        }
    }
    Some((size as i32, size as i32, size * 4, out))
}

thread_local! {
    /// The folder being shown is Junk: its senders' avatars ask no online
    /// source.
    static OFFLINE: Cell<bool> = const { Cell::new(false) };
}

pub fn set_junk_shown(junk: bool) {
    OFFLINE.set(junk);
}

pub fn junk_shown() -> bool {
    OFFLINE.get()
}

/// An avatar for a message whose sender's `verified` domain passed
/// verification, which may show that domain's BIMI logo.
pub fn avatar_verified(size: i32, name: &str, email: &str, verified: Option<&str>) -> adw::Avatar {
    let a = adw::Avatar::new(size, Some(if name.is_empty() { email } else { name }), true);
    bind(&a, name, email, !OFFLINE.get(), verified);
    a
}

/// Makes `avatar` show `email`'s image, now and on changes. `network`
/// allows online sources; `verified` is the sender's domain when the
/// message passed verification, for BIMI.
pub fn bind(avatar: &adw::Avatar, name: &str, email: &str, network: bool, verified: Option<&str>) {
    let idx = index();
    {
        let mut avatars = idx.avatars.borrow_mut();
        avatars.retain(|b| b.avatar.upgrade().is_some_and(|w| &w != avatar));
        avatars.push(Bound {
            avatar: avatar.downgrade(),
            name: name.to_string(),
            email: email.to_string(),
            network,
            verified: verified.map(str::to_string),
        });
    }
    idx.apply(avatar, name, email, network, verified);
}

/// Keeps a `Face:` image from a message from `email`, for its other
/// messages.
pub fn learn_face(email: &str, png: &[u8]) {
    let name = format!("{}.face", avatars::address_hash(email));
    if read_cache(&name, None).flatten().as_deref() == Some(png) {
        return;
    }
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(name), png);
    let idx = index();
    let key = email.trim().to_lowercase();
    if matches!(idx.local.borrow().get(&key), Some(Slot::Done(None))) {
        idx.local.borrow_mut().remove(&key);
        idx.refresh_avatars();
    }
}

/// Whether an address book has `email`; None until the books are listed.
pub fn known(email: &str) -> Option<bool> {
    let idx = index();
    if idx.card(email).is_some() {
        return Some(true);
    }
    idx.ready.get().then_some(false)
}

/// The contact's name and image for `email`, for a notification icon:
/// the address book, OpenPGP and Face, then the online sources turned on.
pub async fn lookup(email: &str, verified: Option<&str>) -> (Option<String>, Option<gdk::Texture>) {
    let idx = index();
    let email = email.trim().to_lowercase();
    let name = idx.card(&email).and_then(|c| c.name);
    let mut image = idx.local_image(&email).await;
    if image.is_none() {
        image = glib::future_with_timeout(NOTE_WAIT, note_online_image(&email, verified)).await.ok().flatten();
    }
    (name, image)
}

/// The brand logo for a message that passed DMARC for `verified`, else
/// Libravatar or Gravatar, as turned on.
async fn note_online_image(email: &str, verified: Option<&str>) -> Option<gdk::Texture> {
    if let Some(domain) = verified.filter(|_| Settings::get().avatar_bimi)
        && let Some(logo) = Index::bimi_logo(domain).await
        && let Some(t) = raster(&logo, NOTE_SIZE)
    {
        return Some(t);
    }
    Index::online_image(email).await
}

/// `p` drawn into a `size` square bitmap, for notifications, which take
/// images, not SVG.
fn raster(p: &gdk::Paintable, size: i32) -> Option<gdk::Texture> {
    let snapshot = gtk::Snapshot::new();
    p.snapshot(&snapshot, size as f64, size as f64);
    let node = snapshot.to_node()?;
    let renderer = gtk::gsk::CairoRenderer::new();
    renderer.realize(None::<&gdk::Surface>).ok()?;
    let bounds = gtk::graphene::Rect::new(0.0, 0.0, size as f32, size as f32);
    let texture = renderer.render_texture(&node, Some(&bounds));
    renderer.unrealize();
    Some(texture)
}

async fn fetch(contacts: Contacts, book: String, uid: String) -> Option<gdk::Texture> {
    let scaled = gio::spawn_blocking(move || match contacts.photo_sync(&book, &uid) {
        Ok(Some(bytes)) => scale(&bytes).ok_or_else(|| "the image could not be decoded".to_string()).map(Some),
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    })
    .await
    .unwrap_or_else(|_| Err("the photo fetch failed".into()))
    .unwrap_or_else(|e| {
        crate::debug::log(&format!("people: photo not shown: {e}"));
        None
    });
    scaled.map(|(w, h, stride, bytes)| {
        gdk::MemoryTexture::new(w, h, gdk::MemoryFormat::R8g8b8a8, &glib::Bytes::from_owned(bytes), stride).upcast::<gdk::Texture>()
    })
}
