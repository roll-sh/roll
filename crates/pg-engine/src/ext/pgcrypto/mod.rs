//! pgcrypto (`contrib/pgcrypto`, as built with OpenSSL): digests, HMAC, password hashing
//! (`crypt` / `gen_salt`), raw encryption and OpenPGP.

use super::native::Native;

pub(in crate::ext) fn symbol(sym: &str) -> Option<Native> {
    let _ = sym;
    None
}
