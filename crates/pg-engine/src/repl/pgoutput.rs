//! pgoutput, the logical replication output plugin (pgoutput.c, proto.c): the binary
//! protocol PostgreSQL subscribers and CDC clients read, with publications applied
//! (published actions, FOR TABLES IN SCHEMA / ALL TABLES, publish_via_partition_root,
//! column lists, generated columns, row filters).

use super::log::{ColDesc, RelDesc};
use super::plugin::{Ctx, Plugin, RowChange, Startup, TxnInfo};
use crate::catalog::pubsub::Publication;
use crate::error::{Error, Result, code};
use crate::types::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// LOGICALREP_PROTO_MAX_VERSION_NUM and the versions features need.
const MAX_PROTO: i64 = 4;
const STREAM_PROTO: i64 = 2;
const TWOPHASE_PROTO: i64 = 3;
const PARALLEL_PROTO: i64 = 4;

/// What a relation is published as (RelationSyncEntry).
struct Entry {
    insert: bool,
    update: bool,
    delete: bool,
    truncate: bool,
    /// The relation changes are published as (an ancestor with publish_via_partition_root).
    as_rel: Arc<RelDesc>,
    /// Positions (in `as_rel`) of the published columns.
    cols: Vec<usize>,
    /// Row filters per action (empty: none; any publication without one: none).
    filters: [Vec<String>; 3],
    /// The child's column positions in `as_rel` (by name).
    map: Option<Vec<Option<usize>>>,
}

#[derive(Default)]
pub struct PgOutput {
    proto: i64,
    pubs: Vec<String>,
    binary: bool,
    messages: bool,
    streaming: bool,
    two_phase: bool,
    origin_none: bool,
    entries: HashMap<u32, (Arc<RelDesc>, Arc<Entry>)>,
    /// The catalog version `entries` were worked out under (rel_sync_cache_publication_cb:
    /// a relation's description is the same `Arc` across transactions, `decode::Shared`).
    entries_catalog: u64,
    /// Relations whose schema went out (per streamed transaction, or outside streaming).
    sent: HashSet<u32>,
    stream_sent: HashSet<u32>,
    sent_begin: bool,
    in_stream: bool,
}

fn i8(b: &mut Vec<u8>, v: u8) {
    b.push(v);
}
fn i16(b: &mut Vec<u8>, v: i16) {
    b.extend_from_slice(&v.to_be_bytes());
}
fn i32(b: &mut Vec<u8>, v: i32) {
    b.extend_from_slice(&v.to_be_bytes());
}
fn i64(b: &mut Vec<u8>, v: i64) {
    b.extend_from_slice(&v.to_be_bytes());
}
fn cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

fn conflicting() -> Error {
    Error::new(code::SYNTAX_ERROR, "conflicting or redundant options")
}

/// SplitIdentifierString of publication_names.
fn split_names(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut name = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    None => return None,
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        name.push('"');
                    }
                    Some('"') => break,
                    Some(c) => name.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' || c.is_whitespace() {
                    break;
                }
                name.push(c.to_ascii_lowercase());
                chars.next();
            }
            if name.is_empty() {
                return None;
            }
        }
        out.push(name);
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.next() {
            None => return Some(out),
            Some(',') => continue,
            Some(_) => return None,
        }
    }
}

impl PgOutput {
    fn xid(&self, b: &mut Vec<u8>, t: &TxnInfo) {
        if self.in_stream {
            i32(b, t.xid as i32);
        }
    }

    /// What `entries` holds is worked out again after DDL (publications, their tables).
    fn invalidate(&mut self, c: &Ctx) {
        let v = c.db.catalog().version;
        if v != self.entries_catalog {
            self.entries.clear();
            self.entries_catalog = v;
        }
    }

    /// get_rel_sync_entry.
    fn entry(&mut self, c: &Ctx, rel: &Arc<RelDesc>) -> Result<Arc<Entry>> {
        if let Some((d, e)) = self.entries.get(&rel.oid)
            && Arc::ptr_eq(d, rel)
        {
            return Ok(e.clone());
        }
        let cat = c.db.catalog();
        let wanted: Vec<String> = self.pubs.clone();
        let Some(t) = cat.tables.get(&rel.oid).cloned() else {
            let e = Arc::new(Entry {
                insert: false,
                update: false,
                delete: false,
                truncate: false,
                as_rel: rel.clone(),
                cols: Vec::new(),
                filters: Default::default(),
                map: None,
            });
            return Ok(e);
        };
        let pubs: Vec<(Arc<Publication>, u32)> =
            cat.publications_of(&t).into_iter().filter(|(p, _)| wanted.contains(&p.name)).collect();
        // publish_as_relid: the topmost ancestor a publication with the option publishes
        // it through.
        let mut as_oid = rel.oid;
        for (p, via) in &pubs {
            if p.via_root && *via != rel.oid {
                let rank = |o: u32| rel.ancestors.iter().position(|a| *a == o).map_or(0, |i| i + 1);
                if rank(*via) > rank(as_oid) {
                    as_oid = *via;
                }
            }
        }
        let as_rel = if as_oid == rel.oid {
            rel.clone()
        } else {
            match cat.tables.get(&as_oid) {
                Some(at) => Arc::new(super::log::describe_in(at, &cat, &[])),
                None => rel.clone(),
            }
        };
        let (mut ins, mut upd, mut del, mut trunc) = (false, false, false, false);
        let mut filters: [Vec<String>; 3] = Default::default();
        let mut unfiltered = [false; 3];
        let mut cols: Option<Option<Vec<i16>>> = None;
        let mut gencols = 'n';
        for (p, via) in &pubs {
            // A publication publishing through another ancestor than the one used.
            if as_oid != rel.oid && !p.via_root && *via != as_oid && p.all_tables {
                continue;
            }
            ins |= p.insert;
            upd |= p.update;
            del |= p.delete;
            trunc |= p.truncate;
            if p.gencols == 's' {
                gencols = 's';
            }
            let entry = (!p.all_tables).then(|| p.rels.iter().find(|r| r.rel == as_oid)).flatten();
            let qual = entry.and_then(|r| r.qual.clone());
            for (i, on) in [p.insert, p.update, p.delete].into_iter().enumerate() {
                if !on {
                    continue;
                }
                match &qual {
                    Some(q) => filters[i].push(q.clone()),
                    None => unfiltered[i] = true,
                }
            }
            let list = entry.and_then(|r| r.cols.clone());
            match &cols {
                None => cols = Some(list),
                Some(prev) if *prev != list => {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cannot use different column lists for table \"{}.{}\" in different publications",
                            as_rel.nsp, as_rel.name
                        ),
                    ));
                }
                _ => {}
            }
        }
        for i in 0..3 {
            if unfiltered[i] {
                filters[i].clear();
            }
        }
        let published = |i: usize, col: &ColDesc| -> bool {
            if col.dropped || col.generated == 'v' {
                return false;
            }
            match cols.as_ref().and_then(|c| c.as_ref()) {
                Some(list) => list.contains(&(i as i16 + 1)),
                None => col.generated == '\0' || gencols == 's',
            }
        };
        let colpos: Vec<usize> = as_rel.cols.iter().enumerate().filter(|(i, c)| published(*i, c)).map(|(i, _)| i).collect();
        let map = (as_oid != rel.oid).then(|| {
            as_rel
                .cols
                .iter()
                .map(|ac| rel.cols.iter().position(|c| !c.dropped && c.name == ac.name && !ac.dropped))
                .collect()
        });
        let e = Arc::new(Entry {
            insert: ins,
            update: upd,
            delete: del,
            truncate: trunc,
            as_rel,
            cols: colpos,
            filters,
            map,
        });
        self.entries.insert(rel.oid, (rel.clone(), e.clone()));
        Ok(e)
    }

    fn send_begin(&mut self, c: &mut Ctx, t: &TxnInfo) {
        if self.sent_begin || self.in_stream {
            return;
        }
        self.sent_begin = true;
        let mut b = vec![b'B'];
        i64(&mut b, t.final_lsn as i64);
        i64(&mut b, t.time);
        i32(&mut b, t.xid as i32);
        let at = c.write_lsn;
        c.write_lsn = t.first_lsn;
        c.write(b);
        if t.origin != 0 {
            let mut o = vec![b'O'];
            i64(&mut o, t.origin_lsn as i64);
            cstr(&mut o, &format!("pg_{}", t.origin));
            c.write(o);
        }
        c.write_lsn = at;
    }

    /// maybe_send_schema: the Relation (and Type) messages before a relation's first change.
    fn schema(&mut self, c: &mut Ctx, t: &TxnInfo, e: &Entry) {
        let rel = &e.as_rel;
        let set = if self.in_stream { &mut self.stream_sent } else { &mut self.sent };
        if !set.insert(rel.oid) {
            return;
        }
        for &i in &e.cols {
            let col = &rel.cols[i];
            if col.type_oid >= 10000 {
                let mut b = vec![b'Y'];
                self.xid(&mut b, t);
                i32(&mut b, col.type_oid as i32);
                cstr(&mut b, if col.type_nsp == "pg_catalog" { "" } else { &col.type_nsp });
                cstr(&mut b, &col.type_name);
                c.write(b);
            }
        }
        let mut b = vec![b'R'];
        self.xid(&mut b, t);
        i32(&mut b, rel.oid as i32);
        cstr(&mut b, if rel.nsp == "pg_catalog" { "" } else { &rel.nsp });
        cstr(&mut b, &rel.name);
        i8(&mut b, rel.replident as u8);
        i16(&mut b, e.cols.len() as i16);
        for &i in &e.cols {
            let col = &rel.cols[i];
            i8(&mut b, col.key as u8);
            cstr(&mut b, &col.name);
            i32(&mut b, col.type_oid as i32);
            i32(&mut b, col.typmod);
        }
        c.write(b);
    }

    /// The row as the published relation's (an ancestor's columns, by name).
    fn mapped<'v>(&self, e: &Entry, row: &'v [Value]) -> std::borrow::Cow<'v, [Value]> {
        match &e.map {
            None => std::borrow::Cow::Borrowed(row),
            Some(m) => std::borrow::Cow::Owned(
                m.iter().map(|p| p.and_then(|p| row.get(p).cloned()).unwrap_or(Value::Null)).collect(),
            ),
        }
    }

    fn tuple(&self, b: &mut Vec<u8>, c: &Ctx, e: &Entry, row: &[Value], toast: &[usize]) -> Result<()> {
        i16(b, e.cols.len() as i16);
        for &i in &e.cols {
            let col = &e.as_rel.cols[i];
            let v = row.get(i).unwrap_or(&Value::Null);
            if v.is_null() {
                i8(b, b'n');
            } else if toast.contains(&i) {
                i8(b, b'u');
            } else if self.binary && crate::types::binary::supports_binary(&col.ty) {
                // (logicalrep_write_tuple: a type without a send function goes as text.)
                i8(b, b'b');
                let mut out = bytes::BytesMut::new();
                crate::types::binary::encode(v, &col.ty, &c.fmt, &mut out)?;
                i32(b, out.len() as i32);
                b.extend_from_slice(&out);
            } else {
                i8(b, b't');
                let s = crate::types::text::format_value(v, &col.ty, &c.fmt);
                i32(b, s.len() as i32);
                b.extend_from_slice(s.as_bytes());
            }
        }
        Ok(())
    }

    fn passes(c: &mut Ctx, filters: &[String], rel: &RelDesc, row: &[Value]) -> Result<bool> {
        if filters.is_empty() {
            return Ok(true);
        }
        let Some(f) = c.filter.as_mut() else { return Ok(true) };
        for q in filters {
            if f(rel, q, row)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn row_change(&mut self, c: &mut Ctx, t: &TxnInfo, rel: &Arc<RelDesc>, ch: &RowChange) -> Result<()> {
        let e = self.entry(c, rel)?;
        let map_toast = |toast: &[usize]| -> Vec<usize> {
            match &e.map {
                None => toast.to_vec(),
                Some(m) => m.iter().enumerate().filter(|(_, p)| p.is_some_and(|p| toast.contains(&p))).map(|(i, _)| i).collect(),
            }
        };
        // What goes out: the action as the row filters make it (pgoutput_row_filter).
        enum Out {
            Insert(Vec<Value>),
            Update(Option<(Vec<Value>, bool)>, Vec<Value>, Vec<usize>),
            Delete(Vec<Value>, bool),
        }
        let out = match ch {
            RowChange::Insert { new } => {
                if !e.insert {
                    return Ok(());
                }
                let new = self.mapped(&e, new).into_owned();
                if !Self::passes(c, &e.filters[0].clone(), &e.as_rel, &new)? {
                    return Ok(());
                }
                Out::Insert(new)
            }
            RowChange::Update { old, new, unchanged_toast } => {
                if !e.update {
                    return Ok(());
                }
                let new = self.mapped(&e, new).into_owned();
                let old = old.map(|(o, k)| (self.mapped(&e, o).into_owned(), k));
                let toast = map_toast(unchanged_toast);
                let filters = e.filters[1].clone();
                if filters.is_empty() {
                    Out::Update(old, new, toast)
                } else {
                    // A key-only old row takes the new row's other columns.
                    let old_full: Option<Vec<Value>> = old.as_ref().map(|(o, key)| {
                        o.iter()
                            .zip(&new)
                            .map(|(a, b)| if *key && a.is_null() { b.clone() } else { a.clone() })
                            .collect()
                    });
                    let new_ok = Self::passes(c, &filters, &e.as_rel, &new)?;
                    let old_ok = match &old_full {
                        Some(o) => Self::passes(c, &filters, &e.as_rel, o)?,
                        None => new_ok,
                    };
                    match (old_ok, new_ok) {
                        (false, false) => return Ok(()),
                        (true, true) => Out::Update(old, new, toast),
                        (false, true) => Out::Insert(new),
                        (true, false) => {
                            let o = old_full.unwrap_or_else(|| new.clone());
                            Out::Delete(o, false)
                        }
                    }
                }
            }
            RowChange::Delete { old } => {
                if !e.delete {
                    return Ok(());
                }
                let Some((o, key)) = old else { return Ok(()) };
                let o = self.mapped(&e, o).into_owned();
                if !Self::passes(c, &e.filters[2].clone(), &e.as_rel, &o)? {
                    return Ok(());
                }
                Out::Delete(o, *key)
            }
        };
        self.send_begin(c, t);
        self.schema(c, t, &e);
        let relid = e.as_rel.oid as i32;
        let mut b = Vec::new();
        match out {
            Out::Insert(new) => {
                b.push(b'I');
                self.xid(&mut b, t);
                i32(&mut b, relid);
                b.push(b'N');
                self.tuple(&mut b, c, &e, &new, &[])?;
            }
            Out::Update(old, new, toast) => {
                b.push(b'U');
                self.xid(&mut b, t);
                i32(&mut b, relid);
                if let Some((o, key)) = old {
                    b.push(if key { b'K' } else { b'O' });
                    self.tuple(&mut b, c, &e, &o, &[])?;
                }
                b.push(b'N');
                self.tuple(&mut b, c, &e, &new, &toast)?;
            }
            Out::Delete(o, key) => {
                b.push(b'D');
                self.xid(&mut b, t);
                i32(&mut b, relid);
                b.push(if key { b'K' } else { b'O' });
                self.tuple(&mut b, c, &e, &o, &[])?;
            }
        }
        c.write(b);
        Ok(())
    }

    fn do_truncate(&mut self, c: &mut Ctx, t: &TxnInfo, rels: &[Arc<RelDesc>], cascade: bool, restart: bool) -> Result<()> {
        let mut ids = Vec::new();
        let mut entries = Vec::new();
        for r in rels {
            let e = self.entry(c, r)?;
            if !e.truncate || ids.contains(&e.as_rel.oid) {
                continue;
            }
            // (Truncating a partition published through its root sends nothing.)
            if e.as_rel.oid != r.oid {
                continue;
            }
            ids.push(e.as_rel.oid);
            entries.push(e);
        }
        if ids.is_empty() {
            return Ok(());
        }
        self.send_begin(c, t);
        for e in &entries {
            self.schema(c, t, e);
        }
        let mut b = vec![b'T'];
        self.xid(&mut b, t);
        i32(&mut b, ids.len() as i32);
        i8(&mut b, cascade as u8 | ((restart as u8) << 1));
        for id in ids {
            i32(&mut b, id as i32);
        }
        c.write(b);
        Ok(())
    }

    fn do_message(&mut self, c: &mut Ctx, t: Option<&TxnInfo>, lsn: u64, transactional: bool, prefix: &str, content: &[u8]) {
        if !self.messages {
            return;
        }
        if transactional && let Some(t) = t {
            self.send_begin(c, t);
        }
        let mut b = vec![b'M'];
        if self.in_stream && transactional && let Some(t) = t {
            i32(&mut b, t.xid as i32);
        }
        i8(&mut b, transactional as u8);
        i64(&mut b, lsn as i64);
        cstr(&mut b, prefix);
        i32(&mut b, content.len() as i32);
        b.extend_from_slice(content);
        c.write(b);
    }

    fn gid_msg(kind: u8, flags: bool, lsns: &[u64], times: &[i64], t: &TxnInfo) -> Vec<u8> {
        let mut b = vec![kind];
        if flags {
            i8(&mut b, 0);
        }
        for l in lsns {
            i64(&mut b, *l as i64);
        }
        for tm in times {
            i64(&mut b, *tm);
        }
        i32(&mut b, t.xid as i32);
        cstr(&mut b, &t.gid);
        b
    }
}

impl Plugin for PgOutput {
    fn startup(&mut self, opts: &[(String, Option<String>)], is_init: bool, s: &mut Startup) -> Result<()> {
        s.binary = true;
        if is_init {
            return Ok(());
        }
        let (mut proto, mut pubs, mut binary, mut messages, mut streaming, mut two_phase, mut origin) =
            (None, None, None, None, None, None, None);
        let boolv = |v: &Option<String>| -> Result<bool> {
            match v {
                None => Ok(true),
                Some(x) => crate::types::text::parse_bool(x).map_err(|_| {
                    Error::new(code::INVALID_PARAMETER_VALUE, format!("invalid value for boolean option: {x}"))
                }),
            }
        };
        for (name, v) in opts {
            match name.as_str() {
                "proto_version" => {
                    if proto.is_some() {
                        return Err(conflicting());
                    }
                    let x = v.as_deref().unwrap_or("");
                    let n: i64 = x.trim().parse().map_err(|_| Error::new(code::INVALID_PARAMETER_VALUE, "invalid proto_version"))?;
                    if !(0..=u32::MAX as i64).contains(&n) {
                        return Err(Error::new(code::INVALID_PARAMETER_VALUE, format!("proto_version \"{x}\" out of range")));
                    }
                    proto = Some(n);
                }
                "publication_names" => {
                    if pubs.is_some() {
                        return Err(conflicting());
                    }
                    pubs = Some(
                        split_names(v.as_deref().unwrap_or(""))
                            .ok_or_else(|| Error::new(code::INVALID_NAME, "invalid publication_names syntax"))?,
                    );
                }
                "binary" => {
                    if binary.is_some() {
                        return Err(conflicting());
                    }
                    binary = Some(boolv(v)?);
                }
                "messages" => {
                    if messages.is_some() {
                        return Err(conflicting());
                    }
                    messages = Some(boolv(v)?);
                }
                "streaming" => {
                    if streaming.is_some() {
                        return Err(conflicting());
                    }
                    streaming = Some(match v.as_deref() {
                        Some(x) if x.eq_ignore_ascii_case("parallel") => 2,
                        _ => boolv(v)? as i32,
                    });
                }
                "two_phase" => {
                    if two_phase.is_some() {
                        return Err(conflicting());
                    }
                    two_phase = Some(boolv(v)?);
                }
                "origin" => {
                    if origin.is_some() {
                        return Err(conflicting());
                    }
                    let x = v.clone().unwrap_or_default();
                    if !x.eq_ignore_ascii_case("none") && !x.eq_ignore_ascii_case("any") {
                        return Err(Error::new(code::INVALID_PARAMETER_VALUE, format!("unrecognized origin value: \"{x}\"")));
                    }
                    origin = Some(x.eq_ignore_ascii_case("none"));
                }
                _ => return Err(Error::new(code::INVALID_PARAMETER_VALUE, format!("unrecognized pgoutput option: {name}"))),
            }
        }
        let Some(proto) = proto else {
            return Err(Error::new(code::INVALID_PARAMETER_VALUE, "option \"proto_version\" missing"));
        };
        let Some(pubs) = pubs else {
            return Err(Error::new(code::INVALID_PARAMETER_VALUE, "option \"publication_names\" missing"));
        };
        if proto > MAX_PROTO {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("client sent proto_version={proto} but server only supports protocol {MAX_PROTO} or lower"),
            ));
        }
        if proto < 1 {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("client sent proto_version={proto} but server only supports protocol 1 or higher"),
            ));
        }
        let streaming = streaming.unwrap_or(0);
        if streaming > 0 && proto < STREAM_PROTO {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("requested proto_version={proto} does not support streaming, need {STREAM_PROTO} or higher"),
            ));
        }
        if streaming == 2 && proto < PARALLEL_PROTO {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("requested proto_version={proto} does not support parallel streaming, need {PARALLEL_PROTO} or higher"),
            ));
        }
        let two_phase = two_phase.unwrap_or(false);
        if two_phase && proto < TWOPHASE_PROTO {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("requested proto_version={proto} does not support two-phase commit, need {TWOPHASE_PROTO} or higher"),
            ));
        }
        self.proto = proto;
        self.pubs = pubs;
        self.binary = binary.unwrap_or(false);
        self.messages = messages.unwrap_or(false);
        self.streaming = streaming > 0;
        self.two_phase = two_phase;
        self.origin_none = origin.unwrap_or(false);
        s.streaming = self.streaming;
        s.twophase_opt = two_phase;
        Ok(())
    }

    fn begin(&mut self, c: &mut Ctx, _t: &TxnInfo) -> Result<()> {
        self.sent_begin = false;
        self.invalidate(c);
        Ok(())
    }

    fn commit(&mut self, c: &mut Ctx, t: &TxnInfo, lsn: u64) -> Result<()> {
        if !self.sent_begin {
            return Ok(());
        }
        self.sent_begin = false;
        let mut b = vec![b'C', 0];
        i64(&mut b, lsn as i64);
        i64(&mut b, t.end_lsn as i64);
        i64(&mut b, t.time);
        c.write(b);
        Ok(())
    }

    fn change(&mut self, c: &mut Ctx, t: &TxnInfo, rel: &Arc<RelDesc>, ch: &RowChange) -> Result<()> {
        self.row_change(c, t, rel, ch)
    }

    fn truncate(&mut self, c: &mut Ctx, t: &TxnInfo, rels: &[Arc<RelDesc>], cascade: bool, restart: bool) -> Result<()> {
        self.do_truncate(c, t, rels, cascade, restart)
    }

    fn message(&mut self, c: &mut Ctx, t: Option<&TxnInfo>, lsn: u64, transactional: bool, prefix: &str, content: &[u8]) -> Result<()> {
        self.do_message(c, t, lsn, transactional, prefix, content);
        Ok(())
    }

    fn filter_by_origin(&mut self, origin: u16) -> bool {
        self.origin_none && origin != 0
    }

    fn begin_prepare(&mut self, c: &mut Ctx, t: &TxnInfo) -> Result<()> {
        self.sent_begin = true;
        self.invalidate(c);
        c.write(Self::gid_msg(b'b', false, &[t.final_lsn, t.end_lsn], &[t.time], t));
        Ok(())
    }

    fn prepare(&mut self, c: &mut Ctx, t: &TxnInfo, lsn: u64) -> Result<()> {
        self.sent_begin = false;
        c.write(Self::gid_msg(b'P', true, &[lsn, t.end_lsn], &[t.time], t));
        Ok(())
    }

    fn commit_prepared(&mut self, c: &mut Ctx, t: &TxnInfo, lsn: u64) -> Result<()> {
        c.write(Self::gid_msg(b'K', true, &[lsn, t.end_lsn], &[t.time], t));
        Ok(())
    }

    fn rollback_prepared(&mut self, c: &mut Ctx, t: &TxnInfo, prepare_end: u64, prepare_time: i64) -> Result<()> {
        c.write(Self::gid_msg(b'r', true, &[prepare_end, t.end_lsn], &[prepare_time, t.time], t));
        Ok(())
    }

    fn stream_start(&mut self, c: &mut Ctx, t: &TxnInfo) -> Result<()> {
        let first = !self.in_stream && self.stream_sent.is_empty();
        self.in_stream = true;
        self.invalidate(c);
        let mut b = vec![b'S'];
        i32(&mut b, t.xid as i32);
        i8(&mut b, first as u8);
        c.write(b);
        Ok(())
    }

    fn stream_stop(&mut self, c: &mut Ctx, _t: &TxnInfo) -> Result<()> {
        c.write(vec![b'E']);
        Ok(())
    }

    fn stream_abort(&mut self, c: &mut Ctx, t: &TxnInfo, sub: u64, lsn: u64) -> Result<()> {
        let mut b = vec![b'A'];
        i32(&mut b, t.xid as i32);
        i32(&mut b, sub as i32);
        if self.proto >= PARALLEL_PROTO {
            i64(&mut b, lsn as i64);
            i64(&mut b, t.time);
        }
        c.write(b);
        if sub == t.xid {
            self.in_stream = false;
            self.stream_sent.clear();
        }
        Ok(())
    }

    fn stream_prepare(&mut self, c: &mut Ctx, t: &TxnInfo, lsn: u64) -> Result<()> {
        self.in_stream = false;
        self.stream_sent.clear();
        c.write(Self::gid_msg(b'p', true, &[lsn, t.end_lsn], &[t.time], t));
        Ok(())
    }

    fn stream_commit(&mut self, c: &mut Ctx, t: &TxnInfo, lsn: u64) -> Result<()> {
        self.in_stream = false;
        self.stream_sent.clear();
        let mut b = vec![b'c'];
        i32(&mut b, t.xid as i32);
        i8(&mut b, 0);
        i64(&mut b, lsn as i64);
        i64(&mut b, t.end_lsn as i64);
        i64(&mut b, t.time);
        c.write(b);
        Ok(())
    }

    fn stream_change(&mut self, c: &mut Ctx, t: &TxnInfo, rel: &Arc<RelDesc>, ch: &RowChange) -> Result<()> {
        self.row_change(c, t, rel, ch)
    }

    fn stream_message(&mut self, c: &mut Ctx, t: &TxnInfo, lsn: u64, transactional: bool, prefix: &str, content: &[u8]) -> Result<()> {
        self.do_message(c, Some(t), lsn, transactional, prefix, content);
        Ok(())
    }

    fn stream_truncate(&mut self, c: &mut Ctx, t: &TxnInfo, rels: &[Arc<RelDesc>], cascade: bool, restart: bool) -> Result<()> {
        self.do_truncate(c, t, rels, cascade, restart)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn publication_names() {
        assert_eq!(super::split_names("\"p1\", P2 ,\"a\"\"b\"").unwrap(), vec!["p1", "p2", "a\"b"]);
        assert!(super::split_names("a,,b").is_none());
    }
}
