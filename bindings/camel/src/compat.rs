//! Calls whose C API changed in Camel 3.58. The build script sets the
//! `camel_3_58` cfg from the installed Camel version; each function here
//! has one body per API. The declarations are written by hand because the
//! generated ones only describe the version the bindings were made from.

use std::ffi::{c_char, c_void};

use glib::translate::*;

use crate::ffi;

#[cfg(camel_3_58)]
unsafe extern "C" {
    fn camel_folder_dup_uids(folder: *mut ffi::CamelFolder) -> *mut glib::ffi::GPtrArray;
    fn camel_folder_search_sync(
        folder: *mut ffi::CamelFolder,
        expression: *const c_char,
        out_uids: *mut *mut glib::ffi::GPtrArray,
        cancellable: *mut gio::ffi::GCancellable,
        error: *mut *mut glib::ffi::GError,
    ) -> glib::ffi::gboolean;
    fn camel_folder_save_state(folder: *mut ffi::CamelFolder);
    fn camel_folder_get_state_filename(folder: *mut ffi::CamelFolder) -> *const c_char;
    fn camel_folder_thread_new(
        folder: *mut ffi::CamelFolder,
        uids: *mut glib::ffi::GPtrArray,
        flags: u32,
    ) -> *mut c_void;
    fn camel_folder_thread_get_tree(thread: *mut c_void) -> *mut c_void;
    fn camel_folder_thread_node_get_item(node: *mut c_void) -> *mut c_void;
    fn camel_folder_thread_node_get_child(node: *mut c_void) -> *mut c_void;
    fn camel_folder_thread_node_get_next(node: *mut c_void) -> *mut c_void;
}

#[cfg(not(camel_3_58))]
unsafe extern "C" {
    fn camel_folder_get_uids(folder: *mut ffi::CamelFolder) -> *mut glib::ffi::GPtrArray;
    fn camel_folder_free_uids(folder: *mut ffi::CamelFolder, array: *mut glib::ffi::GPtrArray);
    fn camel_folder_search_by_expression(
        folder: *mut ffi::CamelFolder,
        expression: *const c_char,
        cancellable: *mut gio::ffi::GCancellable,
        error: *mut *mut glib::ffi::GError,
    ) -> *mut glib::ffi::GPtrArray;
    fn camel_folder_search_free(folder: *mut ffi::CamelFolder, result: *mut glib::ffi::GPtrArray);
    fn camel_object_state_write(object: *mut c_void) -> i32;
    fn camel_object_get_state_filename(object: *mut c_void) -> *const c_char;
    fn camel_folder_thread_messages_new(
        folder: *mut ffi::CamelFolder,
        uids: *mut glib::ffi::GPtrArray,
        thread_subject: glib::ffi::gboolean,
    ) -> *mut Thread356;
    fn camel_folder_thread_messages_unref(thread: *mut Thread356);
}

/// `CamelFolderThreadNode` before 3.58 (camel-folder-thread.h).
#[cfg(not(camel_3_58))]
#[repr(C)]
struct Node356 {
    next: *mut Node356,
    parent: *mut Node356,
    child: *mut Node356,
    message: *const ffi::CamelMessageInfo,
    root_subject: *mut c_char,
    order_and_re: u32,
}

/// The leading fields of `CamelFolderThread` before 3.58; only `tree` is
/// read.
#[cfg(not(camel_3_58))]
#[repr(C)]
struct Thread356 {
    refcount_and_subject: u32,
    tree: *mut Node356,
}

/// Copies the strings of a `GPtrArray` of UIDs.
pub(crate) unsafe fn strings(array: *mut glib::ffi::GPtrArray) -> Vec<String> {
    if array.is_null() {
        return Vec::new();
    }
    unsafe {
        let len = (*array).len as usize;
        let data = (*array).pdata as *const *const c_char;
        (0..len)
            .filter_map(|i| {
                let p = *data.add(i);
                (!p.is_null()).then(|| glib::GStr::from_ptr(p).to_string())
            })
            .collect()
    }
}

pub(crate) unsafe fn uids(folder: *mut ffi::CamelFolder) -> Vec<String> {
    unsafe {
        #[cfg(camel_3_58)]
        {
            let array = camel_folder_dup_uids(folder);
            let out = strings(array);
            if !array.is_null() {
                glib::ffi::g_ptr_array_unref(array);
            }
            out
        }
        #[cfg(not(camel_3_58))]
        {
            let array = camel_folder_get_uids(folder);
            let out = strings(array);
            if !array.is_null() {
                camel_folder_free_uids(folder, array);
            }
            out
        }
    }
}

pub(crate) unsafe fn search(
    folder: *mut ffi::CamelFolder,
    expression: *const c_char,
    cancellable: *mut gio::ffi::GCancellable,
) -> Result<Vec<String>, glib::Error> {
    unsafe {
        let mut error = std::ptr::null_mut();
        #[cfg(camel_3_58)]
        {
            let mut array = std::ptr::null_mut();
            let ok = camel_folder_search_sync(folder, expression, &mut array, cancellable, &mut error);
            let out = strings(array);
            if !array.is_null() {
                glib::ffi::g_ptr_array_unref(array);
            }
            if ok == glib::ffi::GFALSE {
                return Err(from_glib_full(error));
            }
            Ok(out)
        }
        #[cfg(not(camel_3_58))]
        {
            // A search without matches may return NULL and no error.
            let array = camel_folder_search_by_expression(folder, expression, cancellable, &mut error);
            if !error.is_null() {
                if !array.is_null() {
                    camel_folder_search_free(folder, array);
                }
                return Err(from_glib_full(error));
            }
            let out = strings(array);
            if !array.is_null() {
                camel_folder_search_free(folder, array);
            }
            Ok(out)
        }
    }
}

pub(crate) unsafe fn save_state(folder: *mut ffi::CamelFolder) {
    unsafe {
        #[cfg(camel_3_58)]
        camel_folder_save_state(folder);
        #[cfg(not(camel_3_58))]
        camel_object_state_write(folder as *mut c_void);
    }
}

pub(crate) unsafe fn state_filename(folder: *mut ffi::CamelFolder) -> Option<String> {
    unsafe {
        #[cfg(camel_3_58)]
        let p = camel_folder_get_state_filename(folder);
        #[cfg(not(camel_3_58))]
        let p = camel_object_get_state_filename(folder as *mut c_void);
        (!p.is_null()).then(|| glib::GStr::from_ptr(p).to_string())
    }
}

/// A thread tree as nested message infos, roots in summary order. Nodes
/// without a message are dropped and their children promoted.
pub(crate) unsafe fn thread(folder: *mut ffi::CamelFolder) -> Vec<crate::ThreadNode> {
    unsafe {
        #[cfg(camel_3_58)]
        {
            // Flags 0: by Message-ID and References only, unsorted.
            let thread = camel_folder_thread_new(folder, std::ptr::null_mut(), 0);
            if thread.is_null() {
                return Vec::new();
            }
            let out = collect(camel_folder_thread_get_tree(thread));
            glib::gobject_ffi::g_object_unref(thread as *mut _);
            out
        }
        #[cfg(not(camel_3_58))]
        {
            let thread = camel_folder_thread_messages_new(folder, std::ptr::null_mut(), glib::ffi::GFALSE);
            if thread.is_null() {
                return Vec::new();
            }
            // The infos belong to the thread; collect() takes its own refs.
            let out = collect((*thread).tree);
            camel_folder_thread_messages_unref(thread);
            out
        }
    }
}

#[cfg(camel_3_58)]
unsafe fn collect(mut node: *mut c_void) -> Vec<crate::ThreadNode> {
    let mut out = Vec::new();
    while !node.is_null() {
        unsafe {
            let item = camel_folder_thread_node_get_item(node) as *mut ffi::CamelMessageInfo;
            push(&mut out, item, collect(camel_folder_thread_node_get_child(node)));
            node = camel_folder_thread_node_get_next(node);
        }
    }
    out
}

#[cfg(not(camel_3_58))]
unsafe fn collect(mut node: *mut Node356) -> Vec<crate::ThreadNode> {
    let mut out = Vec::new();
    while !node.is_null() {
        unsafe {
            let item = (*node).message as *mut ffi::CamelMessageInfo;
            push(&mut out, item, collect((*node).child));
            node = (*node).next;
        }
    }
    out
}

unsafe fn push(out: &mut Vec<crate::ThreadNode>, item: *mut ffi::CamelMessageInfo, children: Vec<crate::ThreadNode>) {
    if item.is_null() {
        out.extend(children);
    } else {
        out.push(crate::ThreadNode {
            info: unsafe { from_glib_none(item) },
            children,
        });
    }
}

/// Frees the `transferred_uids` array of `camel_folder_transfer_messages_to_sync()`
/// after copying it. Before 3.58 the caller frees the strings; from 3.58 the
/// array is returned with transfer container and its own free function.
pub(crate) unsafe fn take_transferred(array: *mut glib::ffi::GPtrArray) -> Vec<Option<String>> {
    if array.is_null() {
        return Vec::new();
    }
    unsafe {
        let len = (*array).len as usize;
        let data = (*array).pdata as *const *mut c_char;
        let out = (0..len)
            .map(|i| {
                let p = *data.add(i);
                (!p.is_null()).then(|| glib::GStr::from_ptr(p).to_string())
            })
            .collect();
        #[cfg(camel_3_58)]
        glib::ffi::g_ptr_array_unref(array);
        #[cfg(not(camel_3_58))]
        {
            for i in 0..len {
                glib::ffi::g_free(*data.add(i) as *mut c_void);
            }
            glib::ffi::g_ptr_array_free(array, glib::ffi::GTRUE);
        }
        out
    }
}
