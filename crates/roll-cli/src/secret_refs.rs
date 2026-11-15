//! `roll://` references (docs/secrets.md, "References in files"), as 1Password's `op://`: a file
//! committed with the shape of its configuration and no value. `roll://ENV/KEY` is a key in this
//! project's environment, `roll://ENV/folder/sub/KEY` one in a folder, `roll://PROJECT@ENV/KEY`
//! one in another project (its name or `prj_…` id). This module only reads them; `secrets.rs`
//! resolves them.

/// One reference.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ref {
    pub project: Option<String>,
    pub env: String,
    pub path: String,
    pub key: String,
}

impl Ref {
    /// The reference as written in a file: `roll://[PROJECT@]ENV[/folder]/KEY`.
    pub fn text(&self) -> String {
        let project = self.project.as_ref().map(|p| format!("{p}@")).unwrap_or_default();
        let folder = if self.path == "/" { "" } else { self.path.as_str() };
        format!("roll://{project}{}{folder}/{}", self.env, self.key)
    }

    /// `value` as a reference, if it's one (`roll://…`); an error if it's a malformed one.
    pub fn parse(value: &str) -> anyhow::Result<Option<Ref>> {
        let Some(rest) = value.trim().strip_prefix("roll://") else { return Ok(None) };
        let bad = || anyhow::anyhow!("{value}: a reference is roll://ENV/KEY, roll://ENV/folder/KEY or roll://PROJECT@ENV/KEY");
        let (project, rest) = match rest.split_once('@') {
            Some((p, r)) if !p.is_empty() && !p.contains('/') => (Some(p.to_string()), r),
            Some(_) => return Err(bad()),
            None => (None, rest),
        };
        let parts: Vec<&str> = rest.split('/').collect();
        let ok = |p: &&str| !p.is_empty() && *p != "." && *p != ".." && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
        if parts.len() < 2 || !parts.iter().all(ok) {
            return Err(bad());
        }
        let path = if parts.len() == 2 { "/".to_string() } else { format!("/{}", parts[1..parts.len() - 1].join("/")) };
        Ok(Some(Ref { project, env: parts[0].to_string(), path, key: parts[parts.len() - 1].to_string() }))
    }
}

/// A dotenv file's entries, in order: `KEY=value`, `export KEY=value`, values in single quotes
/// (as written) or double quotes (`\n`, `\"`, `\\` unescaped); comments and blank lines skipped.
pub fn parse_env_file(text: &str, file: &str) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = vec![];
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        // Errors name the line, never its text: it may hold a value.
        let (key, raw) = line.split_once('=').ok_or_else(|| anyhow::anyhow!("{file}:{}: not KEY=value", n + 1))?;
        let key = key.trim();
        if key.is_empty() || !key.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            anyhow::bail!("{file}:{}: not a variable name before =", n + 1);
        }
        let raw = raw.trim();
        let opened = raw.starts_with('"') || raw.starts_with('\'');
        if opened && (raw.len() < 2 || !raw.ends_with(&raw[..1])) {
            anyhow::bail!("{file}:{}: a quoted value runs past its line (multi-line values aren't read)", n + 1);
        }
        let value = if let Some(v) = raw.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            v.to_string()
        } else if let Some(v) = raw.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            let mut s = String::with_capacity(v.len());
            let mut chars = v.chars();
            while let Some(c) = chars.next() {
                match (c, c == '\\') {
                    (_, true) => match chars.next() {
                        Some('n') => s.push('\n'),
                        Some(other) => s.push(other),
                        None => s.push('\\'),
                    },
                    (c, false) => s.push(c),
                }
            }
            s
        } else {
            // Unquoted: up to an inline comment.
            raw.split(" #").next().unwrap_or_default().trim_end().to_string()
        };
        out.push((key.to_string(), value));
    }
    Ok(out)
}

/// The `roll://` references in a template's text, with where each starts and ends: a reference
/// runs to the first space, quote, bracket or comma.
pub fn find_in_text(text: &str) -> Vec<(usize, usize)> {
    let mut out = vec![];
    let mut from = 0;
    while let Some(i) = text[from..].find("roll://") {
        let start = from + i;
        let end = text[start..].find(|c: char| c.is_whitespace() || "\"'`)]},;<>".contains(c)).map_or(text.len(), |e| start + e);
        out.push((start, end));
        from = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_and_env_files_read_as_written() {
        let r = |v: &str| Ref::parse(v).unwrap().unwrap();
        assert_eq!(r("roll://production/DB_URL"), Ref { project: None, env: "production".into(), path: "/".into(), key: "DB_URL".into() });
        assert_eq!(r("roll://shop@staging/payments/stripe/KEY").path, "/payments/stripe");
        assert_eq!(r("roll://shop@staging/KEY").project.as_deref(), Some("shop"));
        assert!(Ref::parse("postgres://u:p@h/db").unwrap().is_none());
        assert!(Ref::parse("roll://production").is_err() && Ref::parse("roll://prod/../KEY").is_err());
        let file = "# config\nexport PORT=8080\nDB_URL=roll://production/DB_URL\nGREETING=\"hi\\nthere\"\nRAW='a#b'\nNOTE=plain # comment\n";
        assert_eq!(
            parse_env_file(file, ".env").unwrap(),
            [("PORT", "8080"), ("DB_URL", "roll://production/DB_URL"), ("GREETING", "hi\nthere"), ("RAW", "a#b"), ("NOTE", "plain")].map(|(k, v)| (k.to_string(), v.to_string()))
        );
        let tpl = "url: \"roll://production/DB_URL\"\nkey: roll://shop@staging/KEY, port: 1";
        let refused = parse_env_file("KEY=\"line one\nsecret=two\"\n", ".env").unwrap_err().to_string();
        assert!(refused.contains(".env:1") && !refused.contains("secret"), "{refused}");
        assert_eq!(find_in_text(tpl).iter().map(|(s, e)| &tpl[*s..*e]).collect::<Vec<_>>(), ["roll://production/DB_URL", "roll://shop@staging/KEY"]);
    }
}
