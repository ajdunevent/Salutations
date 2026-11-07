use std::ptr;

use glib::{prelude::*, subclass::prelude::*, translate::*};
use libc::{c_char, c_int};

use crate::{FilterDriver, Folder, ProviderType, Service, Session, ffi};

/// Virtual methods of `CamelSession`. Camel calls these from its own
/// worker threads, so implementations must not touch GTK.
pub trait SessionImpl: ObjectImpl + ObjectSubclass<Type: IsA<Session>> {
    /// Creates the service for `uid`. The returned service is registered
    /// with the session by the caller.
    fn add_service(
        &self,
        uid: &str,
        protocol: &str,
        type_: ProviderType,
    ) -> Result<Service, glib::Error> {
        self.parent_add_service(uid, protocol, type_)
    }

    /// Authenticates `service`, typically by obtaining a password and
    /// calling `ServiceExt::authenticate_sync` until it is accepted.
    fn authenticate_sync(
        &self,
        service: &Service,
        mechanism: Option<&str>,
        cancellable: Option<&gio::Cancellable>,
    ) -> Result<(), glib::Error> {
        self.parent_authenticate_sync(service, mechanism, cancellable)
    }

    /// Returns an OAuth2 access token and its lifetime in seconds for the
    /// XOAUTH2 and OAUTHBEARER SASL mechanisms.
    fn oauth2_access_token_sync(
        &self,
        service: &Service,
        cancellable: Option<&gio::Cancellable>,
    ) -> Result<(glib::GString, i32), glib::Error> {
        self.parent_oauth2_access_token_sync(service, cancellable)
    }

    /// Returns the filter driver for `type_` ("incoming" or "junktest").
    /// `CamelSession` has no default, and Camel asks for one whenever
    /// "filter-inbox" is set, which it is by default; `Ok(None)` skips
    /// filtering.
    fn filter_driver(
        &self,
        type_: &str,
        for_folder: Option<&Folder>,
    ) -> Result<Option<FilterDriver>, glib::Error> {
        let _ = (type_, for_folder);
        Ok(None)
    }

    /// Whether to accept `certificate`, which failed validation with
    /// `errors`, for `service`: a `CamelCertTrust` value. `CamelSession`
    /// has no default, so without an implementation every such
    /// certificate is refused (`CAMEL_CERT_TRUST_UNKNOWN`).
    fn trust_prompt(
        &self,
        service: &Service,
        certificate: &gio::TlsCertificate,
        errors: gio::TlsCertificateFlags,
    ) -> ffi::CamelCertTrust {
        let _ = (service, certificate, errors);
        ffi::CAMEL_CERT_TRUST_UNKNOWN
    }
}

pub trait SessionImplExt: SessionImpl {
    fn parent_add_service(
        &self,
        uid: &str,
        protocol: &str,
        type_: ProviderType,
    ) -> Result<Service, glib::Error> {
        unsafe {
            let parent = parent_class::<Self>();
            let f = (*parent)
                .add_service
                .expect("CamelSession parent class has no add_service");
            let mut error = ptr::null_mut();
            let ret = f(
                session_ptr(self),
                uid.to_glib_none().0,
                protocol.to_glib_none().0,
                type_.into_glib(),
                &mut error,
            );
            if error.is_null() && !ret.is_null() {
                Ok(from_glib_full(ret))
            } else if error.is_null() {
                Err(glib::Error::new(
                    gio::IOErrorEnum::Failed,
                    &format!("No service for protocol {protocol}"),
                ))
            } else {
                Err(from_glib_full(error))
            }
        }
    }

    fn parent_authenticate_sync(
        &self,
        service: &Service,
        mechanism: Option<&str>,
        cancellable: Option<&gio::Cancellable>,
    ) -> Result<(), glib::Error> {
        unsafe {
            let parent = parent_class::<Self>();
            let f = (*parent)
                .authenticate_sync
                .expect("CamelSession parent class has no authenticate_sync");
            let mut error = ptr::null_mut();
            let ok = f(
                session_ptr(self),
                service.to_glib_none().0,
                mechanism.to_glib_none().0,
                cancellable.to_glib_none().0,
                &mut error,
            );
            result_from(ok, error)
        }
    }

    fn parent_oauth2_access_token_sync(
        &self,
        service: &Service,
        cancellable: Option<&gio::Cancellable>,
    ) -> Result<(glib::GString, i32), glib::Error> {
        unsafe {
            let parent = parent_class::<Self>();
            let Some(f) = (*parent).get_oauth2_access_token_sync else {
                return Err(glib::Error::new(
                    gio::IOErrorEnum::NotSupported,
                    "OAuth2 is not supported by this session",
                ));
            };
            let mut token = ptr::null_mut();
            let mut expires_in = 0;
            let mut error = ptr::null_mut();
            let ok = f(
                session_ptr(self),
                service.to_glib_none().0,
                &mut token,
                &mut expires_in,
                cancellable.to_glib_none().0,
                &mut error,
            );
            result_from(ok, error)?;
            Ok((from_glib_full(token), expires_in))
        }
    }
}

impl<T: SessionImpl> SessionImplExt for T {}

unsafe impl<T: SessionImpl> IsSubclassable<T> for Session {
    fn class_init(class: &mut glib::Class<Self>) {
        Self::parent_class_init::<T>(class);
        let klass = class.as_mut();
        klass.add_service = Some(add_service::<T>);
        klass.authenticate_sync = Some(authenticate_sync::<T>);
        klass.get_oauth2_access_token_sync = Some(oauth2_access_token_sync::<T>);
        klass.get_filter_driver = Some(filter_driver::<T>);
        klass.trust_prompt = Some(trust_prompt::<T>);
    }
}

unsafe fn parent_class<T: SessionImpl>() -> *const ffi::CamelSessionClass {
    unsafe { T::type_data().as_ref().parent_class() as *const ffi::CamelSessionClass }
}

fn session_ptr<T: SessionImpl>(imp: &T) -> *mut ffi::CamelSession {
    imp.obj().upcast_ref::<Session>().to_glib_none().0
}

unsafe fn result_from(
    ok: glib::ffi::gboolean,
    error: *mut glib::ffi::GError,
) -> Result<(), glib::Error> {
    if !error.is_null() {
        Err(unsafe { from_glib_full(error) })
    } else if ok == glib::ffi::GFALSE {
        Err(glib::Error::new(gio::IOErrorEnum::Failed, "Operation failed"))
    } else {
        Ok(())
    }
}

unsafe fn set_error(error: *mut *mut glib::ffi::GError, e: glib::Error) {
    if !error.is_null() {
        unsafe { *error = e.into_glib_ptr() };
    }
}

unsafe extern "C" fn add_service<T: SessionImpl>(
    session: *mut ffi::CamelSession,
    uid: *const c_char,
    protocol: *const c_char,
    type_: ffi::CamelProviderType,
    error: *mut *mut glib::ffi::GError,
) -> *mut ffi::CamelService {
    unsafe {
        let instance = &*(session as *mut T::Instance);
        let uid = glib::GStr::from_ptr(uid);
        let protocol = glib::GStr::from_ptr(protocol);
        match instance.imp().add_service(uid, protocol, from_glib(type_)) {
            Ok(service) => service.into_glib_ptr(),
            Err(e) => {
                set_error(error, e);
                ptr::null_mut()
            }
        }
    }
}

unsafe extern "C" fn authenticate_sync<T: SessionImpl>(
    session: *mut ffi::CamelSession,
    service: *mut ffi::CamelService,
    mechanism: *const c_char,
    cancellable: *mut gio::ffi::GCancellable,
    error: *mut *mut glib::ffi::GError,
) -> glib::ffi::gboolean {
    unsafe {
        let instance = &*(session as *mut T::Instance);
        let service: Borrowed<Service> = from_glib_borrow(service);
        let mechanism = (!mechanism.is_null()).then(|| glib::GStr::from_ptr(mechanism));
        let cancellable: Option<Borrowed<gio::Cancellable>> =
            (!cancellable.is_null()).then(|| from_glib_borrow(cancellable));
        match instance.imp().authenticate_sync(
            &service,
            mechanism.map(|m| m.as_str()),
            cancellable.as_deref(),
        ) {
            Ok(()) => glib::ffi::GTRUE,
            Err(e) => {
                set_error(error, e);
                glib::ffi::GFALSE
            }
        }
    }
}

unsafe extern "C" fn oauth2_access_token_sync<T: SessionImpl>(
    session: *mut ffi::CamelSession,
    service: *mut ffi::CamelService,
    out_access_token: *mut *mut c_char,
    out_expires_in: *mut c_int,
    cancellable: *mut gio::ffi::GCancellable,
    error: *mut *mut glib::ffi::GError,
) -> glib::ffi::gboolean {
    unsafe {
        let instance = &*(session as *mut T::Instance);
        let service: Borrowed<Service> = from_glib_borrow(service);
        let cancellable: Option<Borrowed<gio::Cancellable>> =
            (!cancellable.is_null()).then(|| from_glib_borrow(cancellable));
        match instance
            .imp()
            .oauth2_access_token_sync(&service, cancellable.as_deref())
        {
            Ok((token, expires_in)) => {
                if !out_access_token.is_null() {
                    *out_access_token = token.into_glib_ptr();
                }
                if !out_expires_in.is_null() {
                    *out_expires_in = expires_in;
                }
                glib::ffi::GTRUE
            }
            Err(e) => {
                set_error(error, e);
                glib::ffi::GFALSE
            }
        }
    }
}

unsafe extern "C" fn filter_driver<T: SessionImpl>(
    session: *mut ffi::CamelSession,
    type_: *const c_char,
    for_folder: *mut ffi::CamelFolder,
    error: *mut *mut glib::ffi::GError,
) -> *mut ffi::CamelFilterDriver {
    unsafe {
        let instance = &*(session as *mut T::Instance);
        let type_ = glib::GStr::from_ptr(type_);
        let for_folder: Option<Borrowed<Folder>> =
            (!for_folder.is_null()).then(|| from_glib_borrow(for_folder));
        match instance.imp().filter_driver(type_, for_folder.as_deref()) {
            Ok(driver) => driver.into_glib_ptr(),
            Err(e) => {
                set_error(error, e);
                ptr::null_mut()
            }
        }
    }
}

unsafe extern "C" fn trust_prompt<T: SessionImpl>(
    session: *mut ffi::CamelSession,
    service: *mut ffi::CamelService,
    certificate: *mut gio::ffi::GTlsCertificate,
    errors: gio::ffi::GTlsCertificateFlags,
) -> ffi::CamelCertTrust {
    unsafe {
        let instance = &*(session as *mut T::Instance);
        let service: Borrowed<Service> = from_glib_borrow(service);
        let certificate: Borrowed<gio::TlsCertificate> = from_glib_borrow(certificate);
        instance.imp().trust_prompt(&service, &certificate, from_glib(errors))
    }
}
