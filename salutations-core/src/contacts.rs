//! The EDS address books, including the ones GOA syncs: recipient
//! completion, whether an address is known, and adding a name and address
//! as a new contact. libebook is called directly, since only a few of its
//! functions are needed.

use std::ffi::{c_char, c_void};
use std::sync::{Arc, Mutex};

use edataserver::prelude::*;
use edataserver::{Source, SourceRegistry};
use glib::translate::*;

#[link(name = "ebook-1.2")]
unsafe extern "C" {
    fn e_book_client_connect_sync(
        source: *mut edataserver::ffi::ESource,
        wait_for_connected_seconds: u32,
        cancellable: *mut gio::ffi::GCancellable,
        error: *mut *mut glib::ffi::GError,
    ) -> *mut glib::gobject_ffi::GObject;
    fn e_book_client_get_contacts_sync(
        client: *mut glib::gobject_ffi::GObject,
        sexp: *const c_char,
        out_contacts: *mut *mut glib::ffi::GSList,
        cancellable: *mut gio::ffi::GCancellable,
        error: *mut *mut glib::ffi::GError,
    ) -> glib::ffi::gboolean;
    fn e_book_client_add_contact_sync(
        client: *mut glib::gobject_ffi::GObject,
        contact: *mut glib::gobject_ffi::GObject,
        opflags: u32,
        out_added_uid: *mut *mut c_char,
        cancellable: *mut gio::ffi::GCancellable,
        error: *mut *mut glib::ffi::GError,
    ) -> glib::ffi::gboolean;
    fn e_contact_new() -> *mut glib::gobject_ffi::GObject;
    fn e_contact_set(contact: *mut glib::gobject_ffi::GObject, field_id: i32, value: *const c_void);
    fn e_contact_get(contact: *mut glib::gobject_ffi::GObject, field_id: i32) -> *mut c_void;
    fn e_contact_field_id(field_name: *const c_char) -> i32;
}

#[link(name = "ebook-1.2")]
unsafe extern "C" {
    fn e_book_client_get_view_sync(
        client: *mut glib::gobject_ffi::GObject,
        sexp: *const c_char,
        out_view: *mut *mut glib::gobject_ffi::GObject,
        cancellable: *mut gio::ffi::GCancellable,
        error: *mut *mut glib::ffi::GError,
    ) -> glib::ffi::gboolean;
    fn e_book_client_view_set_fields_of_interest(
        view: *mut glib::gobject_ffi::GObject,
        fields: *const glib::ffi::GSList,
        error: *mut *mut glib::ffi::GError,
    );
    fn e_book_client_view_start(view: *mut glib::gobject_ffi::GObject, error: *mut *mut glib::ffi::GError);
    fn e_book_client_get_contact_sync(
        client: *mut glib::gobject_ffi::GObject,
        uid: *const c_char,
        out_contact: *mut *mut glib::gobject_ffi::GObject,
        cancellable: *mut gio::ffi::GCancellable,
        error: *mut *mut glib::ffi::GError,
    ) -> glib::ffi::gboolean;
    fn e_contact_photo_free(photo: *mut ContactPhoto);
}

/// libebook's EContactPhoto.
#[repr(C)]
struct ContactPhoto {
    kind: i32,
    data: PhotoData,
}

#[repr(C)]
union PhotoData {
    inlined: std::mem::ManuallyDrop<Inlined>,
    uri: *mut c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Inlined {
    mime_type: *mut c_char,
    length: usize,
    data: *mut u8,
}

const PHOTO_INLINED: i32 = 0;

#[link(name = "edataserver-1.2")]
unsafe extern "C" {
    fn e_client_is_readonly(client: *mut glib::gobject_ffi::GObject) -> glib::ffi::gboolean;
}

const ADDRESS_BOOK: &str = "Address Book";
/// How long connecting waits for a book's backend to come online.
const CONNECT_WAIT_SECONDS: u32 = 5;

/// One name and address from a contact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub name: String,
    pub email: String,
}

/// The parts of a contact that the address index keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// The book's source UID.
    pub book: String,
    pub uid: String,
    pub name: Option<String>,
    /// As stored.
    pub emails: Vec<String>,
}

/// A live view of one book's contacts that have an address; its signals
/// arrive on the main thread.
pub struct View {
    pub book: String,
    pub view: glib::Object,
}

/// Contacts from an `objects-added` or `objects-modified` signal's list.
///
/// # Safety
/// `list` is the signal's GSList of EContact.
pub unsafe fn cards_from(book: &str, list: glib::ffi::gpointer) -> Vec<Card> {
    let (uid_id, name_id, email_id) = unsafe {
        (e_contact_field_id(c"id".as_ptr()), e_contact_field_id(c"full_name".as_ptr()), e_contact_field_id(c"email".as_ptr()))
    };
    let mut out = Vec::new();
    let mut node = list as *mut glib::ffi::GSList;
    while !node.is_null() {
        unsafe {
            let contact = (*node).data as *mut glib::gobject_ffi::GObject;
            let text = |id: i32| {
                let p = e_contact_get(contact, id) as *mut c_char;
                Option::<String>::from_glib_full(p).filter(|s| !s.trim().is_empty())
            };
            let emails = e_contact_get(contact, email_id) as *mut glib::ffi::GList;
            let mut list = Vec::new();
            let mut e = emails;
            while !e.is_null() {
                list.push(from_glib_none::<_, String>((*e).data as *const c_char));
                e = (*e).next;
            }
            glib::ffi::g_list_free_full(emails, Some(glib::ffi::g_free));
            if let Some(uid) = text(uid_id) {
                out.push(Card { book: book.to_string(), uid, name: text(name_id), emails: list });
            }
            node = (*node).next;
        }
    }
    out
}

/// UIDs from an `objects-removed` signal's list.
///
/// # Safety
/// `list` is the signal's GSList of strings.
pub unsafe fn uids_from(list: glib::ffi::gpointer) -> Vec<String> {
    let mut out = Vec::new();
    let mut node = list as *mut glib::ffi::GSList;
    while !node.is_null() {
        unsafe {
            out.push(from_glib_none((*node).data as *const c_char));
            node = (*node).next;
        }
    }
    out
}

/// Starts `view`, once its signals are connected.
pub fn start(view: &View) {
    let mut error = std::ptr::null_mut();
    unsafe { e_book_client_view_start(view.view.as_ptr(), &mut error) };
    if !error.is_null() {
        let e: glib::Error = unsafe { from_glib_full(error) };
        glib::g_debug!("salutations", "Address book view {}: {}", view.book, e);
    }
}

/// An address book a contact can be added to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Book {
    pub uid: String,
    pub name: String,
}

/// The enabled address books, connected on first use from a worker thread.
#[derive(Clone)]
pub struct Contacts {
    sources: Vec<SendSource>,
    /// Each connected book with its source.
    clients: Arc<Mutex<Option<Vec<(SendSource, SendObject)>>>>,
}

struct SendSource(Source);
// ESource and EBookClient are thread-safe GObjects; EDS uses them from
// its own worker threads.
unsafe impl Send for SendSource {}
unsafe impl Sync for SendSource {}
impl Clone for SendSource {
    fn clone(&self) -> Self {
        SendSource(self.0.clone())
    }
}
struct SendObject(glib::Object);

/// A view handed from the worker thread that made it to the main thread.
pub struct SendView(pub View);
unsafe impl Send for SendView {}
unsafe impl Send for SendObject {}
unsafe impl Sync for SendObject {}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

impl Contacts {
    pub fn new(registry: &SourceRegistry) -> Contacts {
        let sources = registry.list_enabled(Some(ADDRESS_BOOK)).into_iter().map(SendSource).collect();
        Contacts { sources, clients: Arc::default() }
    }

    fn connect(&self) {
        let mut clients = self.clients.lock().unwrap();
        if clients.is_some() {
            return;
        }
        let mut list = Vec::new();
        for s in &self.sources {
            let mut error = std::ptr::null_mut();
            let client = unsafe {
                e_book_client_connect_sync(s.0.to_glib_none().0, CONNECT_WAIT_SECONDS, std::ptr::null_mut(), &mut error)
            };
            if client.is_null() {
                if !error.is_null() {
                    let e: glib::Error = unsafe { from_glib_full(error) };
                    glib::g_debug!("salutations", "Address book {:?}: {}", s.0.display_name(), e);
                }
                continue;
            }
            list.push((s.clone(), SendObject(unsafe { from_glib_full(client) })));
        }
        *clients = Some(list);
    }

    /// Contacts whose name, nickname or address starts with or contains
    /// `query`; at most `limit`, one per address. Blocks.
    pub fn search_sync(&self, query: &str, limit: usize) -> Vec<Match> {
        let query = query.trim();
        if query.chars().count() < 2 {
            return Vec::new();
        }
        self.connect();
        let q = escape(query);
        let sexp = format!(
            "(or (contains \"full_name\" \"{q}\") (beginswith \"nickname\" \"{q}\") (beginswith \"email\" \"{q}\"))"
        );
        let (name_id, nick_id, email_id) = unsafe {
            (
                e_contact_field_id(c"full_name".as_ptr()),
                e_contact_field_id(c"nickname".as_ptr()),
                e_contact_field_id(c"email".as_ptr()),
            )
        };
        let mut out: Vec<Match> = Vec::new();
        let clients = self.clients.lock().unwrap();
        for (_, client) in clients.iter().flatten() {
            let mut list = std::ptr::null_mut();
            let mut error = std::ptr::null_mut();
            let ok = unsafe {
                e_book_client_get_contacts_sync(
                    client.0.as_ptr(),
                    sexp.to_glib_none().0,
                    &mut list,
                    std::ptr::null_mut(),
                    &mut error,
                )
            };
            if ok == glib::ffi::GFALSE {
                if !error.is_null() {
                    unsafe { glib::ffi::g_error_free(error) };
                }
                continue;
            }
            let mut node = list;
            while !node.is_null() {
                let contact = unsafe { (*node).data as *mut glib::gobject_ffi::GObject };
                let text = |id: i32| unsafe {
                    let p = e_contact_get(contact, id) as *mut c_char;
                    Option::<String>::from_glib_full(p).filter(|s| !s.trim().is_empty())
                };
                let name = text(name_id).or_else(|| text(nick_id)).unwrap_or_default();
                let emails = unsafe { e_contact_get(contact, email_id) as *mut glib::ffi::GList };
                let mut e = emails;
                while !e.is_null() {
                    let email: String = unsafe { from_glib_none((*e).data as *const c_char) };
                    if !out.iter().any(|m| m.email.eq_ignore_ascii_case(&email)) {
                        out.push(Match { name: name.clone(), email });
                    }
                    e = unsafe { (*e).next };
                }
                unsafe {
                    glib::ffi::g_list_free_full(emails, Some(glib::ffi::g_free));
                    glib::gobject_ffi::g_object_unref(contact);
                }
                node = unsafe { (*node).next };
            }
            unsafe { glib::ffi::g_slist_free(list) };
        }
        // Names and addresses that start with the query come first.
        let lower = query.to_lowercase();
        out.sort_by_key(|m| {
            let starts = m.name.to_lowercase().starts_with(&lower) || m.email.to_lowercase().starts_with(&lower);
            (!starts, m.name.to_lowercase())
        });
        out.truncate(limit);
        out
    }

    /// A view of each book's contacts with an address, not yet started.
    /// Blocks; call from a worker thread, so that the views' signals go to
    /// the main context.
    pub fn views_sync(&self) -> Vec<SendView> {
        self.connect();
        let clients = self.clients.lock().unwrap();
        let mut out = Vec::new();
        for (s, client) in clients.iter().flatten() {
            let mut view = std::ptr::null_mut();
            let mut error = std::ptr::null_mut();
            let ok = unsafe {
                e_book_client_get_view_sync(
                    client.0.as_ptr(),
                    c"(exists \"email\")".as_ptr(),
                    &mut view,
                    std::ptr::null_mut(),
                    &mut error,
                )
            };
            if ok == glib::ffi::GFALSE || view.is_null() {
                if !error.is_null() {
                    unsafe { glib::ffi::g_error_free(error) };
                }
                continue;
            }
            // Only what the index keeps; photos are fetched when shown.
            unsafe {
                let mut fields: *mut glib::ffi::GSList = std::ptr::null_mut();
                for f in [c"full_name", c"email"] {
                    fields = glib::ffi::g_slist_append(fields, f.as_ptr() as glib::ffi::gpointer);
                }
                e_book_client_view_set_fields_of_interest(view, fields, std::ptr::null_mut());
                glib::ffi::g_slist_free(fields);
            }
            let book = s.0.uid().map(|u| u.to_string()).unwrap_or_default();
            out.push(SendView(View { book, view: unsafe { from_glib_full(view) } }));
        }
        out
    }

    /// The photo of contact `uid` in book `book`: its image file's bytes,
    /// None when it has no photo, or why it could not be read. Blocks.
    pub fn photo_sync(&self, book: &str, uid: &str) -> Result<Option<Vec<u8>>, String> {
        self.connect();
        let clients = self.clients.lock().unwrap();
        let Some((_, client)) = clients.iter().flatten().find(|(s, _)| s.0.uid().is_some_and(|u| u == book)) else {
            return Err(format!("address book {book} is not connected"));
        };
        let photo_id = unsafe { e_contact_field_id(c"photo".as_ptr()) };
        let uid = std::ffi::CString::new(uid).map_err(|e| e.to_string())?;
        unsafe {
            let mut contact = std::ptr::null_mut();
            let mut error = std::ptr::null_mut();
            let ok = e_book_client_get_contact_sync(client.0.as_ptr(), uid.as_ptr(), &mut contact, std::ptr::null_mut(), &mut error);
            if ok == glib::ffi::GFALSE || contact.is_null() {
                let e: Option<glib::Error> = (!error.is_null()).then(|| from_glib_full(error));
                return Err(e.map_or("the contact was not found".into(), |e| e.message().to_string()));
            }
            let photo = e_contact_get(contact, photo_id) as *mut ContactPhoto;
            glib::gobject_ffi::g_object_unref(contact);
            if photo.is_null() {
                return Ok(None);
            }
            let bytes = if (*photo).kind == PHOTO_INLINED {
                let i = *(*photo).data.inlined;
                Ok((!i.data.is_null() && i.length > 0).then(|| std::slice::from_raw_parts(i.data, i.length).to_vec()))
            } else {
                let uri: String = from_glib_none((*photo).data.uri);
                match uri.strip_prefix("data:") {
                    // vCard 4 carries photos as data URIs.
                    Some(data) => Ok(data.split_once(";base64,").map(|(_, b64)| glib::base64_decode(b64.trim()))),
                    // The books keep photos as files in their own folders,
                    // which the sandbox reaches read-only.
                    None => gio::File::for_uri(&uri)
                        .load_contents(None::<&gio::Cancellable>)
                        .map(|(b, _)| Some(b.to_vec()))
                        .map_err(|e| format!("{uri}: {}", e.message())),
                }
            };
            e_contact_photo_free(photo);
            bytes
        }
    }

    /// Whether a contact in any book has `email`. Blocks.
    pub fn is_known_sync(&self, email: &str) -> bool {
        let email = email.trim();
        self.connect();
        // "is" compares case-sensitively in some backends, and the same
        // address is often stored and sent in different case; "contains"
        // ignores case, and the addresses found are then compared whole.
        let sexp = format!("(contains \"email\" \"{}\")", escape(email));
        let email_id = unsafe { e_contact_field_id(c"email".as_ptr()) };
        let clients = self.clients.lock().unwrap();
        clients.iter().flatten().any(|(_, client)| {
            let mut list = std::ptr::null_mut();
            let mut error = std::ptr::null_mut();
            let ok = unsafe {
                e_book_client_get_contacts_sync(
                    client.0.as_ptr(),
                    sexp.to_glib_none().0,
                    &mut list,
                    std::ptr::null_mut(),
                    &mut error,
                )
            };
            if !error.is_null() {
                unsafe { glib::ffi::g_error_free(error) };
            }
            let mut found = false;
            let mut node = if ok != glib::ffi::GFALSE { list } else { std::ptr::null_mut() };
            while !node.is_null() {
                unsafe {
                    let contact = (*node).data as *mut glib::gobject_ffi::GObject;
                    let emails = e_contact_get(contact, email_id) as *mut glib::ffi::GList;
                    let mut e = emails;
                    while !e.is_null() {
                        let have: String = from_glib_none((*e).data as *const c_char);
                        found |= have.trim().eq_ignore_ascii_case(email);
                        e = (*e).next;
                    }
                    glib::ffi::g_list_free_full(emails, Some(glib::ffi::g_free));
                    glib::gobject_ffi::g_object_unref(contact);
                    node = (*node).next;
                }
            }
            unsafe { glib::ffi::g_slist_free(list) };
            found
        })
    }

    /// The books a contact can be added to, by name. Blocks.
    pub fn writable_books_sync(&self) -> Vec<Book> {
        self.connect();
        let clients = self.clients.lock().unwrap();
        let mut books: Vec<Book> = clients
            .iter()
            .flatten()
            .filter(|(_, c)| unsafe { e_client_is_readonly(c.0.as_ptr()) } == glib::ffi::GFALSE)
            .map(|(s, _)| Book {
                uid: s.0.uid().map(|u| u.to_string()).unwrap_or_default(),
                name: s.0.display_name().map(|n| n.to_string()).unwrap_or_default(),
            })
            .collect();
        books.sort_by_key(|b| b.name.to_lowercase());
        books
    }

    /// Adds a contact with `name` (when there is one) and `email` to the
    /// book `uid`. Blocks.
    pub fn add_sync(&self, uid: &str, name: Option<&str>, email: &str) -> Result<(), glib::Error> {
        self.connect();
        let clients = self.clients.lock().unwrap();
        let Some((_, client)) = clients.iter().flatten().find(|(s, _)| s.0.uid().is_some_and(|u| u == uid)) else {
            return Err(glib::Error::new(gio::IOErrorEnum::NotFound, "The address book is not available"));
        };
        let (name_id, email_id) =
            unsafe { (e_contact_field_id(c"full_name".as_ptr()), e_contact_field_id(c"email_1".as_ptr())) };
        unsafe {
            let contact = e_contact_new();
            let name = name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or(email);
            let name = std::ffi::CString::new(name).unwrap_or_default();
            let address = std::ffi::CString::new(email).unwrap_or_default();
            e_contact_set(contact, name_id, name.as_ptr() as *const c_void);
            e_contact_set(contact, email_id, address.as_ptr() as *const c_void);
            let mut error = std::ptr::null_mut();
            let ok = e_book_client_add_contact_sync(
                client.0.as_ptr(),
                contact,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut error,
            );
            glib::gobject_ffi::g_object_unref(contact);
            if ok == glib::ffi::GFALSE {
                return Err(if error.is_null() {
                    glib::Error::new(gio::IOErrorEnum::Failed, "The contact was not added")
                } else {
                    from_glib_full(error)
                });
            }
        }
        Ok(())
    }
}
