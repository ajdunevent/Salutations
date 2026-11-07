//! Camel's GnuPG cipher context: PGP/MIME signing, encryption, decryption
//! and verification, and public key listing, export and import. Every call
//! runs `gpg` and blocks.

use std::ffi::c_char;

use glib::prelude::*;
use glib::translate::*;

use crate::{MimePart, Session, ffi};

/// A `CamelGpgContext`.
#[derive(Clone, Debug)]
pub struct GpgContext(glib::Object);

// Each call runs its own gpg process; Evolution uses contexts from worker
// threads.
unsafe impl Send for GpgContext {}
unsafe impl Sync for GpgContext {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignStatus {
    None,
    Good,
    Bad,
    /// A correct signature by a key the user has not certified.
    Unknown,
    /// Signed by a key that is not in the keyring.
    NeedPublicKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signer {
    pub name: Option<String>,
    pub email: Option<String>,
}

/// What decrypting or verifying found.
#[derive(Debug, Clone)]
pub struct Validity {
    pub sign: SignStatus,
    pub signers: Vec<Signer>,
    /// gpg's own report, which names the signing key.
    pub sign_description: Option<String>,
    pub encrypted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    pub fingerprint: String,
    pub id: String,
    pub user_ids: Vec<String>,
    /// `CamelGpgTrust`: 0 none, 1 unknown, 2 never, 3 marginal, 4 full,
    /// 5 ultimate.
    pub trust: i32,
    pub created: i64,
}

unsafe fn take_error(error: *mut glib::ffi::GError) -> glib::Error {
    if error.is_null() {
        glib::Error::new(gio::IOErrorEnum::Failed, "gpg failed")
    } else {
        unsafe { from_glib_full(error) }
    }
}

unsafe fn queue_signers(queue: &glib::ffi::GQueue) -> Vec<Signer> {
    let mut out = Vec::new();
    let mut link = queue.head;
    while !link.is_null() {
        unsafe {
            let info = (*link).data as *const ffi::CamelCipherCertInfo;
            if !info.is_null() {
                out.push(Signer {
                    name: Option::<String>::from_glib_none((*info).name).filter(|s| !s.is_empty()),
                    email: Option::<String>::from_glib_none((*info).email).filter(|s| !s.is_empty()),
                });
            }
            link = (*link).next;
        }
    }
    out
}

/// Reads and frees a validity.
unsafe fn take_validity(v: *mut ffi::CamelCipherValidity) -> Validity {
    unsafe {
        let sign = match (*v).sign.status {
            ffi::CAMEL_CIPHER_VALIDITY_SIGN_GOOD => SignStatus::Good,
            ffi::CAMEL_CIPHER_VALIDITY_SIGN_BAD => SignStatus::Bad,
            ffi::CAMEL_CIPHER_VALIDITY_SIGN_UNKNOWN => SignStatus::Unknown,
            ffi::CAMEL_CIPHER_VALIDITY_SIGN_NEED_PUBLIC_KEY => SignStatus::NeedPublicKey,
            _ => SignStatus::None,
        };
        let out = Validity {
            sign,
            signers: queue_signers(&(*v).sign.signers),
            sign_description: Option::<String>::from_glib_none((*v).sign.description),
            encrypted: (*v).encrypt.status != ffi::CAMEL_CIPHER_VALIDITY_ENCRYPT_NONE,
        };
        ffi::camel_cipher_validity_free(v);
        out
    }
}

unsafe fn take_key_infos(list: *mut glib::ffi::GSList) -> Vec<KeyInfo> {
    let mut out = Vec::new();
    let mut link = list;
    while !link.is_null() {
        unsafe {
            let info = (*link).data as *const ffi::CamelGpgKeyInfo;
            let mut uids = Vec::new();
            let mut u = ffi::camel_gpg_key_info_get_user_ids(info);
            while !u.is_null() {
                if let Some(s) = Option::<String>::from_glib_none((*u).data as *const c_char) {
                    uids.push(s);
                }
                u = (*u).next;
            }
            out.push(KeyInfo {
                fingerprint: from_glib_none(ffi::camel_gpg_key_info_get_fingerprint(info)),
                id: from_glib_none(ffi::camel_gpg_key_info_get_id(info)),
                user_ids: uids,
                trust: ffi::camel_gpg_key_info_get_trust(info),
                created: ffi::camel_gpg_key_info_get_creation_date(info),
            });
            link = (*link).next;
        }
    }
    unsafe {
        glib::ffi::g_slist_free_full(list, Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut ffi::CamelGpgKeyInfo),
            unsafe extern "C" fn(glib::ffi::gpointer),
        >(ffi::camel_gpg_key_info_free)))
    };
    out
}

impl GpgContext {
    /// Without a session, Camel asks no session for passwords or keys;
    /// gpg-agent still asks for passphrases.
    pub fn new(session: Option<&impl IsA<Session>>) -> Self {
        unsafe {
            let ctx = ffi::camel_gpg_context_new(session.map(|s| s.as_ref()).to_glib_none().0);
            GpgContext(from_glib_full(ctx as *mut glib::gobject_ffi::GObject))
        }
    }

    fn cipher(&self) -> *mut ffi::CamelCipherContext {
        self.0.as_ptr() as *mut ffi::CamelCipherContext
    }

    fn gpg(&self) -> *mut ffi::CamelGpgContext {
        self.0.as_ptr() as *mut ffi::CamelGpgContext
    }

    /// Encrypts to keys the user has not certified (gpg --always-trust).
    /// Affects encryption only; verification still reports such keys.
    pub fn set_always_trust(&self, on: bool) {
        unsafe { ffi::camel_gpg_context_set_always_trust(self.gpg(), on.into_glib()) }
    }

    /// `part` as multipart/signed by `signer` (a fingerprint or key ID).
    pub fn sign_sync(&self, signer: &str, part: &MimePart) -> Result<MimePart, glib::Error> {
        let out = MimePart::new();
        let mut error = std::ptr::null_mut();
        let ok = unsafe {
            ffi::camel_cipher_context_sign_sync(
                self.cipher(),
                signer.to_glib_none().0,
                ffi::CAMEL_CIPHER_HASH_DEFAULT,
                part.to_glib_none().0,
                out.to_glib_none().0,
                std::ptr::null_mut(),
                &mut error,
            )
        };
        if ok == glib::ffi::GFALSE {
            return Err(unsafe { take_error(error) });
        }
        Ok(out)
    }

    /// `part` as multipart/encrypted to `recipients` (addresses or key IDs).
    pub fn encrypt_sync(&self, recipients: &[String], part: &MimePart) -> Result<MimePart, glib::Error> {
        let out = MimePart::new();
        let mut error = std::ptr::null_mut();
        let ok = unsafe {
            let array = glib::ffi::g_ptr_array_new_with_free_func(Some(glib::ffi::g_free));
            for r in recipients {
                let p: *mut c_char = r.as_str().to_glib_full();
                glib::ffi::g_ptr_array_add(array, p as glib::ffi::gpointer);
            }
            let ok = ffi::camel_cipher_context_encrypt_sync(
                self.cipher(),
                std::ptr::null(),
                array,
                part.to_glib_none().0,
                out.to_glib_none().0,
                std::ptr::null_mut(),
                &mut error,
            );
            glib::ffi::g_ptr_array_unref(array);
            ok
        };
        if ok == glib::ffi::GFALSE {
            return Err(unsafe { take_error(error) });
        }
        Ok(out)
    }

    /// The decrypted content of a multipart/encrypted (or inline PGP)
    /// part, with any signature inside checked.
    pub fn decrypt_sync(&self, part: &MimePart) -> Result<(MimePart, Validity), glib::Error> {
        let out = MimePart::new();
        let mut error = std::ptr::null_mut();
        let v = unsafe {
            ffi::camel_cipher_context_decrypt_sync(
                self.cipher(),
                part.to_glib_none().0,
                out.to_glib_none().0,
                std::ptr::null_mut(),
                &mut error,
            )
        };
        if v.is_null() {
            return Err(unsafe { take_error(error) });
        }
        Ok((out, unsafe { take_validity(v) }))
    }

    /// Checks a multipart/signed part.
    pub fn verify_sync(&self, part: &MimePart) -> Result<Validity, glib::Error> {
        let mut error = std::ptr::null_mut();
        let v = unsafe {
            ffi::camel_cipher_context_verify_sync(self.cipher(), part.to_glib_none().0, std::ptr::null_mut(), &mut error)
        };
        if v.is_null() {
            return Err(unsafe { take_error(error) });
        }
        Ok(unsafe { take_validity(v) })
    }

    /// Public keys in the keyring matching `keyid` (an address, key ID or
    /// fingerprint).
    pub fn public_key_info_sync(&self, keyid: &str) -> Result<Vec<KeyInfo>, glib::Error> {
        let mut list = std::ptr::null_mut();
        let mut error = std::ptr::null_mut();
        let ok = unsafe {
            ffi::camel_gpg_context_get_public_key_info_sync(
                self.gpg(),
                keyid.to_glib_none().0,
                0,
                &mut list,
                std::ptr::null_mut(),
                &mut error,
            )
        };
        if ok == glib::ffi::GFALSE {
            // gpg exits with an error when nothing matches.
            unsafe {
                if !error.is_null() {
                    glib::ffi::g_error_free(error);
                }
            }
            return Ok(Vec::new());
        }
        Ok(unsafe { take_key_infos(list) })
    }

    /// The keys in `data` (armored or binary), without importing them.
    pub fn key_data_info_sync(&self, data: &[u8]) -> Result<Vec<KeyInfo>, glib::Error> {
        let mut list = std::ptr::null_mut();
        let mut error = std::ptr::null_mut();
        let ok = unsafe {
            ffi::camel_gpg_context_get_key_data_info_sync(
                self.gpg(),
                data.as_ptr(),
                data.len(),
                0,
                &mut list,
                std::ptr::null_mut(),
                &mut error,
            )
        };
        if ok == glib::ffi::GFALSE {
            return Err(unsafe { take_error(error) });
        }
        Ok(unsafe { take_key_infos(list) })
    }

    pub fn import_key_sync(&self, data: &[u8]) -> Result<(), glib::Error> {
        let mut error = std::ptr::null_mut();
        let ok = unsafe {
            ffi::camel_gpg_context_import_key_sync(self.gpg(), data.as_ptr(), data.len(), 0, std::ptr::null_mut(), &mut error)
        };
        if ok == glib::ffi::GFALSE {
            return Err(unsafe { take_error(error) });
        }
        Ok(())
    }

    /// The public key `keyid`, binary and minimal: what an Autocrypt header
    /// carries.
    pub fn public_key_sync(&self, keyid: &str) -> Result<Vec<u8>, glib::Error> {
        let mut data = std::ptr::null_mut();
        let mut size = 0usize;
        let mut error = std::ptr::null_mut();
        let ok = unsafe {
            ffi::camel_gpg_context_get_public_key_sync(
                self.gpg(),
                keyid.to_glib_none().0,
                0,
                &mut data,
                &mut size,
                std::ptr::null_mut(),
                &mut error,
            )
        };
        if ok == glib::ffi::GFALSE {
            return Err(unsafe { take_error(error) });
        }
        let bytes = unsafe { std::slice::from_raw_parts(data, size).to_vec() };
        unsafe { glib::ffi::g_free(data as glib::ffi::gpointer) };
        Ok(bytes)
    }
}
