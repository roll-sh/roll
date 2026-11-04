//! Chat platforms for roll's agents: the Slack and Discord wire. A library with no knowledge of
//! roll's own records and no I/O except the platforms' HTTP APIs:
//!
//! - [`card`]: platform-neutral views of an agent thread (a live status message, a request the
//!   agent is waiting on). Each platform renders them.
//! - [`slack`]: signing-secret verification, OAuth v2 installs, the Web API calls roll makes,
//!   Events API / slash command / interactivity parsers and Block Kit rendering.
//! - [`discord`]: Ed25519 interaction verification, bot installs, the REST calls roll makes,
//!   interaction parsers, responses and component rendering.
//!
//! Secrets (signing secrets, client secrets, bot and access tokens) never appear in an
//! [`Error`], a [`VerifyError`] or a `Debug` print.

pub mod card;
pub mod discord;
pub mod slack;

pub use card::{
    Choice, ChoiceStyle, Link, QuestionView, RequestCard, RequestKind, StatusCard, Tone,
};

use std::time::Duration;

/// How far a request's timestamp may be from our clock, either way, before it is a replay.
pub const MAX_SKEW_S: i64 = 300;

/// The most of a platform's answer we read. More is [`Error::Decode`].
pub const MAX_ANSWER_BYTES: usize = 1 << 20;

/// Why an inbound request was refused. The text says nothing about the secret or key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("request is not signed")]
    Missing,
    #[error("request timestamp is too far from now")]
    Stale,
    #[error("request signature does not match")]
    Mismatch,
}

/// A failed call to a platform's API. Never carries a token or secret.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The platform refused the call: Slack's `error` code, or Discord's `<code>: <message>`
    /// (`http_<status>` when the body had neither).
    #[error("chat api error: {0}")]
    Api(String),
    #[error("chat api rate limited; retry after {retry_after_s}s")]
    RateLimited { retry_after_s: u64 },
    /// The request didn't complete: connect, timeout, or a 5xx.
    #[error("chat api request failed: {0}")]
    Http(String),
    #[error("chat api answer: {0}")]
    Decode(String),
}

impl Error {
    /// Worth retrying later: rate limits, transport failures and the platform's own outages.
    pub fn transient(&self) -> bool {
        match self {
            Error::RateLimited { .. } | Error::Http(_) => true,
            Error::Api(code) => matches!(
                code.as_str(),
                "internal_error"
                    | "fatal_error"
                    | "service_unavailable"
                    | "request_timeout"
                    | "ratelimited"
            ),
            Error::Decode(_) => false,
        }
    }

    /// A transport error, without its URL (paths can name channels; keep logs boring).
    fn transport(e: reqwest::Error) -> Error {
        Error::Http(e.without_url().to_string())
    }
}

/// The HTTP client both platforms use: 30s timeout, no redirects (an API that redirects is
/// misconfigured, and following one would carry the token elsewhere).
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("roll")
        .build()
        .expect("reqwest client builds with static options")
}

/// Read an answer's body, refusing more than [`MAX_ANSWER_BYTES`].
async fn read_bounded(mut res: reqwest::Response) -> Result<Vec<u8>, Error> {
    if res
        .content_length()
        .is_some_and(|n| n > MAX_ANSWER_BYTES as u64)
    {
        return Err(Error::Decode("answer too large".into()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(Error::transport)? {
        if body.len() + chunk.len() > MAX_ANSWER_BYTES {
            return Err(Error::Decode("answer too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// An answer that is JSON but not the expected shape. serde's text can quote the values it
/// rejected (a token, say), so only a missing field's name is kept.
fn shape_error(e: serde_json::Error) -> Error {
    let msg = e.to_string();
    match msg.strip_prefix("missing field ") {
        Some(field) => Error::Decode(format!("missing field {}", cut_bytes(field, 64))),
        None => Error::Decode("unexpected answer shape".into()),
    }
}

/// `s` cut to at most `max` characters, ending in `…` when cut.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `s` cut to at most `max` bytes on a char boundary, with no marker: for opaque ids.
pub(crate) fn cut_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// RFC 3986 percent-encoding of everything but unreserved characters: query values and path
/// segments.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One environment lookup that treats an empty value as unset.
fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// The required variables: all set (`Some`), none set (`None`), or an error naming the missing.
fn all_or_none(
    get: &impl Fn(&str) -> Option<String>,
    names: &[&str],
) -> Result<Option<Vec<String>>, String> {
    let values: Vec<Option<String>> = names.iter().map(|n| get(n)).collect();
    if values.iter().all(Option::is_none) {
        return Ok(None);
    }
    let missing: Vec<&str> = names
        .iter()
        .zip(&values)
        .filter(|(_, v)| v.is_none())
        .map(|(n, _)| *n)
        .collect();
    if !missing.is_empty() {
        return Err(format!("set {} as well", missing.join(", ")));
    }
    Ok(Some(values.into_iter().flatten().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_marks_cuts_and_counts_chars() {
        assert_eq!(truncate("  short ", 10), "short");
        assert_eq!(truncate("ééééé", 3), "éé…");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(cut_bytes("aé", 2), "a");
    }

    #[test]
    fn transient_errors() {
        assert!(Error::RateLimited { retry_after_s: 1 }.transient());
        assert!(Error::Http("timeout".into()).transient());
        assert!(Error::Api("internal_error".into()).transient());
        assert!(!Error::Api("channel_not_found".into()).transient());
        assert!(!Error::Decode("x".into()).transient());
    }

    #[test]
    fn all_or_none_names_the_missing() {
        let names = ["A", "B", "C"];
        assert_eq!(all_or_none(&|_| None, &names), Ok(None));
        let some = |k: &str| (k == "B").then(|| "b".to_string());
        assert_eq!(all_or_none(&some, &names), Err("set A, C as well".into()));
        let all = |k: &str| Some(k.to_lowercase());
        assert_eq!(
            all_or_none(&all, &names),
            Ok(Some(vec!["a".into(), "b".into(), "c".into()]))
        );
    }
}
