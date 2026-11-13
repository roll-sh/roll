//! Feedback left on a preview's pages (docs/previews.md, "Feedback on the preview page"), read
//! back from the change's reviews: the dashboard posts each note as a `comment` review whose body
//! starts "**Feedback on the preview** · `/path` · `selector`", then the note. This reads that
//! shape and nothing else, so ordinary reviews never show on the page.

use serde::Serialize;

/// A note on a page, as the page's feedback button lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Note {
    pub path: String,
    pub selector: String,
    pub note: String,
    /// Who left it (their handle).
    pub by: String,
    pub at: i64,
}

const HEAD: &str = "**Feedback on the preview** · ";

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

#[cfg(test)]
mod tests {
    use super::parse;

    /// What the dashboard posts (`preview-feedback.tsx`'s `feedbackBody`) reads back; anything
    /// else is no note.
    #[test]
    fn feedback_reviews_read_back_and_others_dont() {
        let body = "**Feedback on the preview** · [`/checkout?step=2`](https://web-pr7.preview.roll.site/checkout?step=2) · `button.pay`\n\nToo small\n\n<sub>1280x800 viewport · roll preview pv_1</sub>";
        assert_eq!(parse(body), Some(("/checkout?step=2".into(), "button.pay".into(), "Too small".into())));
        let plain = "**Feedback on the preview** · `/` \n\nx";
        assert_eq!(parse(plain), None, "a path in backticks only, then the separator");
        let bare = "**Feedback on the preview** · `/about`\n\nLooks off\n\n<sub>roll preview pv_1</sub>";
        assert_eq!(parse(bare), Some(("/about".into(), String::new(), "Looks off".into())));
        assert_eq!(parse("LGTM"), None);
        assert_eq!(parse("**Feedback on the preview** · [`/x`](u)\n\n"), None, "no note");
    }
}
