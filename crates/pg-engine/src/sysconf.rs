//! The server's configuration files as clients see them: pg_file_settings, pg_settings'
//! sourcefile / sourceline / pending_restart, ALTER SYSTEM, pg_reload_conf and
//! pg_conf_load_time.
//!
//! postgresql.conf is the pinned image's: the settings initdb and the image write, at their
//! lines, its locale lines what this cluster's initdb chose; nothing changes it.
//! postgresql.auto.conf is ALTER SYSTEM's, kept in the engine's data directory in PostgreSQL's
//! format (an operator may edit it; a reload reads it again). Both are reported under the
//! reference's data directory, as data_directory is. A reload (pg_reload_conf, SIGHUP to
//! `roll-engine`) evaluates them as ProcessConfigFile does and publishes a new `Loaded`, which
//! every session takes before its next message (`Session::poll_config`).

use crate::error::{Error, Result, code};
use crate::session::settings::{FileParam, file_param};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

pub const CONF_FILE: &str = "/var/lib/postgresql/18/docker/postgresql.conf";
pub const AUTO_FILE: &str = "/var/lib/postgresql/18/docker/postgresql.auto.conf";
/// postgresql.auto.conf's name in the data directory.
pub const AUTO_NAME: &str = "postgresql.auto.conf";

/// postgresql.conf's settings and their lines (the pinned image's).
const BASE: &[(u32, &str, &str)] = &[
    (60, "listen_addresses", "*"),
    (65, "max_connections", "100"),
    (132, "shared_buffers", "128MB"),
    (155, "dynamic_shared_memory_type", "posix"),
    (271, "max_wal_size", "1GB"),
    (272, "min_wal_size", "80MB"),
    (645, "log_timezone", "UTC"),
    (687, "autovacuum_worker_slots", "16"),
    (781, "datestyle", "iso, mdy"),
    (783, "timezone", "UTC"),
    (797, "lc_messages", "C"),
    (799, "lc_monetary", "C"),
    (800, "lc_numeric", "C"),
    (801, "lc_time", "C"),
    (807, "default_text_search_config", "pg_catalog.english"),
];

/// One setting line of a configuration file (or a line that doesn't parse: no name).
#[derive(Clone, Debug)]
pub struct Item {
    pub file: &'static str,
    pub line: u32,
    pub name: Option<String>,
    pub value: Option<String>,
    pub error: Option<&'static str>,
}

/// A configuration file's settings (ParseConfigFp): `name [=] value`, values quoted ('' and
/// backslash escapes) or not, `#` comments. Include directives are skipped.
pub fn parse(text: &str, file: &'static str) -> Vec<Item> {
    let mut out = Vec::new();
    for (n, l) in text.lines().enumerate() {
        let line = n as u32 + 1;
        match parse_line(l) {
            Ok(None) => {}
            Ok(Some((name, _))) if name.starts_with("include") && matches!(name.as_str(), "include" | "include_if_exists" | "include_dir") => {}
            Ok(Some((name, value))) => out.push(Item { file, line, name: Some(name), value: Some(value), error: None }),
            Err(()) => out.push(Item { file, line, name: None, value: None, error: Some("syntax error") }),
        }
    }
    out
}

fn parse_line(l: &str) -> std::result::Result<Option<(String, String)>, ()> {
    let s = l.trim_start();
    if s.is_empty() || s.starts_with('#') {
        return Ok(None);
    }
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$' || c == '.' || !c.is_ascii();
    let end = s.find(|c: char| !ident(c)).unwrap_or(s.len());
    let name = &s[..end];
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit() || c == '.' || c == '$') {
        return Err(());
    }
    let mut rest = s[end..].trim_start();
    if let Some(r) = rest.strip_prefix('=') {
        rest = r.trim_start();
    }
    let (value, rest) = match rest.strip_prefix('\'') {
        Some(q) => unquote(q)?,
        None => {
            let end = rest.find(|c: char| c.is_whitespace() || c == '#').unwrap_or(rest.len());
            if end == 0 {
                return Err(());
            }
            (rest[..end].to_string(), &rest[end..])
        }
    };
    let rest = rest.trim_start();
    if !rest.is_empty() && !rest.starts_with('#') {
        return Err(());
    }
    Ok(Some((name.to_string(), value)))
}

/// A quoted value after its opening quote (DeescapeQuotedString): the value and what follows.
fn unquote(s: &str) -> std::result::Result<(String, &str), ()> {
    let mut out = String::new();
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        match c {
            '\'' if it.peek().is_some_and(|(_, n)| *n == '\'') => {
                it.next();
                out.push('\'');
            }
            '\'' => return Ok((out, &s[i + 1..])),
            '\\' => match it.next().map(|(_, c)| c) {
                Some('b') => out.push('\u{8}'),
                Some('f') => out.push('\u{c}'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(d @ '0'..='7') => {
                    let mut v = d as u32 - '0' as u32;
                    for _ in 0..2 {
                        match it.peek() {
                            Some((_, o @ '0'..='7')) => {
                                v = v * 8 + (*o as u32 - '0' as u32);
                                it.next();
                            }
                            _ => break,
                        }
                    }
                    out.push(char::from_u32(v).unwrap_or('?'));
                }
                Some(o) => out.push(o),
                None => return Err(()),
            },
            c => out.push(c),
        }
    }
    Err(())
}

/// postgresql.auto.conf's text for these settings (write_auto_conf_file).
fn auto_text(items: &[(String, String)]) -> String {
    let mut out = String::from("# Do not edit this file manually!\n# It will be overwritten by the ALTER SYSTEM command.\n");
    for (name, value) in items {
        let escaped: String = value
            .chars()
            .flat_map(|c| if c == '\'' || c == '\\' { vec![c, c] } else { vec![c] })
            .collect();
        out.push_str(&format!("{name} = '{escaped}'\n"));
    }
    out
}

/// Where a value of the files comes from.
#[derive(Clone, Debug, PartialEq)]
pub struct FileValue {
    pub value: String,
    pub file: &'static str,
    pub line: u32,
}

/// The configuration files' values as of a load (the server's start or a reload).
#[derive(Clone, Debug, Default)]
pub struct Loaded {
    pub generation: u64,
    /// When it was loaded (pg_conf_load_time), microseconds since 2000-01-01.
    pub load_time: i64,
    /// Every known parameter the files set, by canonical name, its value normalized.
    pub values: HashMap<String, FileValue>,
    /// Custom names' (and extension libraries' parameters', which are custom names until the
    /// library is loaded) values as written, by lower-case name.
    customs: HashMap<String, String>,
    /// `values` that differ from the parameter's built-in default, all a session's lookups
    /// need (usually none: postgresql.conf's values are the defaults `gucs.tsv` lists).
    differing: HashMap<&'static str, String>,
    /// postmaster parameters whose file value differs from the running one.
    pub pending_restart: Vec<&'static str>,
}

impl Loaded {
    /// No files (a session outside any cluster).
    pub fn empty() -> Arc<Loaded> {
        static E: OnceLock<Arc<Loaded>> = OnceLock::new();
        E.get_or_init(Default::default).clone()
    }

    pub fn differing(&self, canon: &str) -> Option<&str> {
        if self.differing.is_empty() {
            return None;
        }
        self.differing.get(canon).map(String::as_str)
    }

    /// The custom names the files set.
    pub fn custom_names(&self) -> impl Iterator<Item = &String> {
        self.customs.keys()
    }

    pub fn custom(&self, lower: &str) -> Option<&str> {
        if self.customs.is_empty() {
            return None;
        }
        self.customs.get(lower).map(String::as_str)
    }

    /// Recompute `differing` after `values` changed.
    pub fn refresh_differing(&mut self) {
        self.differing = self
            .values
            .iter()
            .filter_map(|(k, f)| match file_param(k, &f.value) {
                FileParam::Known { canon, default, .. } if f.value != default => Some((canon, f.value.clone())),
                _ => None,
            })
            .collect();
    }
}

/// A pg_file_settings row.
#[derive(Clone, Debug)]
pub struct FileRow {
    pub item: Item,
    pub seqno: i32,
    pub applied: bool,
}

/// ProcessConfigFileInternal's evaluation of the files.
struct Outcome {
    rows: Vec<FileRow>,
    /// None: errors, nothing applied.
    loaded: Option<Loaded>,
    /// The LOG messages a reload writes.
    logs: Vec<String>,
}

fn known(name: &str) -> Option<(&'static str, &'static str)> {
    match file_param(name, "") {
        FileParam::Known { canon, context, .. } => Some((canon, context)),
        _ => None,
    }
}

/// `items` against the values running (`running`) and those the server started with
/// (`startup`; None: this is the start, and every value applies).
fn evaluate(items: Vec<Item>, running: &Loaded, startup: Option<&Loaded>) -> Outcome {
    let mut rows: Vec<FileRow> = items
        .into_iter()
        .enumerate()
        .map(|(i, item)| FileRow { item, seqno: i as i32 + 1, applied: false })
        .collect();
    let mut logs = Vec::new();
    let none_applied = |f: &str| format!("configuration file \"{f}\" contains errors; no changes were applied");
    // Syntax errors: nothing else is looked at.
    if let Some(f) = rows.iter().find(|r| r.item.name.is_none()).map(|r| r.item.file) {
        logs.push(none_applied(f));
        return Outcome { rows, loaded: None, logs };
    }
    let params: Vec<FileParam> = rows
        .iter()
        .map(|r| file_param(r.item.name.as_deref().unwrap_or(""), r.item.value.as_deref().unwrap_or("")))
        .collect();
    let n = rows.len();
    let mut ignore = vec![false; n];
    let mut bad = None;
    for i in 0..n {
        if matches!(params[i], FileParam::Unknown) {
            rows[i].item.error = Some("unrecognized configuration parameter");
            bad.get_or_insert(rows[i].item.file);
            continue;
        }
        // (A later line of the same name, as written, overrides it.)
        for j in 0..i {
            if rows[j].item.name == rows[i].item.name {
                ignore[j] = true;
            }
        }
    }
    if let Some(f) = bad {
        logs.push(none_applied(f));
        return Outcome { rows, loaded: None, logs };
    }
    let set: std::collections::HashSet<&'static str> = (0..n)
        .filter(|i| !ignore[*i])
        .filter_map(|i| match &params[i] {
            FileParam::Known { canon, .. } => Some(*canon),
            _ => None,
        })
        .collect();
    let mut l = Loaded::default();
    let mut pending: Vec<&'static str> = Vec::new();
    if let Some(s) = startup {
        // postmaster parameters keep the values the server started with.
        for (k, f) in &s.values {
            if known(k).is_some_and(|(_, c)| c == "postmaster") {
                l.values.insert(k.clone(), f.clone());
            }
        }
        // What the files no longer set.
        let mut gone: Vec<(&'static str, &'static str)> =
            running.values.keys().filter_map(|k| known(k)).filter(|(c, _)| !set.contains(c)).collect();
        gone.sort();
        for (canon, context) in gone {
            if context == "postmaster" {
                pending.push(canon);
                logs.push(format!("parameter \"{canon}\" cannot be changed without restarting the server"));
            } else {
                logs.push(format!("parameter \"{canon}\" removed from configuration file, reset to default"));
            }
        }
    }
    let mut partial = None;
    for i in 0..n {
        if ignore[i] {
            continue;
        }
        let (file, line) = (rows[i].item.file, rows[i].item.line);
        let name = rows[i].item.name.clone().unwrap_or_default();
        let raw = rows[i].item.value.clone().unwrap_or_default();
        let fv = |value: String| FileValue { value, file, line };
        let ok = match &params[i] {
            FileParam::Known { canon, context, module, value, default } => {
                if *module {
                    l.customs.insert(name.to_ascii_lowercase(), raw.clone());
                }
                let before = running.values.get(*canon).map_or(*default, |f| f.value.as_str());
                match (*context, value) {
                    ("internal", _) => false,
                    // (An extension library's parameter is a placeholder until it's loaded.)
                    (_, None) => *module,
                    ("postmaster", Some(v)) if startup.is_some() => {
                        let run = startup.and_then(|s| s.values.get(*canon)).map_or(*default, |f| f.value.as_str());
                        if v != run {
                            pending.push(canon);
                            logs.push(format!("parameter \"{name}\" cannot be changed without restarting the server"));
                            false
                        } else {
                            l.values.insert(canon.to_string(), fv(v.clone()));
                            true
                        }
                    }
                    (_, Some(v)) => {
                        if startup.is_some() && v != before {
                            logs.push(format!("parameter \"{name}\" changed to \"{raw}\""));
                        }
                        l.values.insert(canon.to_string(), fv(v.clone()));
                        true
                    }
                }
            }
            FileParam::Custom(lower) => {
                if startup.is_some() && running.custom(lower) != Some(raw.as_str()) {
                    logs.push(format!("parameter \"{name}\" changed to \"{raw}\""));
                }
                l.customs.insert(lower.clone(), raw.clone());
                true
            }
            FileParam::Unknown => false,
        };
        rows[i].applied = ok;
        if !ok {
            rows[i].item.error = Some("setting could not be applied");
            partial.get_or_insert(file);
        }
    }
    if let Some(f) = partial {
        logs.push(format!("configuration file \"{f}\" contains errors; unaffected changes were applied"));
    }
    pending.sort();
    pending.dedup();
    l.pending_restart = pending;
    l.refresh_differing();
    Outcome { rows, loaded: Some(l), logs }
}

fn now() -> i64 {
    crate::types::datetime::Clock::system().now
}

/// The cluster's configuration files.
pub struct ServerConfig {
    /// The data directory postgresql.auto.conf is kept in (None: in memory).
    dir: Option<PathBuf>,
    /// postgresql.conf's settings.
    base: Vec<Item>,
    /// postgresql.auto.conf's text when kept in memory; held while ALTER SYSTEM rewrites the
    /// file and while a reload reads it (AutoFileLock).
    auto: Mutex<String>,
    /// What the server started with (postmaster parameters keep it).
    startup: Arc<Loaded>,
    current: RwLock<Arc<Loaded>>,
    /// `current`'s generation, read without the lock by every session's next message.
    generation: AtomicU64,
    /// Where pg_hba.conf / pg_ident.conf come from (none: the server's one rule).
    auth: AuthFiles,
    /// pg_hba.conf's rules as last loaded without errors.
    hba: RwLock<Option<Arc<crate::hba::Hba>>>,
}

/// The operator's client authentication files (`roll-engine --hba-file`, `--hba`,
/// `--ident-file`).
#[derive(Clone, Debug, Default)]
pub struct AuthFiles {
    pub hba_file: Option<PathBuf>,
    /// pg_hba.conf lines given directly (when there's no file).
    pub hba_lines: Vec<String>,
    pub ident_file: Option<PathBuf>,
}

impl AuthFiles {
    /// pg_hba.conf's text now (None: no rules configured).
    fn hba_text(&self) -> Option<String> {
        match &self.hba_file {
            Some(p) => Some(std::fs::read_to_string(p).unwrap_or_default()),
            None if !self.hba_lines.is_empty() => Some(self.hba_lines.join("\n")),
            None => None,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> ServerConfig {
        ServerConfig::open(None, &[])
    }
}

impl ServerConfig {
    /// The files of a server whose data directory is `dir` (None: postgresql.auto.conf in
    /// memory), initdb having chosen `initdb` (lc_messages, lc_monetary, ...).
    pub fn open(dir: Option<PathBuf>, initdb: &[(String, String)]) -> ServerConfig {
        let base: Vec<Item> = BASE
            .iter()
            .map(|(line, name, value)| {
                let v = initdb.iter().find(|(k, _)| k == name).map_or(*value, |(_, v)| v.as_str());
                Item { file: CONF_FILE, line: *line, name: Some(name.to_string()), value: Some(v.to_string()), error: None }
            })
            .collect();
        let mut c = ServerConfig {
            dir,
            base,
            auto: Mutex::new(String::new()),
            startup: Arc::default(),
            current: RwLock::new(Loaded::empty()),
            generation: AtomicU64::new(1),
            auth: AuthFiles::default(),
            hba: RwLock::new(None),
        };
        let items = c.items(&c.read_auto(""));
        let mut l = match evaluate(items, &Loaded::default(), None) {
            Outcome { loaded: Some(l), .. } => l,
            Outcome { logs, .. } => {
                // (PostgreSQL refuses to start; the engine starts without postgresql.auto.conf.)
                for m in logs {
                    tracing::warn!("{m}");
                }
                evaluate(c.base.clone(), &Loaded::default(), None).loaded.unwrap_or_default()
            }
        };
        l.generation = 1;
        l.load_time = now();
        c.startup = Arc::new(l.clone());
        c.current = RwLock::new(Arc::new(l));
        c
    }

    fn read_auto(&self, memory: &str) -> String {
        match &self.dir {
            Some(d) => std::fs::read_to_string(d.join(AUTO_NAME)).unwrap_or_default(),
            None => memory.to_string(),
        }
    }

    fn items(&self, auto: &str) -> Vec<Item> {
        let mut items = self.base.clone();
        items.extend(parse(auto, AUTO_FILE));
        items
    }

    /// Client authentication from the operator's files (before the server takes
    /// connections). Errors in pg_hba.conf at the start leave every connection refused, as
    /// PostgreSQL refuses to start.
    pub fn set_auth_files(&mut self, auth: AuthFiles) {
        let hba = auth.hba_text().map(|t| crate::hba::Hba::parse(&t));
        if let Some(h) = hba.as_ref().filter(|h| h.has_errors()) {
            for l in h.lines.iter().filter(|l| l.error.is_some()) {
                tracing::error!(line = l.line, "pg_hba.conf: {}", l.error.as_deref().unwrap_or_default());
            }
        }
        *self.hba.write() = hba.map(|h| {
            if h.has_errors() { Arc::new(crate::hba::Hba::default()) } else { Arc::new(h) }
        });
        self.auth = auth;
    }

    /// pg_hba.conf's rules in effect (None: none configured, the server's one rule applies).
    pub fn hba(&self) -> Option<Arc<crate::hba::Hba>> {
        self.hba.read().clone()
    }

    /// pg_hba_file_rules: the file as it is now (None: none configured).
    pub fn hba_file_now(&self) -> Option<crate::hba::Hba> {
        self.auth.hba_text().map(|t| crate::hba::Hba::parse(&t))
    }

    /// pg_ident_file_mappings: the file as it is now.
    pub fn ident_file_now(&self) -> Vec<crate::hba::IdentLine> {
        self.auth
            .ident_file
            .as_ref()
            .map(|p| crate::hba::parse_ident(&std::fs::read_to_string(p).unwrap_or_default()))
            .unwrap_or_default()
    }

    /// The values sessions take now.
    pub fn loaded(&self) -> Arc<Loaded> {
        self.current.read().clone()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// A value the server started with (a postmaster parameter the engine itself uses).
    pub fn startup_value(&self, canon: &str) -> Option<&str> {
        self.startup.values.get(canon).map(|f| f.value.as_str())
    }

    /// ALTER SYSTEM SET `name` = `value` (None: RESET), or RESET ALL (`name` None): rewrite
    /// postgresql.auto.conf (a changed setting moves to its end), replacing the file
    /// atomically. The caller has checked the name, the value and the privileges.
    pub fn alter_system(&self, name: Option<&str>, value: Option<&str>) -> Result<()> {
        let mut g = self.auto.lock();
        let mut items: Vec<(String, String)> = Vec::new();
        if let Some(name) = name {
            let parsed = parse(&self.read_auto(&g), AUTO_FILE);
            if parsed.iter().any(|i| i.name.is_none()) {
                return Err(Error::new(
                    code::CONFIG_FILE_ERROR,
                    format!("could not parse contents of file \"{AUTO_NAME}\""),
                ));
            }
            items = parsed
                .into_iter()
                .filter_map(|i| Some((i.name?, i.value?)))
                .filter(|(k, _)| !k.eq_ignore_ascii_case(name))
                .collect();
            if let Some(v) = value {
                items.push((name.to_string(), v.to_string()));
            }
        }
        let text = auto_text(&items);
        match &self.dir {
            None => *g = text,
            Some(d) => {
                let tmp = d.join(format!("{AUTO_NAME}.tmp"));
                let write = || -> std::io::Result<()> {
                    use std::io::Write;
                    let mut f = std::fs::File::create(&tmp)?;
                    f.write_all(text.as_bytes())?;
                    f.sync_all()?;
                    std::fs::rename(&tmp, d.join(AUTO_NAME))
                };
                write().map_err(|e| {
                    Error::new(code::IO_ERROR, format!("could not write to file \"{AUTO_NAME}.tmp\": {e}"))
                })?;
            }
        }
        Ok(())
    }

    /// SIGHUP: read the files again and publish what they now give (the LOG messages the
    /// postmaster writes are returned).
    pub fn reload(&self) -> Vec<String> {
        let g = self.auto.lock();
        let running = self.loaded();
        let mut logs = vec!["received SIGHUP, reloading configuration files".to_string()];
        let out = evaluate(self.items(&self.read_auto(&g)), &running, Some(&self.startup));
        logs.extend(out.logs);
        // (load_hba: a file with errors leaves the rules as they were.)
        if let Some(h) = self.auth.hba_text().map(|t| crate::hba::Hba::parse(&t)) {
            if h.has_errors() {
                logs.push("pg_hba.conf was not reloaded".to_string());
            } else {
                *self.hba.write() = Some(Arc::new(h));
            }
        }
        if let Some(mut l) = out.loaded {
            l.generation = self.generation.load(Ordering::Acquire) + 1;
            l.load_time = now();
            let generation = l.generation;
            *self.current.write() = Arc::new(l);
            self.generation.store(generation, Ordering::Release);
        }
        logs
    }

    /// pg_file_settings (show_all_file_settings): the files as they are now, evaluated
    /// without applying them; and the postmaster parameters they'd change.
    pub fn file_settings(&self) -> (Vec<FileRow>, Vec<&'static str>) {
        let g = self.auto.lock();
        let out = evaluate(self.items(&self.read_auto(&g)), &self.loaded(), Some(&self.startup));
        (out.rows, out.loaded.map(|l| l.pending_restart).unwrap_or_default())
    }
}
