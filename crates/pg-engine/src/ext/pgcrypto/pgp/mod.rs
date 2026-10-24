//! OpenPGP encryption (`pgp.c`, `pgp-pgsql.c`): the context with its options, and the SQL
//! functions.

mod armor;
mod cfb;
mod compress;
mod decrypt;
mod encrypt;
mod filters;
mod info;
mod mbuf;
mod mpi;
mod options;
mod packet;
mod pubenc;
mod pubkey;
mod s2k;
mod sql;
#[cfg(test)]
mod tests;

pub(super) use sql::*;

use super::digest::Md;
use std::cell::RefCell;

/// `PGP_Context`: the options, what decryption found, and the keys.
struct Pgp {
    s2k: s2k::S2k,
    s2k_mode: i32,
    s2k_count: i32,
    s2k_digest_algo: i32,
    s2k_cipher_algo: i32,
    cipher_algo: i32,
    compress_algo: i32,
    compress_level: i32,
    disable_mdc: bool,
    use_sess_key: bool,
    text_mode: bool,
    convert_crlf: bool,
    unicode_mode: bool,
    ignore_cipher_failure: bool,
    mdc_checked: bool,
    corrupt_prefix: bool,
    unsupported_compr: bool,
    unexpected_binary: bool,
    in_mdc_pkt: bool,
    use_mdcbuf_filter: bool,
    mdc_ctx: Option<Md>,
    pub_key: Option<pubkey::PubKey>,
    sym_key: Option<Vec<u8>>,
    sess_key: Vec<u8>,
}

impl Default for Pgp {
    /// `pgp_init`'s defaults.
    fn default() -> Pgp {
        Pgp {
            s2k: s2k::S2k::default(),
            s2k_mode: 3,
            s2k_count: -1,
            s2k_digest_algo: 2,
            s2k_cipher_algo: -1,
            cipher_algo: 7,
            compress_algo: 0,
            compress_level: 6,
            disable_mdc: false,
            use_sess_key: false,
            text_mode: false,
            convert_crlf: false,
            unicode_mode: false,
            ignore_cipher_failure: false,
            mdc_checked: false,
            corrupt_prefix: false,
            unsupported_compr: false,
            unexpected_binary: false,
            in_mdc_pkt: false,
            use_mdcbuf_filter: false,
            mdc_ctx: None,
            pub_key: None,
            sym_key: None,
            sess_key: Vec::new(),
        }
    }
}

thread_local! {
    /// The `px_debug` messages of the running call, when its `debug` option is on.
    static DEBUG: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// `px_debug`: shown as `NOTICE: dbg: ...` with the `debug=1` option.
pub(super) fn debug(msg: String) {
    DEBUG.with(|d| {
        if let Some(v) = d.borrow_mut().as_mut() {
            v.push(msg);
        }
    });
}

/// Collects the debug messages while it lives (`px_set_debug_handler`).
pub(super) struct DebugGuard;

impl DebugGuard {
    pub fn enter(on: bool) -> DebugGuard {
        DEBUG.with(|d| *d.borrow_mut() = on.then(Vec::new));
        DebugGuard
    }

    pub fn take(&self) -> Vec<String> {
        DEBUG.with(|d| d.borrow_mut().as_mut().map(std::mem::take).unwrap_or_default())
    }
}

impl Drop for DebugGuard {
    fn drop(&mut self) {
        DEBUG.with(|d| *d.borrow_mut() = None);
    }
}

/// `pg_strong_random` (the operating system's generator; the server reports a panic as an
/// error, should it ever fail).
pub(super) fn random(buf: &mut [u8]) {
    getrandom::fill(buf).expect("could not generate a random number");
}
