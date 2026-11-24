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
}

/// A reply to a note: its own review.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Reply {
    pub id: String,
    pub note: String,
    pub by: String,
    pub at: i64,
}

pub const MAX_REPLIES: usize = 20;

/// The reply a resolve leaves on the note (roll's Resolve, [`crate::Engine::resolve_note`]).
pub const RESOLVED: &str = "Resolved.";

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
}

/// The notes in a change's comment reviews, each with its replies (a reply to anything else
/// is dropped), in the reviews' order.
pub fn thread(comments: impl IntoIterator<Item = Comment>) -> Vec<Note> {
    let mut notes: Vec<Note> = Vec::new();
    let mut replies: Vec<(String, Reply)> = Vec::new();
    for c in comments {
        if let Some((path, selector, note)) = parse(&c.body) {
            notes.push(Note { id: c.id, path, selector, note, by: c.by, at: c.at, replies: Vec::new(), crop: false });
        } else if let Some((to, note)) = parse_reply(&c.body) {
            replies.push((to, Reply { id: c.id, note, by: c.by, at: c.at }));
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


#[cfg(test)]
mod tests {
    use super::parse;

    /// What the dashboard posts (`preview-feedback.tsx`'s `feedbackBody`) reads back; anything
    /// else is no note.
    #[test]
    fn feedback_reviews_read_back_and_others_dont() {
        let body = "**Feedback on the preview** · [`/checkout?step=2`](https://web-pr7.preview.roll.site/checkout?step=2) · `button.pay`\n\nToo small\n\n<sub>1280x800 viewport · roll preview pv_1</sub>";
        assert_eq!(parse(body), Some(("/checkout?step=2".into(), "button.pay".into(), "Too small".into())));
        // What went wrong on the page comes after the `<sub>` line: never the note.
        let with_context = format!("{body}\n\n<details><summary>On the page: 1 error</summary>\n\n- error: `TypeError`\n\n</details>");
        assert_eq!(parse(&with_context), parse(body));
        let plain = "**Feedback on the preview** · `/` \n\nx";
        assert_eq!(parse(plain), None, "a path in backticks only, then the separator");
        let bare = "**Feedback on the preview** · `/about`\n\nLooks off\n\n<sub>roll preview pv_1</sub>";
        assert_eq!(parse(bare), Some(("/about".into(), String::new(), "Looks off".into())));
        assert_eq!(parse("LGTM"), None);
        assert_eq!(parse("**Feedback on the preview** · [`/x`](u)\n\n"), None, "no note");
    }

    /// Replies (`preview-feedback.tsx`'s `replyBody`) join the note they name, oldest first; one
    /// naming anything else, or not shaped as one, joins nothing.
    #[test]
    fn replies_join_their_notes() {
        let c = |id: &str, body: &str, at: i64| super::Comment { id: id.into(), body: body.into(), by: "ada".into(), at };
        let notes = super::thread([
            c("rv_2", "**Reply on the preview** · `rv_1`\n\nFixed in the next push", 30),
            c("rv_1", "**Feedback on the preview** · `/`\n\nThe heading is cut off", 10),
            c("rv_3", "**Reply on the preview** · `rv_1`\n\nWhich width?", 20),
            c("rv_4", "**Reply on the preview** · `rv_9`\n\nTo nothing", 40),
            c("rv_5", "**Reply on the preview** · `rv 1`\n\nNot an id", 50),
            c("rv_6", "LGTM", 60),
        ]);
        assert_eq!(notes.len(), 1);
        let said: Vec<(&str, &str)> = notes[0].replies.iter().map(|r| (r.id.as_str(), r.note.as_str())).collect();
        assert_eq!(said, [("rv_3", "Which width?"), ("rv_2", "Fixed in the next push")]);
    }
}
