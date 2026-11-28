//! Feedback left on a preview's pages (docs/previews.md, "Feedback on the preview page"), read
//! back from the change's reviews: the dashboard posts each note as a `comment` review whose body
//! starts "**Feedback on the preview** · `/path` · `selector`", then the note; a reply to one is a
//! `comment` review starting "**Reply on the preview** · `<the note's id>`", then the reply (Vercel
//! Comments' threads). This reads those shapes and nothing else, so ordinary reviews never show on
//! the page.

use serde::Serialize;

/// A note on a page, as the page's feedback button lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Note {
    /// The review it was posted as: what resolving it dismisses.
    pub id: String,
    pub path: String,
    pub selector: String,
    pub note: String,
    /// Who left it (their handle).
    pub by: String,
    pub at: i64,
    /// The replies to it, oldest first (the latest [`MAX_REPLIES`]).
    pub replies: Vec<Reply>,
    /// Whether a picture of its element is kept (`GET …/notes/{note}/crop`): a screenshot run
    /// cropped it from the page by its selector.
    pub crop: bool,
    /// How that picture was taken, when as the reviewer saw it ("390x844, after 2 steps").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crop_as: Option<String>,
    /// The picture is the reviewer's own, of their tab, taken in their browser (Jam's, Marker.io's)
    /// and sent with the note, rather than roll's crop.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub crop_own: bool,
    /// How the reviewer saw the page, from the note itself: what roll's browser takes it as.
    #[serde(skip)]
    pub seen: Seen,
    /// Written by someone outside the team (a reader who can't push, a share link's guest): MCP's
    /// lockdown withholds its words, and roll's browser never takes its steps.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub outside: bool,
    /// The work item it was made on its change's repo (`POST …/notes/{note}/work`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work: Option<NoteWork>,
}

/// A note's work item: its number on the repo, and where the repo's work is shown.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NoteWork {
    pub number: u64,
    pub url: String,
}

/// How a note's reviewer saw its page (the note's own words, from the page's button): their
/// viewport, and the steps on that page roll's browser can take again: clicks (checking a box
/// is one), typing (as many `x`s as they typed: the button keeps how many characters, never
/// what), their choices in a list (by the option's place, never its words) and Enter or Escape,
/// after their last page change, the last 10.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seen {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub steps: Vec<crate::settings::Step>,
    /// Their browser's engine when it isn't Chromium's ([`crate::settings::BROWSERS`]): every
    /// browser on iOS is WebKit at a phone's size, Safari elsewhere WebKit, Firefox Firefox.
    pub browser: Option<String>,
}

impl Seen {
    /// Read from a note's body: its `<sub>` line's `390x844 viewport`, and "What they did".
    pub fn of(body: &str) -> Seen {
        let mut seen = Seen::default();
        if let Some(sub) = body.split("<sub>").nth(1).and_then(|s| s.split("</sub>").next()) {
            for part in sub.split(" · ") {
                // `Safari 18 on iOS`, `Firefox 133 on Windows` (the button's `browserOf`).
                if let Some((name, os)) = part.split_once(" on ")
                    && name.split_once(' ').is_some_and(|(_, v)| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                {
                    seen.browser = if os == "iOS" {
                        Some("webkit-phone".into())
                    } else if name.starts_with("Safari ") {
                        Some("webkit".into())
                    } else if name.starts_with("Firefox ") {
                        Some("firefox".into())
                    } else {
                        None
                    };
                }
                if let Some((w, h)) = part.strip_suffix(" viewport").and_then(|v| v.split_once('x'))
                    && let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>())
                    && (320..=2560).contains(&w)
                    && (320..=2000).contains(&h)
                {
                    (seen.width, seen.height) = (Some(w), Some(h));
                }
            }
        }
        // Roll's own part of the body (after the `<sub>` line): never the note's text.
        let Some(did) = body.split_once("</sub>").and_then(|(_, after)| after.split("What they did:").nth(1)) else { return seen };
        for line in did.lines().map(str::trim).take_while(|l| !l.starts_with("</details>")) {
            // `1. \`12s before · click button.pay\``
            let Some(what) = line.split_once(". `").and_then(|(_, rest)| rest.strip_suffix('`')).and_then(|s| s.split_once(" · ")).map(|(_, w)| w) else { continue };
            // Another page: what came before was done there.
            if what.starts_with("go to ") {
                seen.steps.clear();
                continue;
            }
            let step = if let Some(sel) = what.strip_prefix("click ").or_else(|| what.strip_prefix("check ")).or_else(|| what.strip_prefix("uncheck ")) {
                crate::settings::selector_ok(sel).then(|| crate::settings::Step::Click(sel.to_string()))
            } else if let Some((n, sel)) = what.strip_prefix("type ").and_then(|r| r.split_once(" characters in ")) {
                // Their length, never their words: the field sees as much typing as it did.
                n.parse::<usize>().ok().filter(|n| (1..=200).contains(n)).filter(|_| crate::settings::selector_ok(sel)).map(|n| crate::settings::Step::Type(sel.to_string(), "x".repeat(n)))
            } else if let Some((n, sel)) = what.strip_prefix("choose option ").and_then(|r| r.split_once(" in ")) {
                n.parse::<u32>().ok().filter(|n| (1..=crate::settings::MAX_OPTION).contains(n)).filter(|_| crate::settings::selector_ok(sel)).map(|n| crate::settings::Step::Choose(sel.to_string(), n))
            } else if let Some(rest) = what.strip_prefix("press ") {
                rest.split_once(" in ").map(|(k, _)| k).filter(|k| matches!(*k, "Enter" | "Escape")).map(|k| crate::settings::Step::Press(k.to_string()))
            } else {
                None
            };
            seen.steps.extend(step);
        }
        let over = seen.steps.len().saturating_sub(10);
        seen.steps.drain(..over);
        seen
    }

    /// What a crop taken this way says it is: the engine when not Chromium's, the viewport, and
    /// the steps taken first.
    pub fn label(&self) -> Option<String> {
        let (w, h) = (self.width?, self.height?);
        let engine = match self.browser.as_deref() {
            Some("webkit" | "webkit-phone") => "WebKit, ",
            Some("firefox") => "Firefox, ",
            _ => "",
        };
        Some(match self.steps.len() {
            0 => format!("{engine}{w}x{h}"),
            1 => format!("{engine}{w}x{h}, after 1 step"),
            n => format!("{engine}{w}x{h}, after {n} steps"),
        })
    }
}

/// A reply to a note: its own review.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Reply {
    pub id: String,
    pub note: String,
    pub by: String,
    pub at: i64,
    /// Written by someone outside the team.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub outside: bool,
}

pub const MAX_REPLIES: usize = 20;

/// The reply a resolve leaves on the note (roll's Resolve, [`crate::Engine::resolve_note`]).
pub const RESOLVED: &str = "Resolved.";

/// Who resolves a note whose work item closed ([`crate::Engine::tend_fixed_notes`]): roll, no
/// person.
pub const ROLL: &str = "roll";

/// Notes an hour one share link's guests may leave.
pub const GUEST_PER_HOUR: usize = 10;

/// A note a guest sent from the page's button (`POST /__roll/notes`), as the page said it.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestNote {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub selector: String,
    pub note: String,
    #[serde(default)]
    pub viewport: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub requests: Vec<String>,
    /// The picture of their tab they took (`POST /__roll/screen`'s token), kept as the note's.
    #[serde(default)]
    pub screen: String,
}

/// The largest picture a reviewer may send of their own tab.
pub const SCREEN_MAX: usize = 1 << 20;
/// Its widest and tallest side (the page downscales to it).
pub const SCREEN_SIDE: u32 = 1600;

/// A reviewer's picture of their own tab, checked: at most [`SCREEN_MAX`] bytes, a PNG, JPEG or
/// WebP by its own bytes (whatever it was sent as), at most [`SCREEN_SIDE`] a side, and decoded
/// whole. Its type.
pub fn screen_ok(bytes: &[u8]) -> Result<&'static str, String> {
    if bytes.len() > SCREEN_MAX {
        return Err(format!("a picture is {} KiB at most", SCREEN_MAX >> 10));
    }
    let (format, mime) = match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => (image::ImageFormat::Png, "image/png"),
        Ok(image::ImageFormat::Jpeg) => (image::ImageFormat::Jpeg, "image/jpeg"),
        Ok(image::ImageFormat::WebP) => (image::ImageFormat::WebP, "image/webp"),
        _ => return Err("a picture is a PNG, JPEG or WebP".into()),
    };
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(SCREEN_SIDE);
    limits.max_image_height = Some(SCREEN_SIDE);
    limits.max_alloc = Some(64 << 20);
    reader.limits(limits);
    match reader.decode() {
        Ok(img) if img.width() > 0 && img.height() > 0 => Ok(mime),
        Ok(_) => Err("the picture is empty".into()),
        Err(image::ImageError::Limits(_)) => Err(format!("a picture is {SCREEN_SIDE}x{SCREEN_SIDE} at most")),
        Err(_) => Err("the picture doesn't decode".into()),
    }
}

/// A guest's words made inert where a pull request shows them: no mention (`@` breaks), no
/// image, no HTML, no fence breaking out of a code span.
pub fn inert(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect::<String>()
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('@', "@\u{200b}")
        .replace("![", "!\u{200b}[")
        .replace("](", "]\u{200b}(")
}

/// One line for a code span: printable, no backtick, at most `n` characters.
fn span(s: &str, n: usize) -> String {
    s.chars().filter(|c| !c.is_control() && *c != '`').take(n).collect()
}

/// A guest note's author (`guest:<name>`: no handle has a colon) and its review body, in the
/// dashboard's shape (`preview-feedback.tsx`'s `feedbackBody`), so roll reads it back as a
/// note. `url` is the preview's first service's; `label` the share link's.
pub fn guest_body(n: &GuestNote, url: &str, preview: &str, label: &str) -> Result<(String, String), String> {
    let name: String = n.name.chars().filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '.' | '-' | '_' | '\'')).take(40).collect::<String>().trim().to_string();
    if name.is_empty() {
        return Err("a guest's note needs their name".into());
    }
    let note = n.note.trim();
    if note.is_empty() || note.chars().count() > 4000 {
        return Err("a note is 1 to 4,000 characters".into());
    }
    let path: String = format!("/{}", n.path.trim_start_matches('/')).chars().filter(|c| !c.is_whitespace() && !"()<>[]`".contains(*c)).take(300).collect();
    let selector = span(&n.selector, 200);
    let place = if url.is_empty() { format!("`{path}`") } else { format!("[`{path}`]({}{path})", url.trim_end_matches('/')) };
    let viewport = Some(n.viewport.as_str()).filter(|v| v.split_once('x').is_some_and(|(w, h)| !w.is_empty() && !h.is_empty() && w.bytes().chain(h.bytes()).all(|b| b.is_ascii_digit())));
    let via = if label.trim().is_empty() { "a share link".to_string() } else { format!("the share link \"{}\"", span(label, 60)) };
    let meta: Vec<String> = [viewport.map(|v| format!("{v} viewport")), Some(format!("roll preview {preview}")), Some(format!("a guest, through {}", inert(&via)))].into_iter().flatten().collect();
    let mut body = format!("{}{place}{}\n\n{}\n\n<sub>{}</sub>", code_control::review::preview_note::HEAD, if selector.is_empty() { String::new() } else { format!(" · `{selector}`") }, inert(note), meta.join(" · "));
    let did: Vec<String> = n.steps.iter().take(20).enumerate().map(|(i, x)| format!("{}. `{}`", i + 1, span(x, 160))).collect();
    let lines: Vec<String> = n.errors.iter().take(5).map(|e| format!("- error: `{}`", span(e, 200))).chain(n.requests.iter().take(5).map(|r| format!("- `{}`", span(r, 200)))).collect();
    if !did.is_empty() || !lines.is_empty() {
        let summary: Vec<String> = [(did.len(), "step"), (n.errors.len().min(5), "error"), (n.requests.len().min(5), "failed request")]
            .into_iter()
            .filter(|(k, _)| *k > 0)
            .map(|(k, w)| format!("{k} {w}{}", if k == 1 { "" } else { "s" }))
            .collect();
        let parts: Vec<String> = [if did.is_empty() { String::new() } else { format!("What they did:\n\n{}", did.join("\n")) }, lines.join("\n")].into_iter().filter(|p| !p.is_empty()).collect();
        body = format!("{body}\n\n<details><summary>On the page: {}</summary>\n\n{}\n\n</details>", summary.join(", "), parts.join("\n\n"));
    }
    Ok((format!("guest:{name}"), body))
}

/// The bodies' shapes are roll code's (chat ops carries notes to Slack and back too).
pub use code_control::review::preview_note::{parse, parse_reply};

/// A change's comment review, as notes are read from it.
pub struct Comment {
    pub id: String,
    pub body: String,
    pub by: String,
    pub at: i64,
    /// Its author can't push to the repo (the review's own mark), or it's a share link's guest's.
    pub outside: bool,
}

/// The notes in a change's comment reviews, each with its replies (a reply to anything else
/// is dropped), in the reviews' order.
pub fn thread(comments: impl IntoIterator<Item = Comment>) -> Vec<Note> {
    let mut notes: Vec<Note> = Vec::new();
    let mut replies: Vec<(String, Reply)> = Vec::new();
    for c in comments {
        if let Some((path, selector, note)) = parse(&c.body) {
            let outside = c.outside || c.by.starts_with("guest:");
            // Its steps aren't taken from outside the team (a reader's could click anything).
            let seen = if outside { Seen { steps: Vec::new(), ..Seen::of(&c.body) } } else { Seen::of(&c.body) };
            notes.push(Note { id: c.id, path, selector, note, by: c.by, at: c.at, replies: Vec::new(), crop: false, crop_as: None, crop_own: false, seen, outside, work: None });
        } else if let Some((to, note)) = parse_reply(&c.body) {
            let outside = c.outside || c.by.starts_with("guest:");
            replies.push((to, Reply { id: c.id, note, by: c.by, at: c.at, outside }));
        }
    }
    replies.sort_by_key(|(_, r)| r.at);
    for n in &mut notes {
        n.replies = replies.iter().filter(|(to, _)| *to == n.id).map(|(_, r)| r.clone()).collect();
        let over = n.replies.len().saturating_sub(MAX_REPLIES);
        n.replies.drain(..over);
    }
    notes
}
