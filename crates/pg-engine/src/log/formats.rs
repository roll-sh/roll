//! csvlog and jsonlog (csvlog.c's write_csvlog, jsonlog.c's write_jsonlog): one line per
//! message, fields in PostgreSQL's order and escaping, so log analysers read them unchanged.

use super::{Format, Message, Verbosity, Who, session_id, stamp};

/// appendCSVLiteral: quoted, quotes doubled (nothing at all for a NULL).
fn csv_lit(b: &mut String, v: Option<&str>) {
    if let Some(v) = v {
        b.push('"');
        b.push_str(&v.replace('"', "\"\""));
        b.push('"');
    }
}

/// One csvlog line (`line`: the process's csvlog line number).
pub fn csv(f: &Format, who: &Who, now: i64, line: u64, m: &Message) -> String {
    let client = who.is_client();
    let s = who.session.as_ref().filter(|_| client);
    let mut b = String::with_capacity(256);
    let field = |b: &mut String, v: &str| {
        b.push_str(v);
        b.push(',');
    };
    field(&mut b, &stamp(now, &f.tz, true));
    csv_lit(&mut b, s.map(|s| s.user.as_str()));
    b.push(',');
    csv_lit(&mut b, s.map(|s| s.database.as_str()));
    b.push(',');
    field(&mut b, &who.pid.to_string());
    if let Some((h, p)) = &who.remote {
        b.push_str(&format!("\"{h}:{p}\""));
    }
    b.push(',');
    field(&mut b, &session_id(who));
    field(&mut b, &line.to_string());
    if client {
        csv_lit(&mut b, Some(s.map_or("", |s| s.command_tag.as_str())));
    }
    b.push(',');
    field(&mut b, &stamp(who.start, &f.tz, false));
    field(&mut b, s.map_or("", |s| s.vxid.as_str()));
    field(&mut b, &s.map_or(0, |s| s.xid).to_string());
    field(&mut b, m.severity().as_str());
    field(&mut b, m.code);
    csv_lit(&mut b, Some(m.text));
    b.push(',');
    csv_lit(&mut b, m.detail);
    b.push(',');
    csv_lit(&mut b, m.hint);
    b.push(',');
    csv_lit(&mut b, m.internal.map(|(_, q)| q));
    b.push(',');
    if let Some((p, _)) = m.internal.filter(|(p, _)| *p > 0) {
        b.push_str(&p.to_string());
    }
    b.push(',');
    csv_lit(&mut b, m.context);
    b.push(',');
    let stmt = m.statement_shown(f);
    csv_lit(&mut b, stmt);
    b.push(',');
    if let Some(p) = m.cursorpos.filter(|_| stmt.is_some()) {
        b.push_str(&p.to_string());
    }
    b.push(',');
    if f.verbosity == Verbosity::Verbose
        && let Some((routine, file, l)) = &m.location
    {
        csv_lit(&mut b, Some(&format!("{routine}, {file}:{l}")));
    }
    b.push(',');
    // application_name (a process without a session has the empty default).
    csv_lit(&mut b, Some(s.map_or("", |s| s.application_name.as_str())));
    b.push(',');
    csv_lit(&mut b, Some(who.backend_type));
    b.push(',');
    // leader_pid: no parallel workers' lock groups.
    b.push(',');
    b.push_str(&s.map_or(0, |s| s.query_id).to_string());
    b.push('\n');
    b
}

/// escape_json.
fn json_str(b: &mut String, v: &str) {
    b.push('"');
    for c in v.chars() {
        match c {
            '\u{8}' => b.push_str("\\b"),
            '\u{c}' => b.push_str("\\f"),
            '\n' => b.push_str("\\n"),
            '\r' => b.push_str("\\r"),
            '\t' => b.push_str("\\t"),
            '"' => b.push_str("\\\""),
            '\\' => b.push_str("\\\\"),
            c if (c as u32) < 0x20 => b.push_str(&format!("\\u{:04x}", c as u32)),
            c => b.push(c),
        }
    }
    b.push('"');
}

/// One jsonlog line: appendJSONKeyValue's `,"key":value`, NULLs left out.
pub fn json(f: &Format, who: &Who, now: i64, line: u64, m: &Message) -> String {
    let client = who.is_client();
    let s = who.session.as_ref().filter(|_| client);
    let mut b = String::with_capacity(384);
    b.push('{');
    json_str(&mut b, "timestamp");
    b.push(':');
    json_str(&mut b, &stamp(now, &f.tz, true));
    let kv = |b: &mut String, k: &str, v: Option<&str>, quote: bool| {
        if let Some(v) = v {
            b.push(',');
            json_str(b, k);
            b.push(':');
            if quote { json_str(b, v) } else { b.push_str(v) }
        }
    };
    kv(&mut b, "user", s.map(|s| s.user.as_str()), true);
    kv(&mut b, "dbname", s.map(|s| s.database.as_str()), true);
    kv(&mut b, "pid", Some(&who.pid.to_string()), false);
    if let Some((h, p)) = &who.remote {
        kv(&mut b, "remote_host", Some(h), true);
        kv(&mut b, "remote_port", Some(&p.to_string()), false);
    }
    kv(&mut b, "session_id", Some(&session_id(who)), true);
    kv(&mut b, "line_num", Some(&line.to_string()), false);
    if client {
        kv(&mut b, "ps", Some(s.map_or("", |s| s.command_tag.as_str())), true);
    }
    kv(&mut b, "session_start", Some(&stamp(who.start, &f.tz, false)), true);
    kv(&mut b, "vxid", s.map(|s| s.vxid.as_str()).filter(|v| !v.is_empty()), true);
    kv(&mut b, "txid", Some(&s.map_or(0, |s| s.xid).to_string()), false);
    kv(&mut b, "error_severity", Some(m.severity().as_str()), true);
    // (00000 is sqlerrcode 0, left out.)
    kv(&mut b, "state_code", Some(m.code).filter(|c| *c != "00000"), true);
    kv(&mut b, "message", Some(m.text), true);
    kv(&mut b, "detail", m.detail, true);
    kv(&mut b, "hint", m.hint, true);
    kv(&mut b, "internal_query", m.internal.map(|(_, q)| q), true);
    if let Some((p, _)) = m.internal.filter(|(p, _)| *p > 0) {
        kv(&mut b, "internal_position", Some(&p.to_string()), false);
    }
    kv(&mut b, "context", m.context, true);
    if let Some(q) = m.statement_shown(f) {
        kv(&mut b, "statement", Some(q), true);
        if let Some(p) = m.cursorpos {
            kv(&mut b, "cursor_position", Some(&p.to_string()), false);
        }
    }
    if f.verbosity == Verbosity::Verbose
        && let Some((routine, file, l)) = &m.location
    {
        kv(&mut b, "func_name", Some(routine), true);
        kv(&mut b, "file_name", Some(file), true);
        kv(&mut b, "file_line_num", Some(l), false);
    }
    kv(&mut b, "application_name", s.map(|s| s.application_name.as_str()).filter(|a| !a.is_empty()), true);
    kv(&mut b, "backend_type", Some(who.backend_type), true);
    kv(&mut b, "query_id", Some(&s.map_or(0, |s| s.query_id).to_string()), false);
    b.push_str("}\n");
    b
}
