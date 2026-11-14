//! Secret references (docs/secrets.md, "References"): `${KEY}`, `${env.KEY}`,
//! `${env.folder.sub.KEY}`, and `$${` for a literal `${`. Anything else between `${` and `}` is
//! literal text.

use crate::names;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Ref {
    /// The environment's slug; `None` for the same environment and folder.
    pub env: Option<String>,
    /// The folder: `/` for `${env.KEY}`; unused when `env` is `None`.
    pub path: String,
    pub key: String,
}

impl Ref {
    pub fn display(&self) -> String {
        match &self.env {
            None => self.key.clone(),
            Some(e) if self.path == "/" => format!("{e}.{}", self.key),
            Some(e) => format!("{e}.{}.{}", self.path[1..].replace('/', "."), self.key),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Part<'a> {
    Text(&'a str),
    /// `$${`: a literal `${`.
    Escaped,
    Ref(Ref, &'a str),
}

pub fn parse(v: &str) -> Vec<Part<'_>> {
    let mut out = vec![];
    let mut rest = v;
    while let Some(i) = rest.find("${") {
        if i > 0 && rest.as_bytes()[i - 1] == b'$' {
            if i > 1 {
                out.push(Part::Text(&rest[..i - 1]));
            }
            out.push(Part::Escaped);
            rest = &rest[i + 2..];
            continue;
        }
        let Some(end) = rest[i + 2..].find('}') else { break };
        let inner = &rest[i + 2..i + 2 + end];
        match reference(inner) {
            Some(r) => {
                if i > 0 {
                    out.push(Part::Text(&rest[..i]));
                }
                out.push(Part::Ref(r, &rest[i..i + 3 + end]));
            }
            None => out.push(Part::Text(&rest[..i + 3 + end])),
        }
        rest = &rest[i + 3 + end..];
    }
    if !rest.is_empty() {
        out.push(Part::Text(rest));
    }
    out
}

fn reference(inner: &str) -> Option<Ref> {
    let segs: Vec<&str> = inner.split('.').collect();
    let key = *segs.last()?;
    names::key(key).ok()?;
    if segs.len() == 1 {
        return Some(Ref { env: None, path: "/".into(), key: key.into() });
    }
    names::slug(segs[0]).ok()?;
    let folders = &segs[1..segs.len() - 1];
    if folders.len() > names::DEPTH_MAX || !folders.iter().all(|s| names::segment(s)) {
        return None;
    }
    let path = if folders.is_empty() { "/".to_string() } else { format!("/{}", folders.join("/")) };
    Some(Ref { env: Some(segs[0].into()), path, key: key.into() })
}

/// The references in a value, in order, without repeats.
pub fn refs(v: &str) -> Vec<Ref> {
    let mut out: Vec<Ref> = vec![];
    for p in parse(v) {
        if let Part::Ref(r, _) = p
            && !out.contains(&r)
        {
            out.push(r);
        }
    }
    out
}

/// `v` with references to environment `from` pointing at `to` instead; `None` if there were none.
pub fn rename_env(v: &str, from: &str, to: &str) -> Option<String> {
    let parts = parse(v);
    if !parts.iter().any(|p| matches!(p, Part::Ref(r, _) if r.env.as_deref() == Some(from))) {
        return None;
    }
    let mut out = String::with_capacity(v.len());
    for p in parts {
        match p {
            Part::Text(t) => out.push_str(t),
            Part::Escaped => out.push_str("$${"),
            Part::Ref(r, _) if r.env.as_deref() == Some(from) => out.push_str(&format!("${{{}}}", Ref { env: Some(to.into()), ..r }.display())),
            Part::Ref(_, raw) => out.push_str(raw),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_parse_escape_and_rename() {
        let r = refs("postgres://${USER}:${production.db.PASS}@${staging.HOST}/x ${not a ref} $${LITERAL}");
        assert_eq!(r.iter().map(Ref::display).collect::<Vec<_>>(), ["USER", "production.db.PASS", "staging.HOST"]);
        assert_eq!(r[1].path, "/db");
        assert_eq!(rename_env("a ${staging.X} ${X} $${staging.X}", "staging", "qa").unwrap(), "a ${qa.X} ${X} $${staging.X}");
        assert_eq!(rename_env("${prod.X}", "staging", "qa"), None);
    }
}
