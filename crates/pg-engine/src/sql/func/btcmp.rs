//! The btree comparison support functions (btint4cmp, bttextcmp, date_cmp, …): negative,
//! zero or positive as the first argument sorts before, with or after the second. Most return
//! -1/0/1; those Postgres implements with memcmp return the first differing bytes'
//! difference, as its C library does. And the hash support functions (hashint4,
//! hashtextextended, …).

use super::{FnCtx, Registry, resolve_binop};
use crate::error::Result;
use crate::sql::expr::Expr;
use crate::types::json::varstr_cmp;
use crate::types::{DataType, Value};

/// By the types' own `<` and `>` operators.
fn by_operators(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (l, r) = (c.arg_type(0), c.arg_type(1));
    let op = |name: &str, x: &Value, y: &Value| -> Result<bool> {
        let res = resolve_binop(name, &l, &r)?;
        let fmt = c.exec.fmt();
        let x = crate::types::cast::cast_value(x.clone(), &l, &res.arg_types[0], fmt, false)?;
        let y = crate::types::cast::cast_value(y.clone(), &r, &res.arg_types[1], fmt, false)?;
        let args = [
            Expr::Const(Value::Null, res.arg_types[0].clone()),
            Expr::Const(Value::Null, res.arg_types[1].clone()),
        ];
        let fc = FnCtx {
            exec: c.exec,
            ret: &DataType::Bool,
            args: &args,
            coll: c.coll,
        };
        Ok((res.def.imp)(&[x, y], &fc)? == Value::Bool(true))
    };
    Ok(Value::Int4(if op("<", &a[0], &a[1])? {
        -1
    } else if op(">", &a[0], &a[1])? {
        1
    } else {
        0
    }))
}

fn str_of(v: &Value) -> &[u8] {
    match v {
        Value::Text(t) => t.as_bytes(),
        Value::Bytea(b) => b,
        _ => &[],
    }
}

/// varstr_cmp (texts in the C collation, bytea, lexemes).
fn memcmp_style(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(varstr_cmp(str_of(&a[0]), str_of(&a[1]))))
}

/// varstr_cmp under a non-C collation: the collator's -1/0/1, ties between deterministically
/// collated strings broken as strcmp does.
fn coll_cmp(coll: &crate::types::collation::Collation, x: &str, y: &str) -> i32 {
    match coll.strcoll(x, y) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Equal if coll.deterministic => varstr_cmp(x.as_bytes(), y.as_bytes()),
        std::cmp::Ordering::Equal => 0,
    }
}

/// bttextcmp & co.: varstr_cmp under the input collation.
fn text_cmp(a: &[Value], c: &FnCtx) -> Result<Value> {
    match c.coll {
        Some(coll) if !coll.bytewise() => Ok(Value::Int4(coll_cmp(
            coll,
            super::text_arg(&a[0]),
            super::text_arg(&a[1]),
        ))),
        _ => memcmp_style(a, c),
    }
}

/// bpchar: trailing spaces don't count.
fn bpchar(a: &[Value], c: &FnCtx) -> Result<Value> {
    let trim = |v: &Value| match v {
        Value::Text(t) => t.trim_end_matches(' ').to_string(),
        _ => String::new(),
    };
    if let Some(coll) = c.coll.filter(|k| !k.bytewise()) {
        return Ok(Value::Int4(coll_cmp(coll, &trim(&a[0]), &trim(&a[1]))));
    }
    Ok(Value::Int4(varstr_cmp(trim(&a[0]).as_bytes(), trim(&a[1]).as_bytes())))
}

/// bpchar's pattern ordering: bytes, whatever the collation.
fn bpchar_bytes(a: &[Value], c: &FnCtx) -> Result<Value> {
    let c = FnCtx { coll: None, ..*c };
    bpchar(a, &c)
}

/// name in its own (C) collation: strncmp over NAMEDATALEN, so the terminating NUL counts.
fn name(a: &[Value], c: &FnCtx) -> Result<Value> {
    if let Some(coll) = c.coll.filter(|k| !k.bytewise()) {
        return Ok(Value::Int4(coll_cmp(coll, super::text_arg(&a[0]), super::text_arg(&a[1]))));
    }
    let (x, y) = (str_of(&a[0]), str_of(&a[1]));
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0) as i32;
    for i in 0..x.len().max(y.len()).min(64) {
        if at(x, i) != at(y, i) {
            return Ok(Value::Int4(at(x, i) - at(y, i)));
        }
    }
    Ok(Value::Int4(0))
}

/// int2s fit an int4 difference.
fn btint2cmp(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int4(
        a[0].as_i64().unwrap_or(0) as i32 - a[1].as_i64().unwrap_or(0) as i32,
    ))
}

fn btcharcmp(a: &[Value], _: &FnCtx) -> Result<Value> {
    let b = |v: &Value| match v {
        Value::Char(c) => *c as i32,
        _ => 0,
    };
    Ok(Value::Int4(b(&a[0]) - b(&a[1])))
}

/// bit_cmp: the bytes, then the shorter string first.
fn bitcmp(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (Value::Bits(n1, b1), Value::Bits(n2, b2)) = (&a[0], &a[1]) else {
        return Ok(Value::Int4(0));
    };
    let n = b1.len().min(b2.len());
    Ok(Value::Int4(
        match b1[..n].iter().zip(&b2[..n]).find(|(x, y)| x != y) {
            Some((x, y)) => *x as i32 - *y as i32,
            None => (*n1 as i32 - *n2 as i32).signum(),
        },
    ))
}

fn uuid_cmp(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (Value::Uuid(x), Value::Uuid(y)) = (&a[0], &a[1]) else {
        return Ok(Value::Int4(0));
    };
    let (x, y) = (x.to_be_bytes(), y.to_be_bytes());
    Ok(Value::Int4(
        x.iter()
            .zip(&y)
            .find(|(p, q)| p != q)
            .map_or(0, |(p, q)| *p as i32 - *q as i32),
    ))
}

/// bitncmp: the first `n` bits of two addresses.
fn bitncmp(l: &[u8], r: &[u8], n: usize) -> i32 {
    let b = n / 8;
    if let Some((x, y)) = l[..b].iter().zip(&r[..b]).find(|(x, y)| x != y) {
        return *x as i32 - *y as i32;
    }
    if n.is_multiple_of(8) {
        return 0;
    }
    let (mut lb, mut rb) = (l[b], r[b]);
    for _ in 0..n % 8 {
        if lb & 0x80 != rb & 0x80 {
            return if lb & 0x80 != 0 { 1 } else { -1 };
        }
        lb <<= 1;
        rb <<= 1;
    }
    0
}

/// network_cmp_internal: family, the common prefix, the netmask length, the whole address.
fn network_cmp(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (Value::Inet(x, xb), Value::Inet(y, yb)) = (&a[0], &a[1]) else {
        return Ok(Value::Int4(0));
    };
    // PGSQL_AF_INET and PGSQL_AF_INET6.
    let family = |addr: &[u8]| if addr.len() == 4 { 2 } else { 3 };
    if family(x) != family(y) {
        return Ok(Value::Int4(family(x) - family(y)));
    }
    let mut order = bitncmp(x, y, (*xb).min(*yb) as usize);
    if order == 0 {
        order = *xb as i32 - *yb as i32;
    }
    if order == 0 {
        order = bitncmp(x, y, x.len() * 8);
    }
    Ok(Value::Int4(order))
}

fn jsonb_cmp(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (Value::Jsonb(x), Value::Jsonb(y)) = (&a[0], &a[1]) else {
        return Ok(Value::Int4(0));
    };
    Ok(Value::Int4(crate::types::json::compare(x, y)))
}

/// An array whose element type has no hash function.
fn no_hash(c: &FnCtx) -> crate::error::Error {
    let elem = match c.arg_type(0) {
        DataType::Array(e) => (*e).clone(),
        t => t,
    };
    no_hash_for(&elem, c.args.len() > 1)
}

/// The error for a type without a (standard / extended) hash function.
fn no_hash_for(t: &DataType, extended: bool) -> crate::error::Error {
    crate::error::Error::new(
        crate::error::code::UNDEFINED_FUNCTION,
        format!(
            "could not identify {} hash function for type {}",
            if extended { "an extended" } else { "a" },
            t.display()
        ),
    )
}

/// ROTATE_HIGH_AND_LOW_32BITS: each 32-bit half rotated left by one.
fn rotate_halves(v: u64) -> u64 {
    ((v << 1) & 0xffff_fffe_ffff_fffe) | ((v >> 31) & 0x0000_0001_0000_0001)
}

/// The extended (64-bit, seeded) hash of `v` as Postgres's hash support functions compute it,
/// for the types whose hash looks at more than the value: enums (their pg_enum oid), ranges
/// and multiranges (flags and bounds), records (their fields), aclitems. The standard hash is
/// the low 32 bits of the one with seed 0. `extended` picks the error's wording.
fn hash_any(v: &Value, ty: &DataType, seed: u64, c: &FnCtx, extended: bool) -> Result<u64> {
    use crate::types::hash::{hash_uint32_extended, value_hash};
    // Range flags (rangetypes.h) for a range and its bounds' hash.
    let range = |r: &crate::types::range::Range, sub: &DataType| -> Result<u64> {
        let mut flags: u32 = 0;
        if r.empty {
            flags |= 0x01;
        } else {
            if r.lower_inc {
                flags |= 0x02;
            }
            if r.upper_inc {
                flags |= 0x04;
            }
            if r.lower.is_none() {
                flags |= 0x08;
            }
            if r.upper.is_none() {
                flags |= 0x10;
            }
        }
        let bound = |b: &Option<Value>| -> Result<u64> {
            match b {
                Some(v) if !r.empty => hash_any(v, sub, seed, c, extended),
                _ => Ok(0),
            }
        };
        let mut h = hash_uint32_extended(flags, seed) ^ bound(&r.lower)?;
        h = rotate_halves(h);
        Ok(h ^ bound(&r.upper)?)
    };
    match (v, ty) {
        (Value::Range(r), DataType::Range(rt)) => range(r, &rt.subtype),
        (Value::Multirange(rs), DataType::Multirange(rt)) => {
            let mut h: u64 = 1;
            for r in rs.iter() {
                let rh = range(r, &rt.subtype)?;
                h = (h << 5).wrapping_sub(h).wrapping_add(rh);
            }
            Ok(h)
        }
        (Value::Record(fields), DataType::Row(rt)) => {
            let mut h: u64 = 0;
            for (f, (_, ft)) in fields.iter().zip(&rt.fields) {
                let eh = if f.is_null() {
                    0
                } else {
                    hash_any(f, ft, seed, c, extended)?
                };
                h = (h << 5).wrapping_sub(h).wrapping_add(eh);
            }
            Ok(h)
        }
        (Value::Text(label), DataType::Enum(oid, ..)) => {
            let cat = c.exec.catalog();
            let e = cat.enums.get(oid);
            let lo = e.and_then(|e| {
                let i = e.labels.iter().position(|l| **l == **label)?;
                e.label_oids.get(i).copied()
            });
            Ok(match lo {
                Some(o) => hash_uint32_extended(o, seed),
                None => crate::types::hash::hash_bytes_extended(label.as_bytes(), seed),
            })
        }
        (Value::Text(item), DataType::Aclitem) => {
            let names = c.exec.fmt().names.clone();
            let lookup = |r: &str| {
                names
                    .as_ref()
                    .and_then(|n| n.role_oid(&crate::catalog::quote_ident(r)).ok())
            };
            let (a, _) = crate::catalog::acl::parse_item(item, &lookup)?;
            let privs = a.privs | (a.goptions << 16);
            Ok(hash_uint32_extended(
                privs.wrapping_add(a.grantee).wrapping_add(a.grantor),
                seed,
            ))
        }
        (Value::Text(s), _) if c.coll.is_some_and(|k| !k.deterministic) => {
            Ok(collated_hash(&Value::Text(s.clone()), c, seed).unwrap_or(0))
        }
        _ => value_hash(v, ty, seed).ok_or_else(|| no_hash_for(ty, extended)),
    }
}

/// hashenum, hash_range, hash_multirange, hash_record, hash_aclitem.
pub(super) fn hash_any32(a: &[Value], c: &FnCtx) -> Result<Value> {
    let h = hash_any(&a[0], &c.arg_type(0), 0, c, false)?;
    Ok(Value::Int4(h as u32 as i32))
}

/// Their extended forms.
pub(super) fn hash_any64(a: &[Value], c: &FnCtx) -> Result<Value> {
    let seed = a[1].as_i64().unwrap_or(0) as u64;
    Ok(Value::Int8(hash_any(&a[0], &c.arg_type(0), seed, c, true)? as i64))
}

/// hashtext under a nondeterministic collation hashes the string's sort key (with the
/// terminating NUL Postgres keeps), so equal strings hash alike.
fn collated_hash(a: &Value, c: &FnCtx, seed: u64) -> Option<u64> {
    let coll = c.coll.filter(|k| !k.deterministic)?;
    let Value::Text(s) = a else { return None };
    // hashtext hashes the sort key with its terminating NUL.
    let key = coll.key(s);
    Some(crate::types::hash::hash_bytes_extended(&key, seed))
}

fn hash32(a: &[Value], c: &FnCtx) -> Result<Value> {
    if let Some(h) = collated_hash(&a[0], c, 0) {
        return Ok(Value::Int4(h as u32 as i32));
    }
    let h = crate::types::hash::value_hash(&a[0], &c.arg_type(0), 0).ok_or_else(|| no_hash(c))?;
    Ok(Value::Int4(h as u32 as i32))
}

fn hash64(a: &[Value], c: &FnCtx) -> Result<Value> {
    let seed = a[1].as_i64().unwrap_or(0) as u64;
    if let Some(h) = collated_hash(&a[0], c, seed) {
        return Ok(Value::Int8(h as i64));
    }
    let h =
        crate::types::hash::value_hash(&a[0], &c.arg_type(0), seed).ok_or_else(|| no_hash(c))?;
    Ok(Value::Int8(h as i64))
}

fn pattern(a: &[Value], c: &FnCtx, keep: fn(i32) -> bool) -> Result<Value> {
    let cmp = match c.arg_type(0) {
        DataType::Bpchar(_) => bpchar_bytes(a, c)?,
        _ => memcmp_style(a, c)?,
    };
    Ok(Value::Bool(keep(cmp.as_i64().unwrap_or(0) as i32)))
}

fn pattern_lt(a: &[Value], c: &FnCtx) -> Result<Value> {
    pattern(a, c, |o| o < 0)
}
fn pattern_le(a: &[Value], c: &FnCtx) -> Result<Value> {
    pattern(a, c, |o| o <= 0)
}
fn pattern_ge(a: &[Value], c: &FnCtx) -> Result<Value> {
    pattern(a, c, |o| o >= 0)
}
fn pattern_gt(a: &[Value], c: &FnCtx) -> Result<Value> {
    pattern(a, c, |o| o > 0)
}

pub fn register(r: &mut Registry) {
    use DataType::*;
    // The pattern operators: byte order, whatever the collation.
    for t in [Text, Bpchar(-1)] {
        r.binop("~<~", t.clone(), t.clone(), Bool, pattern_lt);
        r.binop("~<=~", t.clone(), t.clone(), Bool, pattern_le);
        r.binop("~>=~", t.clone(), t.clone(), Bool, pattern_ge);
        r.binop("~>~", t.clone(), t, Bool, pattern_gt);
    }
    for (name, ext, t) in [
        ("hashenum", "hashenumextended", AnyEnum),
        ("hash_range", "hash_range_extended", AnyRange),
        ("hash_multirange", "hash_multirange_extended", AnyMultirange),
        ("hash_record", "hash_record_extended", Record),
        ("hash_aclitem", "hash_aclitem_extended", Aclitem),
    ] {
        r.func(name, vec![t.clone()], Int4, hash_any32);
        r.func(ext, vec![t, Int8], Int8, hash_any64);
    }
    for (name, ext, t) in [
        ("hashint2", "hashint2extended", Int2),
        ("hashint4", "hashint4extended", Int4),
        ("hashint8", "hashint8extended", Int8),
        ("hashoid", "hashoidextended", Oid),
        ("hashchar", "hashcharextended", Char),
        ("hashbool", "hashboolextended", Bool),
        ("hashname", "hashnameextended", Name),
        ("hashtext", "hashtextextended", Text),
        ("hashbpchar", "hashbpcharextended", Bpchar(-1)),
        ("hashbytea", "hashbyteaextended", Bytea),
        ("hashfloat4", "hashfloat4extended", Float4),
        ("hashfloat8", "hashfloat8extended", Float8),
        ("hash_numeric", "hash_numeric_extended", Numeric(-1)),
        ("hashdate", "hashdateextended", Date),
        ("time_hash", "time_hash_extended", Time(-1)),
        ("timetz_hash", "timetz_hash_extended", Timetz(-1)),
        ("timestamp_hash", "timestamp_hash_extended", Timestamp(-1)),
        (
            "timestamptz_hash",
            "timestamptz_hash_extended",
            Timestamptz(-1),
        ),
        ("interval_hash", "interval_hash_extended", Interval(-1)),
        ("uuid_hash", "uuid_hash_extended", Uuid),
        ("hashmacaddr", "hashmacaddrextended", Macaddr),
        ("hashmacaddr8", "hashmacaddr8extended", Macaddr8),
        ("hashinet", "hashinetextended", Inet),
        ("pg_lsn_hash", "pg_lsn_hash_extended", PgLsn),
        ("hashxid8", "hashxid8extended", Xid8),
        ("hashtid", "hashtidextended", Tid),
        ("hashoidvector", "hashoidvectorextended", OidVector),
        ("jsonb_hash", "jsonb_hash_extended", Jsonb),
        ("hash_array", "hash_array_extended", AnyArray),
    ] {
        r.func(name, vec![t.clone()], Int4, hash32);
        r.func(ext, vec![t, Int8], Int8, hash64);
    }
    let ops = [
        ("btint4cmp", Int4, Int4),
        ("btint8cmp", Int8, Int8),
        ("btint24cmp", Int2, Int4),
        ("btint42cmp", Int4, Int2),
        ("btint28cmp", Int2, Int8),
        ("btint82cmp", Int8, Int2),
        ("btint48cmp", Int4, Int8),
        ("btint84cmp", Int8, Int4),
        ("btfloat4cmp", Float4, Float4),
        ("btfloat8cmp", Float8, Float8),
        ("btfloat48cmp", Float4, Float8),
        ("btfloat84cmp", Float8, Float4),
        ("btoidcmp", Oid, Oid),
        ("btoidvectorcmp", OidVector, OidVector),
        ("btboolcmp", Bool, Bool),
        ("bttidcmp", Tid, Tid),
        ("numeric_cmp", Numeric(-1), Numeric(-1)),
        ("cash_cmp", Money, Money),
        ("date_cmp", Date, Date),
        ("time_cmp", Time(-1), Time(-1)),
        ("timetz_cmp", Timetz(-1), Timetz(-1)),
        ("timestamp_cmp", Timestamp(-1), Timestamp(-1)),
        ("timestamptz_cmp", Timestamptz(-1), Timestamptz(-1)),
        ("date_cmp_timestamp", Date, Timestamp(-1)),
        ("date_cmp_timestamptz", Date, Timestamptz(-1)),
        ("timestamp_cmp_date", Timestamp(-1), Date),
        ("timestamptz_cmp_date", Timestamptz(-1), Date),
        ("timestamp_cmp_timestamptz", Timestamp(-1), Timestamptz(-1)),
        ("timestamptz_cmp_timestamp", Timestamptz(-1), Timestamp(-1)),
        ("interval_cmp", Interval(-1), Interval(-1)),
        ("macaddr_cmp", Macaddr, Macaddr),
        ("macaddr8_cmp", Macaddr8, Macaddr8),
        ("pg_lsn_cmp", PgLsn, PgLsn),
        ("xid8cmp", Xid8, Xid8),
        ("btarraycmp", AnyArray, AnyArray),
        ("enum_cmp", AnyEnum, AnyEnum),
        ("btrecordcmp", Record, Record),
    ];
    for (name, l, r2) in ops {
        r.func(name, vec![l, r2], Int4, by_operators);
    }
    // The larger / smaller of two arrays (max / min's transition functions).
    r.func("array_larger", vec![AnyArray, AnyArray], AnyArray, |a, c| {
        let gt = by_operators(a, c)? == Value::Int4(1);
        Ok(a[usize::from(!gt)].clone())
    });
    r.func("array_smaller", vec![AnyArray, AnyArray], AnyArray, |a, c| {
        let lt = by_operators(a, c)? == Value::Int4(-1);
        Ok(a[usize::from(!lt)].clone())
    });
    r.func("bttextcmp", vec![Text, Text], Int4, text_cmp);
    r.func("bttext_pattern_cmp", vec![Text, Text], Int4, memcmp_style);
    r.func("btnametextcmp", vec![Name, Text], Int4, text_cmp);
    r.func("bttextnamecmp", vec![Text, Name], Int4, text_cmp);
    r.func("byteacmp", vec![Bytea, Bytea], Int4, memcmp_style);
    r.func("gin_cmp_tslexeme", vec![Text, Text], Int4, memcmp_style);
    r.func("bpcharcmp", vec![Bpchar(-1), Bpchar(-1)], Int4, bpchar);
    r.func(
        "btbpchar_pattern_cmp",
        vec![Bpchar(-1), Bpchar(-1)],
        Int4,
        bpchar_bytes,
    );
    r.func("btnamecmp", vec![Name, Name], Int4, name);
    r.func("btint2cmp", vec![Int2, Int2], Int4, btint2cmp);
    r.func("btcharcmp", vec![Char, Char], Int4, btcharcmp);
    r.func("bitcmp", vec![Bit(-1), Bit(-1)], Int4, bitcmp);
    r.func("varbitcmp", vec![Varbit(-1), Varbit(-1)], Int4, bitcmp);
    r.func("uuid_cmp", vec![Uuid, Uuid], Int4, uuid_cmp);
    r.func("network_cmp", vec![Inet, Inet], Int4, network_cmp);
    r.func("jsonb_cmp", vec![Jsonb, Jsonb], Int4, jsonb_cmp);
}
