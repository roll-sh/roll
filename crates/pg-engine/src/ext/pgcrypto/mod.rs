//! pgcrypto (`contrib/pgcrypto`, as built with OpenSSL 3): digests, HMAC, password hashing
//! (`crypt` / `gen_salt`), raw encryption and OpenPGP. The SQL-callable wrappers
//! (`pgcrypto.c`, `pgp-pgsql.c`) are here; the algorithms are in the submodules, ported from
//! PostgreSQL's so their results, errors and debug notices are the same.

mod bf_tables;
mod cipher;
mod crypt;
mod crypt_bf;
mod crypt_des;
mod digest;
mod pgp;
mod px;

use super::native::Native;
use crate::error::code;
use crate::sql::func::FnCtx;
use crate::types::Value;
use crate::{Error, Result};
use px::{EXTERNAL_ROUTINE, PxErr, strerror};
use std::sync::Arc;

pub(in crate::ext) fn symbol(sym: &str) -> Option<Native> {
    Some(match sym {
        "pg_digest" => Native::scalar(pg_digest),
        "pg_hmac" => Native::scalar(pg_hmac),
        "pg_crypt" => Native::scalar(pg_crypt),
        "pg_gen_salt" => Native::scalar(pg_gen_salt),
        "pg_gen_salt_rounds" => Native::scalar(pg_gen_salt),
        "pg_encrypt" => Native::scalar(pg_encrypt),
        "pg_decrypt" => Native::scalar(pg_decrypt),
        "pg_encrypt_iv" => Native::scalar(pg_encrypt_iv),
        "pg_decrypt_iv" => Native::scalar(pg_decrypt_iv),
        "pg_random_bytes" => Native::scalar(pg_random_bytes),
        "pg_random_uuid" => Native::scalar(pg_random_uuid),
        "pg_check_fipsmode" => Native::scalar(|_, _| Ok(Value::Bool(false))),
        "pgp_sym_encrypt_text" => Native::scalar(pgp::sym_encrypt_text),
        "pgp_sym_encrypt_bytea" => Native::scalar(pgp::sym_encrypt_bytea),
        "pgp_sym_decrypt_text" => Native::scalar(pgp::sym_decrypt_text),
        "pgp_sym_decrypt_bytea" => Native::scalar(pgp::sym_decrypt_bytea),
        "pgp_pub_encrypt_text" => Native::scalar(pgp::pub_encrypt_text),
        "pgp_pub_encrypt_bytea" => Native::scalar(pgp::pub_encrypt_bytea),
        "pgp_pub_decrypt_text" => Native::scalar(pgp::pub_decrypt_text),
        "pgp_pub_decrypt_bytea" => Native::scalar(pgp::pub_decrypt_bytea),
        "pgp_key_id_w" => Native::scalar(pgp::key_id),
        "pg_armor" => Native::scalar(pgp::armor),
        "pg_dearmor" => Native::scalar(pgp::dearmor),
        "pgp_armor_headers" => Native::set(pgp::armor_headers),
        _ => return None,
    })
}

/// A text or bytea argument's bytes.
pub(super) fn bytes(v: &Value) -> &[u8] {
    match v {
        Value::Text(s) => s.as_bytes(),
        Value::Bytea(b) => b,
        _ => &[],
    }
}

pub(super) fn bytea(b: Vec<u8>) -> Value {
    Value::Bytea(Arc::from(b))
}

/// Text made of bytes the functions built (ASCII, or checked UTF-8).
pub(super) fn text(b: Vec<u8>) -> Value {
    Value::Text(Arc::from(String::from_utf8_lossy(&b).into_owned()))
}

fn int(v: &Value) -> i32 {
    crate::sql::func::int_arg(v) as i32
}

/// `downcase_truncate_identifier`: ASCII letters lowercased, cut to NAMEDATALEN - 1 bytes.
fn downcase_name(v: &Value) -> String {
    let s = String::from_utf8_lossy(bytes(v)).to_ascii_lowercase();
    let mut n = s.len().min(63);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s[..n].to_string()
}

/// `find_provider`'s error.
fn cannot_use(name: &str, e: PxErr) -> Error {
    match e {
        PxErr::Code(c) => Error::new(code::INVALID_PARAMETER_VALUE, format!("Cannot use \"{name}\": {}", strerror(c))),
        PxErr::Raised(e) => e,
    }
}

fn raised(e: PxErr) -> Error {
    px::thrown(e)
}

fn pg_digest(a: &[Value], _: &FnCtx) -> Result<Value> {
    let name = downcase_name(&a[1]);
    let mut md = digest::Md::find(&name).map_err(|e| cannot_use(&name, e))?;
    md.result_size().map_err(raised)?;
    md.update(bytes(&a[0]));
    Ok(bytea(md.finish()))
}

fn pg_hmac(a: &[Value], _: &FnCtx) -> Result<Value> {
    let name = downcase_name(&a[2]);
    let mut h = digest::Hmac::find(&name).map_err(|e| cannot_use(&name, e))?;
    h.result_size().map_err(raised)?;
    h.init(bytes(&a[1]));
    h.update(bytes(&a[0]));
    Ok(bytea(h.finish()))
}

/// `CheckBuiltinCryptoMode` (pgcrypto.builtin_crypto_enabled; OpenSSL isn't in FIPS mode).
fn check_builtin_crypto(c: &FnCtx) -> Result<()> {
    let v = c.exec.sess.settings.borrow().get_raw("pgcrypto.builtin_crypto_enabled");
    if v.is_some_and(|v| v.eq_ignore_ascii_case("off")) {
        return Err(Error::new(code::INTERNAL_ERROR, "use of built-in crypto functions is disabled"));
    }
    Ok(())
}

fn pg_crypt(a: &[Value], c: &FnCtx) -> Result<Value> {
    check_builtin_crypto(c)?;
    let mut notices = Vec::new();
    let r = crypt::px_crypt(bytes(&a[0]), bytes(&a[1]), &mut |e| notices.push(e));
    for n in notices {
        c.exec.notice(n);
    }
    match r? {
        Some(res) => Ok(text(res)),
        None => Err(Error::new(EXTERNAL_ROUTINE, "crypt(3) returned NULL")),
    }
}

/// gen_salt(type) and gen_salt(type, rounds).
fn pg_gen_salt(a: &[Value], c: &FnCtx) -> Result<Value> {
    // text_to_cstring_buffer into PX_MAX_SALT_LEN + 1 bytes.
    let t = bytes(&a[0]);
    let t = &t[..t.len().min(128)];
    let rounds = a.get(1).map_or(0, int);
    check_builtin_crypto(c)?;
    match crypt::gen_salt(t, rounds, &mut |b| c.exec.random_bytes(b)) {
        Ok(s) => Ok(text(s)),
        Err(e) => Err(Error::new(code::INVALID_PARAMETER_VALUE, format!("gen_salt: {}", strerror(e)))),
    }
}

/// encrypt / decrypt / encrypt_iv / decrypt_iv.
fn combo(a: &[Value], encrypt: bool, iv: bool) -> Result<Value> {
    let name = downcase_name(&a[if iv { 3 } else { 2 }]);
    let mut cx = cipher::Combo::find(&name).map_err(|e| cannot_use(&name, e))?;
    let what = match (encrypt, iv) {
        (true, false) => "encrypt",
        (false, false) => "decrypt",
        (true, true) => "encrypt_iv",
        (false, true) => "decrypt_iv",
    };
    let r = cx
        .init(bytes(&a[1]), iv.then(|| bytes(&a[2])))
        .and_then(|_| if encrypt { cx.encrypt(bytes(&a[0])) } else { cx.decrypt(bytes(&a[0])) });
    match r {
        Ok(b) => Ok(bytea(b)),
        Err(PxErr::Code(e)) => Err(Error::new(EXTERNAL_ROUTINE, format!("{what} error: {}", strerror(e)))),
        Err(PxErr::Raised(e)) => Err(e),
    }
}

fn pg_encrypt(a: &[Value], _: &FnCtx) -> Result<Value> {
    combo(a, true, false)
}

fn pg_decrypt(a: &[Value], _: &FnCtx) -> Result<Value> {
    combo(a, false, false)
}

fn pg_encrypt_iv(a: &[Value], _: &FnCtx) -> Result<Value> {
    combo(a, true, true)
}

fn pg_decrypt_iv(a: &[Value], _: &FnCtx) -> Result<Value> {
    combo(a, false, true)
}

fn pg_random_bytes(a: &[Value], c: &FnCtx) -> Result<Value> {
    let len = int(&a[0]);
    if !(1..=1024).contains(&len) {
        return Err(Error::new(EXTERNAL_ROUTINE, "Length not in range"));
    }
    let mut b = vec![0u8; len as usize];
    c.exec.random_bytes(&mut b);
    Ok(bytea(b))
}

/// gen_random_uuid(): the built-in one.
fn pg_random_uuid(_: &[Value], c: &FnCtx) -> Result<Value> {
    let mut b = [0u8; 16];
    c.exec.random_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(Value::Uuid(u128::from_be_bytes(b)))
}
