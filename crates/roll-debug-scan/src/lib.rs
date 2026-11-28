//! The `Debug` canary's source scan, shared by every crate that holds credentials (AGENTS.md: a
//! value or a credential never goes in a `Debug`).
//!
//! [`check`] walks a crate's sources for structs and enums that derive `Debug` with a field named
//! like a credential (`token`, `secret`, `password`, `hmac`…) held as text or bytes. Each one must
//! be printed by the crate's canary test (which asserts the credential doesn't show), or have a
//! line in the crate's reviewed list saying why it holds none. A type that holds one either keeps
//! it in a type that redacts itself or prints itself by hand. A new field can't be forgotten, and a
//! line for one that's gone fails too.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Parts of a field's name that mark it as a credential.
pub const NAMES: &[&str] = &["token", "secret", "password", "passwd", "passphrase", "private", "hmac", "credential", "seed", "api_key", "cookie", "authorization", "bearer"];

/// Panic, naming every unchecked field, unless each `derive(Debug)` type under `src` with a
/// credential-named text or bytes field is printed in `canary` (the source text of the crate's
/// canary test, as `::Type>` or `::Type {`) or has a `Type.field: why` line in `list` (the reviewed
/// list's text, `#` comments allowed). `list_name` names the list in the messages.
pub fn check(src: &Path, list_name: &str, list: &str, canary: &str) {
    let allowed: BTreeMap<&str, &str> = list
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (field, why) = l.split_once(": ").unwrap_or_else(|| panic!("{list_name}: `Type.field: why`, not {l}"));
            assert!(!why.trim().is_empty(), "{list_name}: {l}: say why");
            (field, why)
        })
        .collect();
    let mut found = BTreeSet::new();
    let mut missing = vec![];
    let mut files = vec![src.to_path_buf()];
    while let Some(p) = files.pop() {
        if p.is_dir() {
            files.extend(std::fs::read_dir(&p).unwrap().map(|e| e.unwrap().path()));
            continue;
        }
        if p.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&p).unwrap();
        for field in credential_fields(&text) {
            let ty = &field[..field.find('.').unwrap()];
            let canaried = canary.contains(&format!("::{ty}>")) || canary.contains(&format!("::{ty} {{"));
            if !canaried && !allowed.contains_key(field.as_str()) {
                missing.push(format!("{}: {field}", p.strip_prefix(src).unwrap().display()));
            }
            found.insert(field);
        }
    }
    missing.sort();
    assert!(missing.is_empty(), "derive(Debug) over a credential's name: keep it in a type that redacts itself, print the type by hand, give it a line in the canary test, or say why it holds none in {list_name}: {missing:#?}");
    let stale: Vec<&&str> = allowed.keys().filter(|f| !found.contains(**f)).collect();
    assert!(stale.is_empty(), "{list_name} names fields no longer found: {stale:?}");
}

/// `Type.field` for each credential-named text or bytes field of a `derive(Debug)` struct or enum
/// in one source file.
fn credential_fields(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = vec![];
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        i += 1;
        if !line.starts_with("#[derive(") {
            continue;
        }
        // The attribute (it may span lines), then the item after any others and its docs.
        let mut attr = line.to_string();
        while !attr.contains(")]") && i < lines.len() {
            attr.push_str(lines[i].trim());
            i += 1;
        }
        if !attr.split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == "Debug") {
            continue;
        }
        while i < lines.len() && (lines[i].trim().starts_with("#[") || lines[i].trim().starts_with("//")) {
            i += 1;
        }
        let Some(item) = lines.get(i).map(|l| l.trim()) else { break };
        let words: Vec<&str> = item.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| !w.is_empty()).collect();
        let Some(at) = words.iter().position(|w| *w == "struct" || *w == "enum") else { continue };
        let ty = words.get(at + 1).copied().unwrap_or_default();
        if !item.contains('{') {
            continue;
        }
        let mut depth = item.matches('{').count() as i64 - item.matches('}').count() as i64;
        i += 1;
        while depth > 0 && i < lines.len() {
            let l = lines[i].trim();
            i += 1;
            if !l.starts_with("//")
                && let Some((name, kind)) = l.trim_start_matches("pub(crate) ").trim_start_matches("pub(super) ").trim_start_matches("pub ").split_once(':')
                && name.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !kind.starts_with(':')
            {
                let named = name.to_ascii_lowercase();
                let text_or_bytes = kind.contains("String") || kind.contains("u8");
                if text_or_bytes && NAMES.iter().any(|n| named.contains(n)) {
                    out.push(format!("{ty}.{name}"));
                }
            }
            depth += l.matches('{').count() as i64 - l.matches('}').count() as i64;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// rolld, roll-edge and apps-control hold credentials too (clap's flags, Cloudflare's and AWS's
    /// keys, gates' passwords): each crate's sources against the list next to them. roll-secrets
    /// runs [`super::check`] itself, beside its canary test.
    #[test]
    fn beyond_roll_secrets() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        for (name, list) in [
            ("rolld", include_str!("../../rolld/src/debug-no-credential.txt")),
            ("roll-edge", include_str!("../../roll-edge/src/debug-no-credential.txt")),
            ("apps-control", include_str!("../../apps-control/src/debug-no-credential.txt")),
        ] {
            super::check(&crates.join(name).join("src"), &format!("{name}/src/debug-no-credential.txt"), list, "");
        }
    }
}
