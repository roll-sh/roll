//! The pgp_* SQL functions (`pgp-pgsql.c`).

use super::super::px::{self, PXE_ARGUMENT_ERROR, Px, PxErr};
use super::super::{bytea, bytes, text};
use super::options::{Expect, check_expect, parse_args};
use super::{DebugGuard, Pgp, decrypt, encrypt, info, pubkey};
use crate::error::{Severity, code};
use crate::sql::func::{FnCtx, SrfIter};
use crate::types::Value;
use crate::{Error, Result};

fn notice(c: &FnCtx, msg: String) {
    let mut e = Error::new("00000", msg);
    e.severity = Severity::Notice;
    c.exec.notice(e);
}

/// What a call ran into: its debug messages (and expect- notices), then the result.
fn finish<T>(c: &FnCtx, dbg: &DebugGuard, extra: Vec<String>, r: Px<T>) -> Result<T> {
    for m in dbg.take() {
        notice(c, format!("dbg: {m}"));
    }
    for m in extra {
        notice(c, m);
    }
    r.map_err(px::thrown)
}

/// `init_work`: the context with the call's options.
fn init_work(text_mode: bool, args: Option<&Value>) -> Result<(Pgp, Expect)> {
    let mut ctx = Pgp::default();
    let mut ex = Expect::default();
    if let Some(a) = args {
        parse_args(&mut ctx, bytes(a), &mut ex).map_err(px::thrown)?;
    }
    ctx.text_mode = text_mode;
    Ok((ctx, ex))
}

fn encrypt_internal(c: &FnCtx, pubenc: bool, text_mode: bool, a: &[Value]) -> Result<Value> {
    let (mut ctx, ex) = init_work(text_mode, a.get(2))?;
    let dbg = DebugGuard::enter(ex.debug);
    let key = bytes(&a[1]);
    let r = (|| {
        if pubenc {
            pubkey::set_pubkey(&mut ctx, key, None, 0)?;
        } else {
            if key.is_empty() {
                return Err(PxErr::Code(PXE_ARGUMENT_ERROR));
            }
            ctx.sym_key = Some(key.to_vec());
        }
        encrypt::encrypt(&mut ctx, bytes(&a[0]))
    })();
    finish(c, &dbg, Vec::new(), r).map(bytea)
}

fn decrypt_internal(c: &FnCtx, pubenc: bool, need_text: bool, a: &[Value]) -> Result<Value> {
    let args = if pubenc { a.get(3) } else { a.get(2) };
    let (mut ctx, ex) = init_work(need_text, args)?;
    let dbg = DebugGuard::enter(ex.debug);
    let key = bytes(&a[1]);
    let set = if pubenc {
        let psw = a.get(2).map(bytes);
        pubkey::set_pubkey(&mut ctx, key, psw, 1)
    } else if key.is_empty() {
        Err(PxErr::Code(PXE_ARGUMENT_ERROR))
    } else {
        ctx.sym_key = Some(key.to_vec());
        Ok(())
    };
    let mut extra = Vec::new();
    let r = set.and_then(|_| {
        let r = decrypt::decrypt(&mut ctx, bytes(&a[0]));
        if ex.expect {
            extra = check_expect(&ctx, &ex);
        }
        r
    });
    let res = finish(c, &dbg, extra, r)?;
    if need_text {
        crate::types::mbconv::verify(crate::types::mbconv::UTF8, &res)?;
        return Ok(text(res));
    }
    Ok(bytea(res))
}

pub(in crate::ext::pgcrypto) fn sym_encrypt_text(a: &[Value], c: &FnCtx) -> Result<Value> {
    encrypt_internal(c, false, true, a)
}

pub(in crate::ext::pgcrypto) fn sym_encrypt_bytea(a: &[Value], c: &FnCtx) -> Result<Value> {
    encrypt_internal(c, false, false, a)
}

pub(in crate::ext::pgcrypto) fn sym_decrypt_text(a: &[Value], c: &FnCtx) -> Result<Value> {
    decrypt_internal(c, false, true, a)
}

pub(in crate::ext::pgcrypto) fn sym_decrypt_bytea(a: &[Value], c: &FnCtx) -> Result<Value> {
    decrypt_internal(c, false, false, a)
}

pub(in crate::ext::pgcrypto) fn pub_encrypt_text(a: &[Value], c: &FnCtx) -> Result<Value> {
    encrypt_internal(c, true, true, a)
}

pub(in crate::ext::pgcrypto) fn pub_encrypt_bytea(a: &[Value], c: &FnCtx) -> Result<Value> {
    encrypt_internal(c, true, false, a)
}

pub(in crate::ext::pgcrypto) fn pub_decrypt_text(a: &[Value], c: &FnCtx) -> Result<Value> {
    decrypt_internal(c, true, true, a)
}

pub(in crate::ext::pgcrypto) fn pub_decrypt_bytea(a: &[Value], c: &FnCtx) -> Result<Value> {
    decrypt_internal(c, true, false, a)
}

pub(in crate::ext::pgcrypto) fn key_id(a: &[Value], _: &FnCtx) -> Result<Value> {
    info::key_id(bytes(&a[0])).map(|s| text(s.into_bytes())).map_err(px::thrown)
}

/// `parse_key_value_arrays`: armor()'s header keys and values.
fn header_arrays(k: &Value, v: &Value) -> Result<Vec<(String, String)>> {
    let (Value::Array(k), Value::Array(v)) = (k, v) else {
        return Ok(Vec::new());
    };
    if k.dims.len() > 1 || k.dims.len() != v.dims.len() {
        return Err(Error::new(code::ARRAY_SUBSCRIPT_ERROR, "wrong number of array subscripts"));
    }
    if k.dims.is_empty() {
        return Ok(Vec::new());
    }
    if k.items.len() != v.items.len() {
        return Err(Error::new(code::ARRAY_SUBSCRIPT_ERROR, "mismatched array dimensions"));
    }
    let bad = |m: &str| Error::new(code::INVALID_PARAMETER_VALUE, m.to_string());
    let mut out = Vec::new();
    for (kv, vv) in k.items.iter().zip(&v.items) {
        let Value::Text(key) = kv else {
            return Err(Error::new(code::NULL_VALUE_NOT_ALLOWED, "null value not allowed for header key"));
        };
        if !key.is_ascii() {
            return Err(bad("header key must not contain non-ASCII characters"));
        }
        if key.contains(": ") {
            return Err(bad("header key must not contain \": \""));
        }
        if key.contains('\n') {
            return Err(bad("header key must not contain newlines"));
        }
        let Value::Text(val) = vv else {
            return Err(Error::new(code::NULL_VALUE_NOT_ALLOWED, "null value not allowed for header value"));
        };
        if !val.is_ascii() {
            return Err(bad("header value must not contain non-ASCII characters"));
        }
        if val.contains('\n') {
            return Err(bad("header value must not contain newlines"));
        }
        out.push((key.to_string(), val.to_string()));
    }
    Ok(out)
}

pub(in crate::ext::pgcrypto) fn armor(a: &[Value], _: &FnCtx) -> Result<Value> {
    let headers = if a.len() == 3 { header_arrays(&a[1], &a[2])? } else { Vec::new() };
    Ok(text(super::armor::encode(bytes(&a[0]), &headers)))
}

pub(in crate::ext::pgcrypto) fn dearmor(a: &[Value], _: &FnCtx) -> Result<Value> {
    super::armor::decode(bytes(&a[0])).map(bytea).map_err(px::thrown)
}

pub(in crate::ext::pgcrypto) fn armor_headers(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    let hs = super::armor::headers(bytes(&a[0])).map_err(px::thrown)?;
    let mut rows = Vec::with_capacity(hs.len());
    for (k, v) in hs {
        crate::types::mbconv::verify(crate::types::mbconv::UTF8, &k)?;
        crate::types::mbconv::verify(crate::types::mbconv::UTF8, &v)?;
        rows.push(Ok(vec![text(k), text(v)]));
    }
    Ok(Box::new(rows.into_iter()))
}
