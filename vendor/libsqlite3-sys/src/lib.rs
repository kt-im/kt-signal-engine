#![expect(non_snake_case, non_camel_case_types)]
#![cfg_attr(not(test), no_std)]
// force linking to openssl
#[cfg(feature = "bundled-sqlcipher-vendored-openssl")]
extern crate openssl_sys;

pub use self::error::*;

use core::mem;

mod error;

#[must_use]
pub fn SQLITE_STATIC() -> sqlite3_destructor_type {
    None
}

#[must_use]
pub fn SQLITE_TRANSIENT() -> sqlite3_destructor_type {
    Some(unsafe { mem::transmute::<isize, unsafe extern "C" fn(*mut core::ffi::c_void)>(-1_isize) })
}

#[allow(dead_code, clippy::all)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindgen.rs"));
}
pub use bindings::*;

impl Default for sqlite3_vtab {
    fn default() -> Self {
        unsafe { mem::zeroed() }
    }
}

impl Default for sqlite3_vtab_cursor {
    fn default() -> Self {
        unsafe { mem::zeroed() }
    }
}

/// SQLCipher custom crypto provider (KT, 2026-11-19).
///
/// Shipped inside this crate on purpose: SQLCipher's C code references
/// `kt_sqlcipher_provider_setup` (see `SQLCIPHER_CRYPTO_CUSTOM`), so every
/// artifact linking libsqlite3-sys must resolve that symbol -- main bin,
/// side bins and integration tests alike. Implementing it here means nobody
/// has to re-provide it.
#[cfg(feature = "bundled-sqlcipher-custom")]
pub mod provider;
