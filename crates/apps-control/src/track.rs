//! Custom events (Vercel Web Analytics' `track()`, Plausible's goals; docs/apps/README.md,
//! "Visitors"): a page sends `POST /__roll/event {name, props?, path?}` to its own host, or loads
//! `/__roll/track.js` and calls `roll.track(name, props)`. Each is a line of the environment's
//! `events:` log (its name, its bounded properties, its route, the edge's country), kept with the
//! log's retention; nothing about the visitor is. Read back on the analytics answer as counts by
//! name, with each property's most common values.

use crate::{Apps, Result};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub const SCRIPT_PATH: &str = "/__roll/track.js";
pub const BEACON: &str = "/__roll/event";
const SCRIPT: &str = include_str!("../assets/track.js");
/// An event's most bytes.
pub const MAX_BODY: usize = 4096;
/// Events an environment keeps a minute on its owner (the rest are dropped, 429).
const PER_MINUTE: u32 = 1_200;
/// Lines one read looks at (newest first).
const MAX_LINES: usize = 200_000;
/// Names an answer lists, and values each property lists.
const TOP: usize = 20;
const TOP_VALUES: usize = 5;
/// Names counted apart before the rest are left out: a page that names events by an id can't
/// make the map grow with every one.
const MAX_NAMES: usize = 500;
/// Properties counted apart per name.
const MAX_PROPS: usize = 20;

pub(crate) fn key(service: &str, env: &str) -> String {
    format!("events:{service}/{env}")
}

pub fn script() -> Response {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=3600")], SCRIPT).into_response()
}

/// One name's events: how many, and how often each property took each value.
type Tally = (u64, BTreeMap<String, HashMap<String, u64>>);

/// One event as kept.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Event {
    name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    props: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    country: Option<String>,
}

/// What a beacon said, checked: a name (1–100 printable characters), at most ten properties
/// (names 1–40 characters of a-z, 0-9, `_` and `-`; strings, numbers and booleans, as text of at
/// most 200), and the page's path (as its route). Nothing else is read.
fn parse(body: &[u8]) -> Option<Event> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let name = v["name"].as_str().map(str::trim).filter(|n| !n.is_empty() && n.chars().count() <= 100 && !n.chars().any(char::is_control))?.to_string();
    let mut props = BTreeMap::new();
    match &v["props"] {
        Value::Null => {}
        Value::Object(m) if m.len() <= 10 => {
            for (k, x) in m {
                if k.is_empty() || k.len() > 40 || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                    return None;
                }
                let text = match x {
                    Value::String(s) if s.chars().count() <= 200 && !s.chars().any(char::is_control) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => return None,
                };
                props.insert(k.to_ascii_lowercase(), text);
            }
        }
        _ => return None,
    }
    let route = v["path"].as_str().filter(|p| p.starts_with('/') && p.len() <= 2048).map(crate::traffic::route_of);
    Some(Event { name, props, route, country: None })
}

impl Apps {
    /// A page's event (on the environment's owner): kept, or why not.
    pub(crate) fn event_beacon(&self, service: &str, env: &str, body: &[u8], country: Option<String>) -> StatusCode {
        let Some(mut e) = parse(body) else { return StatusCode::BAD_REQUEST };
        let minute = code_sql::now_ms() / 60_000;
        {
            let mut seen = self.events_seen.lock();
            if seen.len() > 10_000 {
                seen.clear();
            }
            let n = seen.entry(crate::env_key(service, env)).or_insert((minute, 0));
            if n.0 != minute {
                *n = (minute, 0);
            }
            if n.1 >= PER_MINUTE {
                return StatusCode::TOO_MANY_REQUESTS;
            }
            n.1 += 1;
        }
        e.country = country;
        self.logs.sink(key(service, env), crate::logs::Stream::System).line(serde_json::to_string(&e).expect("an event serializes"));
        StatusCode::NO_CONTENT
    }

    /// The events since `since` (ms): by name, the most first, each with its properties' most
    /// common values; and whether the read stopped short.
    pub(crate) async fn tracked(self: &Arc<Self>, service: &str, env: &str, since: i64) -> Result<(Value, bool)> {
        let (lines, cut) = self.logs.read(&key(service, env), |_| true, Some(since), None, MAX_LINES).await;
        let cut = cut || lines.len() >= MAX_LINES;
        let mut by: HashMap<String, Tally> = HashMap::new();
        for l in &lines {
            let Ok(e) = serde_json::from_str::<Event>(&l.text) else { continue };
            if by.len() >= MAX_NAMES && !by.contains_key(&e.name) {
                continue;
            }
            let entry = by.entry(e.name).or_default();
            entry.0 += 1;
            for (k, v) in e.props {
                // A name's properties are counted apart up to MAX_PROPS: beacons that make up a
                // new key each time can't make one read hold millions of them.
                if entry.1.len() >= MAX_PROPS && !entry.1.contains_key(&k) {
                    continue;
                }
                let values = entry.1.entry(k).or_default();
                // As names are: a property that takes a new value every time stays bounded.
                if values.len() < MAX_NAMES || values.contains_key(&v) {
                    *values.entry(v).or_default() += 1;
                }
            }
        }
        let mut list: Vec<(String, Tally)> = by.into_iter().collect();
        list.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
        let events: Vec<Value> = list
            .into_iter()
            .take(TOP)
            .map(|(name, (count, props))| {
                let props: serde_json::Map<String, Value> = props
                    .into_iter()
                    .map(|(k, values)| {
                        let mut v: Vec<(String, u64)> = values.into_iter().collect();
                        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                        (k, Value::Array(v.into_iter().take(TOP_VALUES).map(|(value, count)| json!({ "value": value, "count": count })).collect()))
                    })
                    .collect();
                json!({ "name": name, "count": count, "props": props })
            })
            .collect();
        Ok((Value::Array(events), cut))
    }
}

/// A user agent as a browser, an operating system and a device class (`mobile`, `tablet`,
/// `desktop` or `bot`), from the tokens browsers send (no list of versions kept).
pub fn device_of(ua: &str) -> (&'static str, &'static str, &'static str) {
    let u = ua.to_ascii_lowercase();
    let has = |s: &str| u.contains(s);
    if u.is_empty() || ["bot", "crawl", "spider", "slurp", "curl/", "wget/", "python-requests", "headless", "lighthouse", "preview"].iter().any(|b| has(b)) {
        return ("Other", "Other", "bot");
    }
    let browser = if has("edg/") || has("edga/") || has("edgios/") {
        "Edge"
    } else if has("opr/") || has("opera") {
        "Opera"
    } else if has("samsungbrowser/") {
        "Samsung Internet"
    } else if has("firefox/") || has("fxios/") {
        "Firefox"
    } else if has("chrome/") || has("crios/") || has("chromium/") {
        "Chrome"
    } else if has("safari/") {
        "Safari"
    } else {
        "Other"
    };
    let os = if has("windows") {
        "Windows"
    } else if has("iphone") || has("ipad") || has("ipod") {
        "iOS"
    } else if has("android") {
        "Android"
    } else if has("cros") {
        "ChromeOS"
    } else if has("mac os x") || has("macintosh") {
        "macOS"
    } else if has("linux") {
        "Linux"
    } else {
        "Other"
    };
    let class = if has("ipad") || has("tablet") || (has("android") && !has("mobile")) {
        "tablet"
    } else if has("mobi") || has("iphone") || has("ipod") {
        "mobile"
    } else {
        "desktop"
    };
    (browser, os, class)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_keeps_only_its_bounded_name_and_properties() {
        let e = parse(br#"{"name":"Signup","props":{"Plan":"pro","seats":3,"trial":true},"path":"/users/42","evil":"x"}"#).unwrap();
        assert_eq!(e.name, "Signup");
        assert_eq!(e.props.into_iter().collect::<Vec<_>>(), [("plan".into(), "pro".into()), ("seats".into(), "3".into()), ("trial".into(), "true".into())]);
        assert_eq!(e.route.as_deref(), Some("/users/:id"));
        for bad in [&br#"{"name":""}"#[..], br#"{"name":"x","props":{"a":{"b":1}}}"#, br#"{"name":"x","props":{"no spaces":1}}"#, br#"{"props":{}}"#] {
            assert!(parse(bad).is_none(), "{}", String::from_utf8_lossy(bad));
        }
        assert!(parse(format!(r#"{{"name":"{}"}}"#, "x".repeat(101)).as_bytes()).is_none());
    }

    #[test]
    fn user_agents_read_as_browser_os_and_device() {
        let chrome_mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
        let iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1";
        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0";
        let tablet = "Mozilla/5.0 (Linux; Android 14; SM-X710) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
        assert_eq!(device_of(chrome_mac), ("Chrome", "macOS", "desktop"));
        assert_eq!(device_of(iphone), ("Safari", "iOS", "mobile"));
        assert_eq!(device_of(edge), ("Edge", "Windows", "desktop"));
        assert_eq!(device_of(tablet), ("Chrome", "Android", "tablet"));
        assert_eq!(device_of("Googlebot/2.1 (+http://www.google.com/bot.html)").2, "bot");
        assert_eq!(device_of("curl/8.5.0").2, "bot");
    }
}
