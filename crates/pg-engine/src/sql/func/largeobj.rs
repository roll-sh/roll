//! The server-side large object functions (Postgres's be-fsstubs.c): lo_creat, lo_open,
//! loread, lowrite, lo_lseek, lo_get, lo_put, ….

use super::{FnCtx, Registry};
use crate::error::{Error, Result, code};
use crate::largeobj::{self, Desc, INV_READ, INV_WRITE, Snap};
use crate::types::{DataType, Value};

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn oid_arg(v: &Value) -> u32 {
    int(v) as u32
}

fn bytes(v: &Value) -> &[u8] {
    match v {
        Value::Bytea(b) => b,
        _ => &[],
    }
}

/// PreventCommandIfReadOnly.
fn prevent_read_only(c: &FnCtx, what: &str) -> Result<()> {
    if c.exec.txn.borrow().read_only {
        return Err(Error::new(
            code::READ_ONLY_SQL_TRANSACTION,
            format!("cannot execute {what} in a read-only transaction"),
        ));
    }
    Ok(())
}

/// lo_compat_privileges: skip the privilege checks.
fn compat(c: &FnCtx) -> bool {
    c.exec
        .sess
        .settings
        .borrow()
        .get_raw("lo_compat_privileges")
        .as_deref()
        == Some("on")
}

/// inv_open as the current user.
fn open(c: &FnCtx, oid: u32, mode: i32) -> Result<Desc> {
    let cat = c.exec.catalog();
    let (me, db_owner) = (c.exec.current_user(), c.exec.db_owner());
    let compat = compat(c);
    largeobj::open(&c.exec.txn.borrow(), &cat, oid, mode, me, db_owner, compat)
}

/// Run `f` on descriptor `fd`, keeping its new offset.
fn with_desc<T>(
    c: &FnCtx,
    fd: i32,
    f: impl FnOnce(&mut Desc, &mut crate::txn::Txn) -> Result<T>,
) -> Result<T> {
    let mut txn = c.exec.txn.borrow_mut();
    let mut d = txn.lo_desc(fd)?.clone();
    let r = f(&mut d, &mut txn);
    *txn.lo_desc(fd)? = d;
    r
}

fn not_opened(fd: i32, what: &str) -> Error {
    Error::new(
        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("large object descriptor {fd} was not opened for {what}"),
    )
}

fn lo_open(a: &[Value], c: &FnCtx) -> Result<Value> {
    let mode = int(&a[1]) as i32;
    if mode & INV_WRITE != 0 {
        prevent_read_only(c, "lo_open(INV_WRITE)")?;
    }
    let d = open(c, oid_arg(&a[0]), mode)?;
    Ok(Value::Int4(c.exec.txn.borrow_mut().lo_add(d)))
}

fn lo_close(a: &[Value], c: &FnCtx) -> Result<Value> {
    c.exec.txn.borrow_mut().lo_close(int(&a[0]) as i32)?;
    Ok(Value::Int4(0))
}

fn loread(a: &[Value], c: &FnCtx) -> Result<Value> {
    let fd = int(&a[0]) as i32;
    let len = int(&a[1]).max(0);
    with_desc(c, fd, |d, txn| {
        if !d.read {
            return Err(not_opened(fd, "reading"));
        }
        Ok(Value::Bytea(d.read(txn, len)?.into()))
    })
}

fn lowrite(a: &[Value], c: &FnCtx) -> Result<Value> {
    prevent_read_only(c, "lowrite()")?;
    let fd = int(&a[0]) as i32;
    with_desc(c, fd, |d, txn| {
        if !d.write {
            return Err(not_opened(fd, "writing"));
        }
        Ok(Value::Int4(d.write(txn, bytes(&a[1]))?))
    })
}

fn seek(a: &[Value], c: &FnCtx) -> Result<i64> {
    let fd = int(&a[0]) as i32;
    with_desc(c, fd, |d, txn| d.seek(txn, int(&a[1]), int(&a[2]) as i32))
}

fn out_of_range(what: &str, fd: &Value) -> Error {
    Error::new(
        code::NUMERIC_VALUE_OUT_OF_RANGE,
        format!(
            "{what} result out of range for large-object descriptor {}",
            int(fd)
        ),
    )
}

fn lo_lseek(a: &[Value], c: &FnCtx) -> Result<Value> {
    let n = seek(a, c)?;
    i32::try_from(n)
        .map(Value::Int4)
        .map_err(|_| out_of_range("lo_lseek", &a[0]))
}

fn lo_lseek64(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Int8(seek(a, c)?))
}

fn tell(a: &[Value], c: &FnCtx) -> Result<i64> {
    Ok(c.exec.txn.borrow_mut().lo_desc(int(&a[0]) as i32)?.offset)
}

fn lo_tell(a: &[Value], c: &FnCtx) -> Result<Value> {
    let n = tell(a, c)?;
    i32::try_from(n)
        .map(Value::Int4)
        .map_err(|_| out_of_range("lo_tell", &a[0]))
}

fn lo_tell64(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Int8(tell(a, c)?))
}

fn truncate(a: &[Value], c: &FnCtx) -> Result<Value> {
    let fd = int(&a[0]) as i32;
    with_desc(c, fd, |d, txn| {
        if !d.write {
            return Err(not_opened(fd, "writing"));
        }
        d.truncate(txn, int(&a[1]))?;
        Ok(Value::Int4(0))
    })
}

fn lo_truncate(a: &[Value], c: &FnCtx) -> Result<Value> {
    prevent_read_only(c, "lo_truncate()")?;
    truncate(a, c)
}

fn lo_truncate64(a: &[Value], c: &FnCtx) -> Result<Value> {
    prevent_read_only(c, "lo_truncate64()")?;
    truncate(a, c)
}

/// inv_create as the current user.
fn create(c: &FnCtx, oid: u32) -> Result<u32> {
    let cat = c.exec.catalog();
    let me = c.exec.current_user();
    largeobj::create(&mut c.exec.txn.borrow_mut(), &cat, oid, me)
}

fn lo_creat(_: &[Value], c: &FnCtx) -> Result<Value> {
    prevent_read_only(c, "lo_creat()")?;
    Ok(Value::Oid(create(c, 0)?))
}

fn lo_create(a: &[Value], c: &FnCtx) -> Result<Value> {
    prevent_read_only(c, "lo_create()")?;
    Ok(Value::Oid(create(c, oid_arg(&a[0]))?))
}

fn lo_unlink(a: &[Value], c: &FnCtx) -> Result<Value> {
    prevent_read_only(c, "lo_unlink()")?;
    let oid = oid_arg(&a[0]);
    let cat = c.exec.catalog();
    let (me, db_owner) = (c.exec.current_user(), c.exec.db_owner());
    let compat = compat(c);
    let mut txn = c.exec.txn.borrow_mut();
    let m = largeobj::existing(&txn, oid, Snap::Latest)?;
    if !compat && !cat.has_privs_of_role(me, m.owner, db_owner) {
        return Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("must be owner of large object {oid}"),
        ));
    }
    txn.lo_close_all(oid);
    largeobj::drop(&mut txn, oid)?;
    Ok(Value::Int4(1))
}

/// lo_get_fragment_internal: `nbytes` (-1: all) from `offset`.
fn fragment(c: &FnCtx, oid: u32, offset: i64, nbytes: i64) -> Result<Value> {
    let mut d = open(c, oid, INV_READ)?;
    let txn = c.exec.txn.borrow();
    let size = d.seek(&txn, 0, 2)?;
    let len = if size > offset {
        if nbytes >= 0 && nbytes <= size - offset {
            nbytes
        } else {
            size - offset
        }
    } else {
        0
    };
    // MaxAllocSize - VARHDRSZ.
    if len > 0x3fff_ffff - 4 {
        return Err(Error::new(
            code::PROGRAM_LIMIT_EXCEEDED,
            "large object read request is too large",
        ));
    }
    d.seek(&txn, offset, 0)?;
    Ok(Value::Bytea(d.read(&txn, len)?.into()))
}

fn lo_get(a: &[Value], c: &FnCtx) -> Result<Value> {
    fragment(c, oid_arg(&a[0]), 0, -1)
}

fn lo_get_fragment(a: &[Value], c: &FnCtx) -> Result<Value> {
    let n = int(&a[2]);
    if n < 0 {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "requested length cannot be negative",
        ));
    }
    fragment(c, oid_arg(&a[0]), int(&a[1]), n)
}

fn lo_from_bytea(a: &[Value], c: &FnCtx) -> Result<Value> {
    prevent_read_only(c, "lo_from_bytea()")?;
    let oid = create(c, oid_arg(&a[0]))?;
    let mut d = open(c, oid, INV_WRITE)?;
    d.write(&mut c.exec.txn.borrow_mut(), bytes(&a[1]))?;
    Ok(Value::Oid(oid))
}

fn lo_put(a: &[Value], c: &FnCtx) -> Result<Value> {
    prevent_read_only(c, "lo_put()")?;
    let mut d = open(c, oid_arg(&a[0]), INV_WRITE)?;
    let mut txn = c.exec.txn.borrow_mut();
    d.seek(&txn, int(&a[1]), 0)?;
    d.write(&mut txn, bytes(&a[2]))?;
    Ok(Value::Null)
}

/// Server-side files aren't reachable here (as COPY to or from a file isn't), for any role:
/// the error is the one Postgres gives a role without EXECUTE on the function.
fn server_files(what: &str) -> Error {
    Error::new(
        code::INSUFFICIENT_PRIVILEGE,
        format!("permission denied for function {what}"),
    )
}

fn lo_import(_: &[Value], c: &FnCtx) -> Result<Value> {
    prevent_read_only(c, "lo_import()")?;
    Err(server_files("lo_import"))
}

fn lo_export(a: &[Value], c: &FnCtx) -> Result<Value> {
    // The object is opened (and checked) before the file.
    open(c, oid_arg(&a[0]), INV_READ)?;
    Err(server_files("lo_export"))
}

pub fn register(r: &mut Registry) {
    use DataType::*;
    r.func("lo_open", vec![Oid, Int4], Int4, lo_open).volatile();
    r.func("lo_close", vec![Int4], Int4, lo_close).volatile();
    r.func("loread", vec![Int4, Int4], Bytea, loread).volatile();
    r.func("lowrite", vec![Int4, Bytea], Int4, lowrite)
        .volatile();
    r.func("lo_lseek", vec![Int4, Int4, Int4], Int4, lo_lseek)
        .volatile();
    r.func("lo_lseek64", vec![Int4, Int8, Int4], Int8, lo_lseek64)
        .volatile();
    r.func("lo_tell", vec![Int4], Int4, lo_tell).volatile();
    r.func("lo_tell64", vec![Int4], Int8, lo_tell64).volatile();
    r.func("lo_truncate", vec![Int4, Int4], Int4, lo_truncate)
        .volatile();
    r.func("lo_truncate64", vec![Int4, Int8], Int4, lo_truncate64)
        .volatile();
    r.func("lo_creat", vec![Int4], Oid, lo_creat).volatile();
    r.func("lo_create", vec![Oid], Oid, lo_create).volatile();
    r.func("lo_unlink", vec![Oid], Int4, lo_unlink).volatile();
    r.func("lo_get", vec![Oid], Bytea, lo_get).volatile();
    r.func("lo_get", vec![Oid, Int8, Int4], Bytea, lo_get_fragment)
        .volatile();
    r.func("lo_put", vec![Oid, Int8, Bytea], Void, lo_put)
        .volatile();
    r.func("lo_from_bytea", vec![Oid, Bytea], Oid, lo_from_bytea)
        .volatile();
    r.func("lo_import", vec![Text], Oid, lo_import).volatile();
    r.func("lo_import", vec![Text, Oid], Oid, lo_import)
        .volatile();
    r.func("lo_export", vec![Oid, Text], Int4, lo_export)
        .volatile();
}
