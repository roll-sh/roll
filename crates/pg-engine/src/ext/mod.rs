//! Extensions: the contrib modules the engine implements natively.
//!
//! An extension here is what it is in PostgreSQL: a control file and SQL scripts (PostgreSQL
//! 18.6's own, embedded in `files/`), run by CREATE EXTENSION / ALTER EXTENSION UPDATE
//! (`session/ddl/extension.rs`), so its objects, their OIDs, dependencies and update paths
//! come out as PostgreSQL's do. What the scripts declare `LANGUAGE C` (`AS 'MODULE_PATHNAME',
//! 'symbol'`) is native Rust code, found by library and symbol (`native`).
//!
//! Only the extensions with their files here are available (pg_available_extensions):
//! a server where just these are installed.

mod files;
pub mod native;
pub mod auto_explain;
mod amcheck;
pub(crate) mod bloom;
pub(crate) mod binary;
mod btree;
mod citext;
mod cube;
pub mod dblink;
pub mod fdw;
pub mod file_fdw;
mod fuzzystrmatch;
mod hstore;
pub mod index_am;
mod intarray;
mod isn;
mod lo;
mod ltree;
mod pg_buffercache;
pub(crate) mod pg_trgm;
mod pgcrypto;
pub(crate) mod relinfo;
mod pgrowlocks;
mod dicts;
mod sslinfo;
mod spi;
mod xml2;
mod xslt;
mod tcn;
mod pgstattuple;
mod pg_visibility;
mod pg_freespacemap;
mod pg_prewarm;
pub mod pg_stat_statements;
pub mod postgres_fdw;
mod seg;
mod tablefunc;
pub mod tsm;
pub mod types;
mod unaccent;
mod uuid_ossp;
pub(crate) mod vector;

use std::collections::HashMap;
use std::sync::OnceLock;

/// An extension's control file (`ExtensionControlFile`).
#[derive(Clone, Debug, Default)]
pub struct Control {
    pub name: String,
    pub default_version: Option<String>,
    pub module_pathname: Option<String>,
    pub comment: Option<String>,
    pub schema: Option<String>,
    pub relocatable: bool,
    pub superuser: bool,
    pub trusted: bool,
    pub encoding: Option<String>,
    pub requires: Vec<String>,
    pub no_relocate: Vec<String>,
}

fn file(name: &str) -> Option<&'static str> {
    files::FILES.iter().find(|f| f.0 == name).map(|f| f.1)
}

/// The control files' values, by extension name.
fn controls() -> &'static HashMap<String, Control> {
    static C: OnceLock<HashMap<String, Control>> = OnceLock::new();
    C.get_or_init(|| {
        files::FILES
            .iter()
            .filter_map(|(f, text)| {
                let name = f.strip_suffix(".control")?;
                (!name.contains("--")).then(|| (name.to_string(), parse_control(name, text)))
            })
            .collect()
    })
}

/// `parse_extension_control_file`: `name = value` lines (`#` comments); a value is a quoted
/// string (`''` for a quote) or a bare word; lists are comma-separated names.
fn parse_control(name: &str, text: &str) -> Control {
    let mut c = Control {
        name: name.to_string(),
        relocatable: false,
        superuser: true,
        ..Default::default()
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        let v = match v.strip_prefix('\'') {
            Some(rest) => {
                let mut out = String::new();
                let mut it = rest.chars().peekable();
                while let Some(ch) = it.next() {
                    if ch == '\'' {
                        if it.peek() == Some(&'\'') {
                            it.next();
                            out.push('\'');
                            continue;
                        }
                        break;
                    }
                    out.push(ch);
                }
                out
            }
            None => v.split('#').next().unwrap_or("").trim().to_string(),
        };
        let list = |v: &str| -> Vec<String> {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let boolean = |v: &str| matches!(v.to_ascii_lowercase().as_str(), "true" | "on" | "yes" | "1");
        match k.trim() {
            "default_version" => c.default_version = Some(v),
            "module_pathname" => c.module_pathname = Some(v),
            "comment" => c.comment = Some(v),
            "schema" => c.schema = Some(v),
            "relocatable" => c.relocatable = boolean(&v),
            "superuser" => c.superuser = boolean(&v),
            "trusted" => c.trusted = boolean(&v),
            "encoding" => c.encoding = Some(v),
            "requires" => c.requires = list(&v),
            "no_relocate" => c.no_relocate = list(&v),
            _ => {}
        }
    }
    c
}

/// An available extension's control file.
pub fn control(name: &str) -> Option<&'static Control> {
    controls().get(name)
}

/// The available extensions' names, in name order.
pub fn names() -> Vec<&'static str> {
    let mut v: Vec<&str> = controls().keys().map(String::as_str).collect();
    v.sort_unstable();
    v
}

/// pg_available_extensions' rows: (name, default version, comment), in name order. Besides
/// the extensions here, the older stand-ins CREATE EXTENSION still accepts without running a
/// script (`session::ddl::EXTENSIONS`).
pub fn available() -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = names()
        .into_iter()
        .filter_map(|n| {
            let c = control(n)?;
            Some((n.to_string(), c.default_version.clone()?, c.comment.clone().unwrap_or_default()))
        })
        .collect();
    for (n, v, c) in crate::session::ddl::EXTENSIONS {
        if control(n).is_none() {
            out.push((n.to_string(), v.to_string(), c.to_string()));
        }
    }
    out.sort();
    out
}

/// pg_available_extension_versions' rows: name, version, superuser, trusted, relocatable,
/// schema, requires, comment.
pub fn available_version_rows() -> Vec<(String, String, bool, bool, bool, Option<String>, Vec<String>, String)> {
    let mut out = Vec::new();
    for (n, v, comment) in available() {
        match control(&n) {
            Some(c) => {
                for ver in available_versions(&n) {
                    out.push((
                        n.clone(),
                        ver,
                        c.superuser,
                        c.trusted,
                        c.relocatable,
                        c.schema.clone(),
                        c.requires.clone(),
                        comment.clone(),
                    ));
                }
            }
            None => out.push((n, v, true, false, true, None, Vec::new(), comment)),
        }
    }
    out
}

/// The C symbols extension `name`'s scripts bind (`AS 'MODULE_PATHNAME'[, 'symbol']`, the
/// function's own name without one): what its library exports.
pub fn script_symbols(name: &str) -> &'static std::collections::HashSet<String> {
    static S: OnceLock<std::sync::Mutex<HashMap<String, &'static std::collections::HashSet<String>>>> = OnceLock::new();
    let mut m = S.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = m.get(name) {
        return s;
    }
    let mut out = std::collections::HashSet::new();
    let prefix = format!("{name}--");
    for (_, text) in files::FILES.iter().filter(|(f, _)| f.starts_with(&prefix)) {
        for stmt in text.split(';') {
            let lower = stmt.to_ascii_lowercase();
            if !lower.contains("language c") {
                continue;
            }
            // `AS 'MODULE_PATHNAME'` or `AS '$libdir/name'`, then an optional symbol.
            let Some(i) = stmt.find("MODULE_PATHNAME'").map(|i| i + 16).or_else(|| {
                let j = stmt.find("'$libdir/")?;
                Some(j + 1 + stmt[j + 1..].find('\'')? + 1)
            }) else {
                continue;
            };
            // (After CREATE [OR REPLACE] FUNCTION: comments before it may say "function".)
            let Some(fp) = lower.find("create function ").map(|p| p + 16).or_else(|| lower.find("create or replace function ").map(|p| p + 27)) else {
                continue;
            };
            let fname: String = stmt[fp..].trim_start().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            let rest = stmt[i..].trim_start();
            let sym = rest
                .strip_prefix(',')
                .map(str::trim_start)
                .and_then(|r| r.strip_prefix('\''))
                .and_then(|r| r.split('\'').next())
                .map(str::to_string);
            out.insert(sym.unwrap_or(fname));
        }
    }
    let leaked: &'static std::collections::HashSet<String> = Box::leak(Box::new(out));
    m.insert(name.to_string(), leaked);
    leaked
}

/// The script installing `version` (`name--version.sql`), or updating `from` to `version`:
/// its file name and text.
pub fn script(name: &str, from: Option<&str>, version: &str) -> Option<(String, &'static str)> {
    let f = match from {
        Some(from) => format!("{name}--{from}--{version}.sql"),
        None => format!("{name}--{version}.sql"),
    };
    let text = file(&f)?;
    Some((f, text))
}

/// A version in the update graph (`ExtensionVersionInfo`).
#[derive(Clone, Debug)]
pub struct Version {
    pub name: String,
    /// Versions an update script leads to, in file order.
    pub reachable: Vec<usize>,
    /// Has an install script.
    pub installable: bool,
}

/// `get_ext_ver_list`: every version an install or update script names, in the order the
/// scripts are listed (by file name).
pub fn version_graph(name: &str) -> Vec<Version> {
    fn index(out: &mut Vec<Version>, v: &str) -> usize {
        if let Some(i) = out.iter().position(|x| x.name == v) {
            return i;
        }
        out.push(Version {
            name: v.to_string(),
            reachable: Vec::new(),
            installable: false,
        });
        out.len() - 1
    }
    let mut out: Vec<Version> = Vec::new();
    let prefix = format!("{name}--");
    for (f, _) in files::FILES {
        let Some(rest) = f.strip_prefix(&prefix).and_then(|r| r.strip_suffix(".sql")) else {
            continue;
        };
        match rest.split_once("--") {
            None => {
                let i = index(&mut out, rest);
                out[i].installable = true;
            }
            Some((from, to)) => {
                if to.contains("--") {
                    continue;
                }
                let a = index(&mut out, from);
                let b = index(&mut out, to);
                out[a].reachable.push(b);
            }
        }
    }
    out
}

/// The index of version `v` in `graph`, added when no script names it (`get_ext_ver_info`).
pub fn version_index(graph: &mut Vec<Version>, v: &str) -> usize {
    if let Some(i) = graph.iter().position(|x| x.name == v) {
        return i;
    }
    graph.push(Version {
        name: v.to_string(),
        reachable: Vec::new(),
        installable: false,
    });
    graph.len() - 1
}

/// `find_update_path`: the fewest update steps from `start` to `target` (ties broken by the
/// earlier version name, as PostgreSQL does), the versions after `start`; None when
/// unreachable. `reject_indirect`: don't pass through installable versions.
pub fn update_path(graph: &[Version], start: usize, target: usize, reject_indirect: bool) -> Option<Vec<String>> {
    let n = graph.len();
    let mut dist = vec![usize::MAX; n];
    let mut known = vec![false; n];
    let mut prev: Vec<Option<usize>> = vec![None; n];
    dist[start] = 0;
    loop {
        // The first unprocessed vertex at the least distance (get_nearest_unprocessed_vertex).
        let mut next: Option<usize> = None;
        for i in 0..n {
            if !known[i] && next.is_none_or(|j| dist[i] < dist[j]) {
                next = Some(i);
            }
        }
        let Some(v) = next else {
            break;
        };
        if dist[v] == usize::MAX {
            break;
        }
        known[v] = true;
        if v == target {
            break;
        }
        for &w in &graph[v].reachable {
            if reject_indirect && graph[w].installable {
                continue;
            }
            let d = dist[v] + 1;
            if d < dist[w] {
                dist[w] = d;
                prev[w] = Some(v);
            } else if d == dist[w]
                && let Some(p) = prev[w]
                && graph[v].name < graph[p].name
            {
                prev[w] = Some(v);
            }
        }
    }
    if !known[target] {
        return None;
    }
    let mut path = Vec::new();
    let mut v = target;
    while v != start {
        path.push(graph[v].name.clone());
        v = prev[v]?;
    }
    path.reverse();
    Some(path)
}

/// `find_install_path`: the installable version to start from for `target` (one without an
/// install script of its own) and the updates after it.
pub fn install_path(graph: &[Version], target: usize) -> Option<(usize, Vec<String>)> {
    let mut best: Option<(usize, Vec<String>)> = None;
    for (i, v) in graph.iter().enumerate() {
        if !v.installable || i == target {
            continue;
        }
        let Some(path) = update_path(graph, i, target, true) else {
            continue;
        };
        let better = match &best {
            None => true,
            Some((s, p)) => path.len() < p.len() || (path.len() == p.len() && graph[*s].name < v.name),
        };
        if better {
            best = Some((i, path));
        }
    }
    best
}

/// pg_available_extension_versions' versions of `name`: each installable one, then the
/// versions only an update from it reaches (`get_available_versions_for_extension`).
pub fn available_versions(name: &str) -> Vec<String> {
    let g = version_graph(name);
    let mut out = Vec::new();
    for (i, v) in g.iter().enumerate() {
        if !v.installable {
            continue;
        }
        out.push(v.name.clone());
        for (j, w) in g.iter().enumerate() {
            if w.installable {
                continue;
            }
            if install_path(&g, j).is_some_and(|(s, _)| s == i) {
                out.push(w.name.clone());
            }
        }
    }
    out
}

fn bad_name(what: &str, v: &str, detail: &str) -> crate::Error {
    crate::Error::new(
        crate::error::code::INVALID_PARAMETER_VALUE,
        format!("invalid extension {what}: \"{v}\""),
    )
    .detail(detail.to_string())
}

/// `check_valid_version_name`.
pub fn check_version_name(v: &str) -> crate::Result<()> {
    let w = "version name";
    if v.is_empty() {
        return Err(bad_name(w, v, "Version names must not be empty."));
    }
    if v.contains("--") {
        return Err(bad_name(w, v, "Version names must not contain \"--\"."));
    }
    if v.starts_with('-') || v.ends_with('-') {
        return Err(bad_name(w, v, "Version names must not begin or end with \"-\"."));
    }
    if v.contains('/') || v.contains('\\') {
        return Err(bad_name(w, v, "Version names must not contain directory separator characters."));
    }
    Ok(())
}

/// `check_valid_extension_name`.
pub fn check_extension_name(n: &str) -> crate::Result<()> {
    let w = "name";
    if n.is_empty() {
        return Err(bad_name(w, n, "Extension names must not be empty."));
    }
    if n.contains("--") {
        return Err(bad_name(w, n, "Extension names must not contain \"--\"."));
    }
    if n.starts_with('-') || n.ends_with('-') {
        return Err(bad_name(w, n, "Extension names must not begin or end with \"-\"."));
    }
    if n.contains('/') || n.contains('\\') {
        return Err(bad_name(w, n, "Extension names must not contain directory separator characters."));
    }
    Ok(())
}

/// `json_categorize_type`'s JSONTYPE_CAST for a value of an extension's native type: its
/// type's cast to json (`jsonb`: to jsonb) by function, run on it. None when there is none
/// (the value is then a JSON string of its text).
pub fn json_cast(v: &crate::types::Value, ty: &crate::types::DataType, jsonb: bool) -> Option<crate::Result<crate::types::Value>> {
    use crate::types::{DataType, Value};
    let Value::Ext(kind, _) = v else {
        return None;
    };
    let target = if jsonb { 3802 } else { 114 };
    crate::session::current::with_current(|ctx| {
        let cat = ctx.catalog();
        // (The value's own type when the declared one isn't known: an anonymous record's
        // field.)
        let oid = match ty {
            DataType::Base(b) => b.oid,
            _ => cat.base_types.values().find(|t| t.ty.ext == *kind)?.ty.oid,
        };
        let cast = cat
            .casts_all()
            .into_iter()
            .find(|x| x.source == oid && x.target == target && x.method == 'f')?;
        let f = cat.functions.get(&cast.func)?.clone();
        if f.language == "c" {
            let def = crate::sql::binder::Binder::native_def(&f)?;
            let args = [crate::sql::expr::Expr::Const(Value::Null, ty.clone())];
            let fc = crate::sql::func::FnCtx {
                exec: ctx,
                ret: &f.ret,
                args: &args,
                coll: None,
            };
            return Some((def.imp)(std::slice::from_ref(v), &fc));
        }
        Some(
            crate::plpgsql::call_function(ctx, &f, vec![v.clone()])
                .map(|rows| rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null)),
        )
    })
    .flatten()
    .map(|r| {
        r.map(|x| match (x, jsonb) {
            (Value::Text(s), false) => Value::Json(s),
            (x, _) => x,
        })
    })
}

/// The native subscripting of type `ty` (its SUBSCRIPT handler a native one).
pub fn native_subscript(cat: &crate::catalog::Catalog, ty: &crate::types::DataType) -> Option<native::NativeSubscript> {
    let crate::types::DataType::Base(b) = ty else {
        return None;
    };
    if b.ext == 0 {
        return None;
    }
    let def = cat.base_types.get(&b.oid)?;
    let f = cat.functions.get(&def.subscript)?;
    if f.language != "c" {
        return None;
    }
    native::subscript(f.probin.as_deref()?, &f.body)
}

/// `parse_extension_control_file`'s error for an extension that isn't installed here (no
/// control file on `extension_control_path`).
pub fn not_available(name: &str) -> crate::Error {
    use crate::error::code;
    crate::Error::new(code::FEATURE_NOT_SUPPORTED, format!("extension \"{name}\" is not available"))
        .hint("The extension must first be installed on the system where PostgreSQL is running.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pgcrypto_paths() {
        let g = version_graph("pgcrypto");
        let t = g.iter().position(|v| v.name == "1.4").unwrap();
        let (s, path) = install_path(&g, t).unwrap();
        assert_eq!(g[s].name, "1.3");
        assert_eq!(path, vec!["1.4".to_string()]);
        let from = g.iter().position(|v| v.name == "1.0").unwrap();
        assert_eq!(update_path(&g, from, t, false).unwrap(), vec!["1.1", "1.2", "1.3", "1.4"]);
        assert_eq!(available_versions("pgcrypto"), vec!["1.3", "1.4"]);
        let c = control("pgcrypto").unwrap();
        assert!(c.relocatable && c.trusted && c.superuser);
        assert_eq!(c.module_pathname.as_deref(), Some("$libdir/pgcrypto"));
    }

    #[test]
    fn symbols_of_scripts() {
        let s = script_symbols("btree_gist");
        assert!(s.contains("gbt_date_consistent") && s.contains("int4_dist") && !s.contains("date"));
        assert!(script_symbols("pgcrypto").contains("pg_digest"));
    }
}
