//! Form submissions for static sites (Netlify Forms; docs/apps/README.md, "Forms"): a static
//! site's HTML forms marked `data-roll-form` (or Netlify's `data-netlify="true"` / `netlify`) are
//! found once when its files are read. A POST to the page they're on, or to their `action`, with
//! their fields (urlencoded, at most 64 KiB) is kept per environment: sealed, at most 50 fields,
//! the newest 1,000 a form, nothing about the person who sent it. A filled honeypot field
//! (`netlify-honeypot`, `data-roll-honeypot`) is answered as a success and dropped. Each kept one
//! is a `form.submitted` event naming its fields, never their values; the values come back only
//! through `…/forms/{form}/submissions`, which is a reveal.

use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// A form's body's most bytes, and its fields'.
pub const MAX_BODY: usize = 64 << 10;
const MAX_FIELDS: usize = 50;
const MAX_NAME: usize = 100;
const MAX_VALUE: usize = 10_000;
/// Forms a site may mark, and the HTML read to find them.
const MAX_FORMS: usize = 50;
const MAX_HTML_FILES: usize = 500;
pub const MAX_HTML_BYTES: u64 = 256 << 10;
/// Submissions kept a form (the oldest go), and taken a minute an environment.
pub const KEEP: usize = 1_000;
const PER_MINUTE: u32 = 600;
const PER_CLIENT: u32 = 10;

/// A submission's fields, in the order they came: name and value.
pub type Fields = Vec<(String, String)>;
/// A kept submission: its id, when (ms), and its fields.
pub type Submission = (String, i64, Fields);

/// A form a site marked: the field that, filled, says a bot sent it, where it posts, and the
/// page it's on.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct FormSpec {
    pub honeypot: Option<String>,
    /// Its `action` when that's a path on the site (none: the page it's on).
    pub action: Option<String>,
    /// The page it's on (`/contact.html`).
    pub page: String,
}

/// One `<form …>` tag's attributes, read as a browser does: names lowercased, values quoted
/// (`"…"`, `'…'`) or bare, a name with no `=` present with an empty value. Where the tag ends:
/// the first `>` outside quotes. None: no `>` before the text ends.
fn tag_attrs(html: &str) -> Option<(HashMap<String, String>, usize)> {
    let b = html.as_bytes();
    let mut i = 0;
    let mut out = HashMap::new();
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        if b[i] == b'>' {
            return Some((out, i + 1));
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let name = html[start..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            match b.get(i) {
                Some(&q @ (b'"' | b'\'')) => {
                    let end = html[i + 1..].find(q as char)? + i + 1;
                    value = html[i + 1..end].to_string();
                    i = end + 1;
                }
                _ => {
                    let v = i;
                    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                        i += 1;
                    }
                    value = html[v..i].to_string();
                }
            }
        }
        if !name.is_empty() {
            out.entry(name).or_insert(value);
        }
    }
}

/// The forms an HTML page marks (`page` is its path on the site).
pub fn scan(html: &str, page: &str) -> Vec<(String, FormSpec)> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("<form") {
        let start = from + i + "<form".len();
        // `<formula>` isn't a form.
        if lower[start..].starts_with(|c: char| !c.is_ascii_whitespace() && c != '>' && c != '/') {
            from = start;
            continue;
        }
        let Some((attrs, len)) = tag_attrs(&html[start..]) else { break };
        from = start + len;
        let has = |k: &str| attrs.get(k);
        let marked = has("data-roll-form").is_some() || has("data-netlify").is_some_and(|v| v != "false") || has("netlify").is_some();
        if !marked {
            continue;
        }
        let Some(name) = has("name").filter(|n| !n.is_empty() && n.len() <= MAX_NAME && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_ ".contains(&b))).cloned() else { continue };
        let honeypot = has("data-roll-honeypot").or_else(|| has("netlify-honeypot")).filter(|h| !h.is_empty()).cloned();
        let action = has("action").filter(|a| a.starts_with('/') && !a.starts_with("//")).map(|a| a.split(['?', '#']).next().unwrap_or("/").to_string());
        out.push((name, FormSpec { honeypot, action, page: page.to_string() }));
    }
    out
}

/// A site's marked forms, from its HTML files (the first [`MAX_HTML_FILES`], each at most
/// [`MAX_HTML_BYTES`]): by name, the first page that marks one winning.
pub fn site_forms(pages: impl Iterator<Item = (String, String)>) -> HashMap<String, FormSpec> {
    let mut forms = HashMap::new();
    for (path, html) in pages.take(MAX_HTML_FILES) {
        for (name, spec) in scan(&html, &path) {
            if forms.len() < MAX_FORMS {
                forms.entry(name).or_insert(spec);
            }
        }
    }
    forms
}

/// The page path a file answers (`contact/index.html` is `/contact/`, `about.html` `/about.html`).
pub fn page_of(file: &str) -> String {
    match file.strip_suffix("index.html") {
        Some(dir) => format!("/{dir}"),
        None => format!("/{file}"),
    }
}

/// Which form a POST to `path` is: the one its `form-name` names, else the only one posting
/// there (its action) or on that page (a script's own POST).
pub fn which<'a>(forms: &'a HashMap<String, FormSpec>, path: &str, named: Option<&str>) -> Option<(&'a str, &'a FormSpec)> {
    if let Some(n) = named {
        return forms.get_key_value(n).map(|(k, v)| (k.as_str(), v));
    }
    let norm = |p: &str| p.trim_end_matches("index.html").trim_end_matches(".html").trim_end_matches('/').to_string();
    let here: Vec<(&String, &FormSpec)> = forms.iter().filter(|(_, f)| f.action.as_deref().is_some_and(|a| norm(a) == norm(path)) || norm(&f.page) == norm(path)).collect();
    match here.as_slice() {
        [(k, v)] => Some((k.as_str(), v)),
        _ => None,
    }
}

/// Whether any form posts to `path` (its action) or is on that page.
fn posts_here(forms: &HashMap<String, FormSpec>, path: &str) -> bool {
    let norm = |p: &str| p.trim_end_matches("index.html").trim_end_matches(".html").trim_end_matches('/').to_string();
    forms.values().any(|f| f.action.as_deref().is_some_and(|a| norm(a) == norm(path)) || norm(&f.page) == norm(path))
}

/// A urlencoded body's fields, bounded: `form-name` apart. None: not a form body roll takes.
pub fn fields(body: &[u8]) -> Option<(Option<String>, Fields)> {
    let mut name = None;
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in url::form_urlencoded::parse(body) {
        if k == "form-name" {
            name = Some(v.into_owned());
            continue;
        }
        if k.is_empty() || k.len() > MAX_NAME || v.len() > MAX_VALUE || k.chars().any(char::is_control) {
            return None;
        }
        // A repeated field (checkboxes) is one, its values joined.
        if let Some((_, old)) = out.iter_mut().find(|(n, _)| *n == k) {
            if old.len() + v.len() > MAX_VALUE {
                return None;
            }
            old.push_str(", ");
            old.push_str(&v);
        } else if out.len() < MAX_FIELDS {
            out.push((k.into_owned(), v.into_owned()));
        } else {
            return None;
        }
    }
    Some((name, out))
}

impl Apps {
    /// A POST to a static site that names one of its forms: kept (or a honeypot's, dropped), and
    /// where the sender goes next (its action's page, or roll's thanks). None: not a form's (no
    /// form posts to `path` and its body names none), so the site answers as it would anyway.
    /// `body` None: it was bigger than [`MAX_BODY`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn form_post(self: &Arc<Self>, forms: &HashMap<String, FormSpec>, service: &str, env: &str, path: &str, client: Option<&str>, content_type: Option<&str>, body: Option<&[u8]>) -> Option<std::result::Result<Option<String>, (u16, String)>> {
        if forms.is_empty() {
            return None;
        }
        let urlencoded = content_type.is_some_and(|c| c.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/x-www-form-urlencoded"));
        let parsed = body.filter(|_| urlencoded).map(fields);
        // Which form, before anything about the body is judged: one a POST elsewhere names, or
        // one posting here. A POST that's no form's keeps the site's own answer (405).
        let named = parsed.as_ref().and_then(|p| p.as_ref()).and_then(|(n, _)| n.as_deref());
        let (form, spec) = match which(forms, path, named) {
            Some(f) => f,
            None if named.is_none() && posts_here(forms, path) => {
                return Some(Err(match (body, urlencoded) {
                    (None, _) => (413, "This form's fields are too long to take.".into()),
                    (_, false) => (415, "This form is taken urlencoded, without files: send it without them.".into()),
                    _ => (400, "Two forms post here: send its form-name.".into()),
                }));
            }
            None => return None,
        };
        let Some(Some((_, fields))) = parsed else {
            return Some(Err(match (body, urlencoded) {
                (None, _) => (413, "This form's fields are too long to take.".into()),
                (_, false) => (415, "This form is taken urlencoded, without files: send it without them.".into()),
                _ => (400, "This form's fields are too many or too long.".into()),
            }));
        };
        let next = spec.action.clone();
        if let Some(h) = &spec.honeypot
            && fields.iter().any(|(k, v)| k == h && !v.trim().is_empty())
        {
            return Some(Ok(next));
        }
        let fields: Fields = fields.into_iter().filter(|(k, _)| Some(k) != spec.honeypot.as_ref()).collect();
        // A sender at a time (one bot can't make everyone's form refuse), and the environment.
        let minute = code_sql::now_ms() / 60_000;
        {
            let mut seen = self.forms_seen.lock();
            if seen.len() > 10_000 {
                seen.clear();
            }
            let env_key = crate::env_key(service, env);
            let mut take = |k: String, max: u32| {
                let n = seen.entry(k).or_insert((minute, 0));
                if n.0 != minute {
                    *n = (minute, 0);
                }
                n.1 += 1;
                n.1 <= max
            };
            let ok = take(format!("{env_key}|{}", client.unwrap_or("-")), PER_CLIENT) && take(env_key, PER_MINUTE);
            if !ok {
                return Some(Err((429, "Too many submissions just now: try again in a minute.".into())));
            }
        }
        let names: Vec<String> = fields.iter().map(|(k, _)| k.clone()).collect();
        let (sid, e, f) = (service.to_string(), env.to_string(), form.to_string());
        match self.db(move |s| s.add_form_submission(&sid, &e, &f, &fields)).await {
            Ok(id) => {
                let (sid, e, data) = (service.to_string(), env.to_string(), json!({ "form": form, "submission": id, "fields": names }));
                let _ = self.db(move |s| s.add_event("form.submitted", &sid, &e, None, &data)).await;
                Some(Ok(next))
            }
            Err(e) => Some(Err((503, format!("The submission wasn't kept: {e}")))),
        }
    }

    /// `GET …/environments/{env}/forms`: its forms with submissions kept, and how many.
    pub async fn forms(self: &Arc<Self>, service: &str, env: &str) -> Result<Value> {
        self.service_env(service, env).await?;
        let (sid, e) = (service.to_string(), env.to_string());
        let list = self.db(move |s| s.form_counts(&sid, &e)).await?;
        Ok(Value::Array(list.into_iter().map(|(form, n, last)| json!({ "name": form, "submissions": n, "last_at": last })).collect()))
    }

    /// `GET …/forms/{form}/submissions?limit=&before=`: the newest first, values in the clear;
    /// `before` a submission's id (the page after it).
    pub async fn form_submissions(self: &Arc<Self>, service: &str, env: &str, form: &str, limit: usize, before: Option<String>) -> Result<Value> {
        self.service_env(service, env).await?;
        let (sid, e, f) = (service.to_string(), env.to_string(), form.to_string());
        let list = self.db(move |s| s.form_submissions(&sid, &e, &f, limit.clamp(1, 200), before)).await?;
        Ok(Value::Array(
            list.into_iter()
                .map(|(id, at, fields)| json!({ "id": id, "created_at": at, "fields": fields.into_iter().map(|(k, v)| json!({ "name": k, "value": v })).collect::<Vec<_>>() }))
                .collect(),
        ))
    }

    pub async fn delete_form_submission(self: &Arc<Self>, service: &str, env: &str, form: &str, id: &str) -> Result<()> {
        self.service_env(service, env).await?;
        let (sid, e, f, i) = (service.to_string(), env.to_string(), form.to_string(), id.to_string());
        if !self.db(move |s| s.delete_form_submission(&sid, &e, &f, &i)).await? {
            return Err(Error::NotFound(format!("no submission {id}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marked_forms_are_found_with_their_honeypot_and_action() {
        let html = r#"<form name="search" action="/s"><input name=q></form>
            <FORM name="contact" method="POST" data-netlify="true" netlify-honeypot="bot-field" action="/thanks.html?x=1">
            <form name='signup' data-roll-form><input name="email"></form>
            <form netlify name=plain>"#;
        let forms = scan(html, "/contact.html");
        let names: Vec<&str> = forms.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["contact", "signup", "plain"], "an unmarked form isn't one");
        assert_eq!(forms[0].1, FormSpec { honeypot: Some("bot-field".into()), action: Some("/thanks.html".into()), page: "/contact.html".into() });
        assert_eq!((forms[1].1.action.as_deref(), forms[2].1.honeypot.as_deref()), (None, None));
        assert!(scan(r#"<form data-netlify="false" name="x">"#, "/").is_empty());
        // A word in another attribute's value isn't an attribute; a `>` in a value doesn't end the tag.
        assert!(scan(r#"<form class="card netlify" name="x">"#, "/").is_empty());
        assert_eq!(scan(r#"<form title="your name" data-roll-form name="real" data-x="a>b">"#, "/").first().map(|(n, _)| n.as_str()), Some("real"));
        assert!(scan("<formula netlify name=x>", "/").is_empty());
        assert!(scan(r#"<form data-roll-form name="x" action="https://evil.example/">"#, "/")[0].1.action.is_none(), "an action off the site isn't followed");
    }

    #[test]
    fn a_post_finds_its_form_and_fields_stay_bounded() {
        let forms: HashMap<String, FormSpec> = [("contact".to_string(), FormSpec { honeypot: None, action: None, page: "/contact/".into() }), ("news".to_string(), FormSpec { honeypot: None, action: Some("/thanks".into()), page: "/".into() })].into_iter().collect();
        assert_eq!(which(&forms, "/contact/index.html", None).map(|(n, _)| n), Some("contact"));
        assert_eq!(which(&forms, "/thanks", None).map(|(n, _)| n), Some("news"), "its action");
        assert_eq!(which(&forms, "/", Some("contact")).map(|(n, _)| n), Some("contact"), "form-name decides");
        assert!(which(&forms, "/other", None).is_none());
        let (name, f) = fields(b"form-name=contact&email=a%40b.c&tags=x&tags=y").unwrap();
        assert_eq!((name.as_deref(), f), (Some("contact"), vec![("email".into(), "a@b.c".into()), ("tags".into(), "x, y".into())]));
        let many: String = (0..51).map(|i| format!("f{i}=v&")).collect();
        assert!(fields(many.as_bytes()).is_none());
        assert_eq!((page_of("contact/index.html"), page_of("about.html"), page_of("index.html")), ("/contact/".into(), "/about.html".into(), "/".into()));
    }
}
