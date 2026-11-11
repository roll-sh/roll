//! The replication protocol's commands (repl_gram.y / repl_scanner.l): what a walsender
//! connection (`replication=database` or `replication=true`) runs besides SQL.

use crate::error::{Error, Result, code};

#[derive(Debug, Clone, PartialEq)]
pub enum SlotKind {
    Physical { reserve_wal: bool },
    Logical { plugin: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    IdentifySystem,
    Show(String),
    TimelineHistory(u32),
    CreateSlot {
        name: String,
        temporary: bool,
        kind: SlotKind,
        /// SNAPSHOT 'export' / 'use' / 'nothing' (None: the default, export).
        snapshot: Option<String>,
        two_phase: bool,
        failover: bool,
    },
    AlterSlot { name: String, failover: Option<bool>, two_phase: Option<bool> },
    ReadSlot(String),
    DropSlot { name: String, wait: bool },
    StartPhysical { slot: Option<String>, lsn: u64, timeline: Option<u32> },
    StartLogical { slot: String, lsn: u64, options: Vec<(String, Option<String>)> },
    BaseBackup,
    UploadManifest,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    /// A double-quoted identifier.
    Ident(String),
    Str(String),
    Lsn(u64),
    Num(String),
    LParen,
    RParen,
    Comma,
}

fn syntax(near: &str) -> Error {
    Error::new(code::SYNTAX_ERROR, format!("syntax error at or near \"{near}\""))
}

fn lex(s: &str) -> Result<Vec<Tok>> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() || c == ';' {
            i += 1;
            continue;
        }
        match c {
            '(' => out.push(Tok::LParen),
            ')' => out.push(Tok::RParen),
            ',' => out.push(Tok::Comma),
            '\'' | '"' => {
                let q = c;
                let mut v = String::new();
                i += 1;
                loop {
                    if i >= b.len() {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            if q == '\'' { "unterminated quoted string" } else { "unterminated quoted identifier" },
                        ));
                    }
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            v.push(q);
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    v.push(b[i]);
                    i += 1;
                }
                out.push(if q == '\'' { Tok::Str(v) } else { Tok::Ident(v) });
            }
            _ => {
                let start = i;
                while i < b.len() && !b[i].is_whitespace() && !matches!(b[i], '(' | ')' | ',' | ';' | '\'' | '"') {
                    i += 1;
                }
                let w: String = b[start..i].iter().collect();
                if let Some((hi, lo)) = w.split_once('/')
                    && let (Ok(h), Ok(l)) = (u32::from_str_radix(hi, 16), u32::from_str_radix(lo, 16))
                {
                    out.push(Tok::Lsn(((h as u64) << 32) | l as u64));
                } else if w.chars().all(|c| c.is_ascii_digit()) {
                    out.push(Tok::Num(w));
                } else {
                    out.push(Tok::Word(w));
                }
                continue;
            }
        }
        i += 1;
    }
    Ok(out)
}

const KEYWORDS: &[&str] = &[
    "identify_system",
    "show",
    "timeline_history",
    "create_replication_slot",
    "alter_replication_slot",
    "read_replication_slot",
    "drop_replication_slot",
    "start_replication",
    "base_backup",
    "upload_manifest",
];

/// Whether `sql` is a replication command (replication_scanner_is_replication_command):
/// its first word is one of theirs. (SHOW is SQL's too, and the replication grammar's.)
pub fn is_command(sql: &str) -> bool {
    let w: String = sql
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_lowercase();
    KEYWORDS.contains(&w.as_str())
}

struct P {
    t: Vec<Tok>,
    i: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.t.get(self.i).cloned();
        self.i += 1;
        t
    }
    fn word(&mut self, w: &str) -> bool {
        if let Some(Tok::Word(x)) = self.peek()
            && x.eq_ignore_ascii_case(w)
        {
            self.i += 1;
            return true;
        }
        false
    }
    fn err(&self) -> Error {
        match self.peek() {
            None => Error::new(code::SYNTAX_ERROR, "syntax error at end of input"),
            Some(Tok::Word(w) | Tok::Num(w)) => syntax(w),
            Some(Tok::Ident(w)) => syntax(&format!("\"{w}\"")),
            Some(Tok::Str(w)) => syntax(&format!("'{w}'")),
            Some(Tok::Lsn(l)) => syntax(&super::lsn::fmt(*l)),
            Some(Tok::LParen) => syntax("("),
            Some(Tok::RParen) => syntax(")"),
            Some(Tok::Comma) => syntax(","),
        }
    }
    /// An identifier (a slot or plugin name): unquoted ones are folded to lower case.
    fn ident(&mut self) -> Result<String> {
        match self.peek().cloned() {
            Some(Tok::Word(w)) => {
                self.i += 1;
                Ok(w.to_ascii_lowercase())
            }
            Some(Tok::Ident(w)) => {
                self.i += 1;
                Ok(w)
            }
            _ => Err(self.err()),
        }
    }
    fn lsn(&mut self) -> Result<u64> {
        match self.peek() {
            Some(Tok::Lsn(l)) => {
                let l = *l;
                self.i += 1;
                Ok(l)
            }
            _ => Err(self.err()),
        }
    }
    fn end(&self) -> Result<()> {
        if self.i < self.t.len() { Err(self.err()) } else { Ok(()) }
    }
    /// A generic option list `( name [value] [, ...] )`; values as text.
    fn options(&mut self) -> Result<Vec<(String, Option<String>)>> {
        let mut out = Vec::new();
        if !matches!(self.peek(), Some(Tok::LParen)) {
            return Ok(out);
        }
        self.i += 1;
        loop {
            let name = self.ident()?;
            let val = match self.peek().cloned() {
                Some(Tok::Str(s) | Tok::Num(s)) => {
                    self.i += 1;
                    Some(s)
                }
                Some(Tok::Word(w)) => {
                    self.i += 1;
                    Some(w)
                }
                Some(Tok::Ident(w)) => {
                    self.i += 1;
                    Some(w)
                }
                _ => None,
            };
            out.push((name, val));
            match self.next() {
                Some(Tok::Comma) => continue,
                Some(Tok::RParen) => break,
                _ => {
                    self.i -= 1;
                    return Err(self.err());
                }
            }
        }
        Ok(out)
    }
}

fn opt_bool(name: &str, v: &Option<String>) -> Result<bool> {
    match v.as_deref() {
        None => Ok(true),
        Some(s) => crate::types::text::parse_bool(s).map_err(|_| {
            Error::new(code::SYNTAX_ERROR, format!("{name} requires a Boolean value"))
        }),
    }
}

/// Parse a replication command.
pub fn parse(sql: &str) -> Result<Cmd> {
    let mut p = P { t: lex(sql)?, i: 0 };
    let cmd = if p.word("identify_system") {
        Cmd::IdentifySystem
    } else if p.word("show") {
        let mut name = p.ident()?;
        while let Some(Tok::Word(w)) = p.peek().cloned() {
            if let Some(rest) = w.strip_prefix('.') {
                p.i += 1;
                name.push('.');
                name.push_str(&rest.to_ascii_lowercase());
            } else {
                break;
            }
        }
        Cmd::Show(name)
    } else if p.word("timeline_history") {
        match p.next() {
            Some(Tok::Num(n)) => Cmd::TimelineHistory(n.parse().unwrap_or(0)),
            _ => {
                p.i -= 1;
                return Err(p.err());
            }
        }
    } else if p.word("create_replication_slot") {
        let name = p.ident()?;
        let temporary = p.word("temporary");
        let (mut snapshot, mut two_phase, mut failover) = (None, false, false);
        let kind = if p.word("physical") {
            let mut reserve_wal = p.word("reserve_wal");
            for (o, v) in p.options()? {
                match o.as_str() {
                    "reserve_wal" => reserve_wal = opt_bool("RESERVE_WAL", &v)?,
                    _ => return Err(Error::new(code::SYNTAX_ERROR, format!("unrecognized option: {o}"))),
                }
            }
            SlotKind::Physical { reserve_wal }
        } else if p.word("logical") {
            let plugin = p.ident()?;
            // The old syntax's options, or a list.
            loop {
                if p.word("export_snapshot") {
                    snapshot = Some("export".into());
                } else if p.word("noexport_snapshot") {
                    snapshot = Some("nothing".into());
                } else if p.word("use_snapshot") {
                    snapshot = Some("use".into());
                } else if p.word("two_phase") {
                    two_phase = true;
                } else {
                    break;
                }
            }
            for (o, v) in p.options()? {
                match o.as_str() {
                    "snapshot" => snapshot = v,
                    "two_phase" => two_phase = opt_bool("TWO_PHASE", &v)?,
                    "failover" => failover = opt_bool("FAILOVER", &v)?,
                    _ => return Err(Error::new(code::SYNTAX_ERROR, format!("unrecognized option: {o}"))),
                }
            }
            SlotKind::Logical { plugin }
        } else {
            return Err(p.err());
        };
        Cmd::CreateSlot { name, temporary, kind, snapshot, two_phase, failover }
    } else if p.word("alter_replication_slot") {
        let name = p.ident()?;
        let (mut failover, mut two_phase) = (None, None);
        for (o, v) in p.options()? {
            match o.as_str() {
                "failover" => failover = Some(opt_bool("FAILOVER", &v)?),
                "two_phase" => two_phase = Some(opt_bool("TWO_PHASE", &v)?),
                _ => return Err(Error::new(code::SYNTAX_ERROR, format!("unrecognized option: {o}"))),
            }
        }
        Cmd::AlterSlot { name, failover, two_phase }
    } else if p.word("read_replication_slot") {
        Cmd::ReadSlot(p.ident()?)
    } else if p.word("drop_replication_slot") {
        let name = p.ident()?;
        Cmd::DropSlot { name, wait: p.word("wait") }
    } else if p.word("start_replication") {
        if p.word("slot") {
            let slot = p.ident()?;
            if p.word("logical") {
                let lsn = p.lsn()?;
                let options = p.options()?;
                p.end()?;
                return Ok(Cmd::StartLogical { slot, lsn, options });
            }
            p.word("physical");
            let lsn = p.lsn()?;
            let timeline = if p.word("timeline") {
                match p.next() {
                    Some(Tok::Num(n)) => n.parse().ok(),
                    _ => None,
                }
            } else {
                None
            };
            Cmd::StartPhysical { slot: Some(slot), lsn, timeline }
        } else {
            p.word("physical");
            let lsn = p.lsn()?;
            let timeline = if p.word("timeline") {
                match p.next() {
                    Some(Tok::Num(n)) => n.parse().ok(),
                    _ => None,
                }
            } else {
                None
            };
            Cmd::StartPhysical { slot: None, lsn, timeline }
        }
    } else if p.word("base_backup") {
        // (Its options don't matter: it isn't supported.)
        p.i = p.t.len();
        Cmd::BaseBackup
    } else if p.word("upload_manifest") {
        Cmd::UploadManifest
    } else {
        return Err(p.err());
    };
    p.end()?;
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_commands_clients_send() {
        assert_eq!(parse("IDENTIFY_SYSTEM").unwrap(), Cmd::IdentifySystem);
        assert_eq!(
            parse("CREATE_REPLICATION_SLOT \"sub1\" LOGICAL pgoutput (SNAPSHOT 'nothing')").unwrap(),
            Cmd::CreateSlot {
                name: "sub1".into(),
                temporary: false,
                kind: SlotKind::Logical { plugin: "pgoutput".into() },
                snapshot: Some("nothing".into()),
                two_phase: false,
                failover: false,
            }
        );
        assert_eq!(
            parse("CREATE_REPLICATION_SLOT x TEMPORARY LOGICAL test_decoding USE_SNAPSHOT").unwrap(),
            Cmd::CreateSlot {
                name: "x".into(),
                temporary: true,
                kind: SlotKind::Logical { plugin: "test_decoding".into() },
                snapshot: Some("use".into()),
                two_phase: false,
                failover: false,
            }
        );
        match parse("START_REPLICATION SLOT \"s\" LOGICAL 0/1A2B (proto_version '4', publication_names '\"p\"', binary)").unwrap() {
            Cmd::StartLogical { slot, lsn, options } => {
                assert_eq!(slot, "s");
                assert_eq!(lsn, 0x1A2B);
                assert_eq!(options[1], ("publication_names".into(), Some("\"p\"".into())));
                assert_eq!(options[2], ("binary".into(), None));
            }
            c => panic!("{c:?}"),
        }
        assert_eq!(
            parse("DROP_REPLICATION_SLOT s WAIT").unwrap(),
            Cmd::DropSlot { name: "s".into(), wait: true }
        );
        assert!(is_command("  start_replication SLOT x LOGICAL 0/0"));
        assert!(!is_command("SELECT 1"));
    }
}
