//! Notes on a preview's pages as reviews (docs/previews.md, "Feedback on the preview page"). The
//! dashboard posts a note as a `comment` review whose body starts
//! "**Feedback on the preview** · [`/path`](url) · `selector`", then the note; a reply to one is a
//! `comment` review starting "**Reply on the preview** · `<the note's id>`", then the reply (Vercel
//! Comments' threads). roll previews reads them back as the page's notes (`roll_previews::notes`),
//! and chat ops' `slack.note` carries them to Slack and back. These read those shapes and nothing
//! else, so ordinary reviews are never notes.

pub const HEAD: &str = "**Feedback on the preview** · ";
pub const REPLY: &str = "**Reply on the preview** · ";

/// The page, the element and the note of a feedback review's body, if it is one.
pub fn parse(body: &str) -> Option<(String, String, String)> {
    let (first, rest) = body.split_once("\n\n")?;
    let first = first.strip_prefix(HEAD)?;
    // `[`/path`](url)` or `` `/path` ``, then ` · `selector``.
    let mut parts = first.splitn(2, " · ");
    let place = parts.next()?;
    let path = place.strip_prefix("[`").and_then(|p| p.split_once("`]")).map(|(p, _)| p).or_else(|| place.strip_prefix('`').and_then(|p| p.strip_suffix('`')))?;
    let selector = parts.next().and_then(|s| s.strip_prefix('`')?.strip_suffix('`')).unwrap_or("");
    let note = rest.rsplit_once("\n\n<sub>").map_or(rest, |(n, _)| n).trim();
    if !path.starts_with('/') || note.is_empty() {
        return None;
    }
    Some((path.to_string(), selector.to_string(), note.chars().take(2000).collect()))
}

/// The page's address a note links to (`[`/path`](url)`), if it has one and it's http(s).
pub fn link(body: &str) -> Option<String> {
    let first = body.split_once("\n\n")?.0.strip_prefix(HEAD)?;
    let url = first.strip_prefix("[`")?.split_once("`](")?.1.split_once(')')?.0;
    (url.starts_with("https://") || url.starts_with("http://")).then(|| url.to_string())
}

/// The note a reply's body answers, and the reply, if it is one.
pub fn parse_reply(body: &str) -> Option<(String, String)> {
    let (first, rest) = body.split_once("\n\n")?;
    let id = first.strip_prefix(REPLY)?.strip_prefix('`')?.strip_suffix('`')?;
    let reply = rest.trim();
    let id_ok = (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    (id_ok && !reply.is_empty()).then(|| (id.to_string(), reply.chars().take(2000).collect()))
}

/// A reply's body, as the dashboard posts one.
pub fn reply_body(note: &str, reply: &str) -> String {
    format!("{REPLY}`{note}`\n\n{}", reply.trim())
}
