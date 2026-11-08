//! Pi's session tree, as `get_entries` lists it (T3: `piSessionListing`, `walkPiBranch` and
//! `piForkBoundary` in `packages/provider-pi/src/server/adapter.ts`).
//!
//! A Pi session file is a tree of entries: messages, compactions, and config entries (model and
//! thinking-level changes, the session's name). The conversation is the branch from the root to
//! the leaf; `get_entries` also lists abandoned branches, so everything here follows `parentId`
//! from the leaf. A turn's ref is the leaf it left ([`ProviderEvent::TurnRef`]); going back to it
//! means forking the session at the first user message after it (`fork` keeps everything before
//! that entry), which only works when nothing but config entries sits between the two.
//!
//! [`ProviderEvent::TurnRef`]: crate::provider::ProviderEvent::TurnRef

use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A user message: where a turn (or a steer) starts, and where `fork` can cut.
    User,
    /// Something the model sees: an assistant or tool message, a compaction, a summary.
    Content,
    /// Config entries and the system prompt's snapshot.
    Other,
}

#[derive(Debug)]
struct Entry {
    parent: Option<String>,
    kind: Kind,
}

#[derive(Debug)]
pub struct Listing {
    entries: HashMap<String, Entry>,
    pub leaf: Option<String>,
}

/// Parse `get_entries`'s answer; `None` when it isn't one (T3 then treats the cursor as stale).
pub fn parse(data: &Value) -> Option<Listing> {
    let leaf = match data.get("leafId") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) => None,
        _ => return None,
    };
    let list = data.get("entries")?.as_array()?;
    let mut entries = HashMap::with_capacity(list.len());
    for e in list {
        let id = e.get("id")?.as_str()?.to_string();
        let parent = match e.get("parentId") {
            Some(Value::String(p)) => Some(p.clone()),
            Some(Value::Null) | None => None,
            _ => return None,
        };
        let kind = match e.get("type").and_then(Value::as_str) {
            Some("message") => match e.pointer("/message/role").and_then(Value::as_str) {
                Some("user") => Kind::User,
                Some("system") => Kind::Other,
                _ => Kind::Content,
            },
            Some("compaction" | "branch_summary" | "custom_message") => Kind::Content,
            _ => Kind::Other,
        };
        if entries.insert(id, Entry { parent, kind }).is_some() {
            return None;
        }
    }
    Some(Listing { entries, leaf })
}

impl Listing {
    /// The conversation's branch, root first: the leaf's ancestors as far as this listing goes
    /// (a `since` listing stops at the entry it started after). `None` on a cycle.
    fn branch(&self) -> Option<Vec<&str>> {
        let mut seen = HashSet::new();
        let mut path = Vec::new();
        let mut at = self.leaf.as_deref();
        while let Some(id) = at {
            let Some(entry) = self.entries.get(id) else { break };
            if !seen.insert(id) {
                return None;
            }
            path.push(id);
            at = entry.parent.as_deref();
        }
        path.reverse();
        Some(path)
    }

    /// Where to fork so that the conversation ends at `at` (a turn's ref): the user message to
    /// cut at, or `None` when nothing the model sees comes after `at`.
    pub fn cut_after(&self, at: &str) -> Result<Option<String>, String> {
        let path = self.branch().ok_or("pi's session tree has a cycle")?;
        let Some(i) = path.iter().position(|id| *id == at) else {
            return Err(format!("turn {at} isn't on the conversation's branch"));
        };
        for id in &path[i + 1..] {
            match self.entries[*id].kind {
                Kind::User => return Ok(Some(id.to_string())),
                Kind::Content => return Err(format!("the conversation goes on after {at} without a user message to cut at")),
                Kind::Other => {}
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str, parent: Option<&str>, kind: &str, role: Option<&str>) -> Value {
        let mut e = json!({ "id": id, "parentId": parent, "type": kind });
        if let Some(role) = role {
            e["message"] = json!({ "role": role });
        }
        e
    }

    fn listing(leaf: &str) -> Listing {
        parse(&json!({
            "leafId": leaf,
            "entries": [
                entry("m0", None, "model_change", None),
                entry("s0", Some("m0"), "message", Some("system")),
                entry("u1", Some("s0"), "message", Some("user")),
                entry("a1", Some("u1"), "message", Some("assistant")),
                entry("m2", Some("a1"), "model_change", None),
                entry("u2", Some("m2"), "message", Some("user")),
                entry("a2", Some("u2"), "message", Some("assistant")),
                entry("c3", Some("a2"), "compaction", None),
                entry("u4", Some("c3"), "message", Some("user")),
                // An abandoned branch.
                entry("x", Some("a1"), "message", Some("user")),
            ],
        }))
        .unwrap()
    }

    #[test]
    fn cuts_at_the_next_user_message_on_the_branch() {
        let l = listing("u4");
        assert_eq!(l.cut_after("a1"), Ok(Some("u2".into())));
        assert_eq!(l.cut_after("u4"), Ok(None));
        assert!(l.cut_after("a2").unwrap_err().contains("without a user message"));
        assert!(l.cut_after("x").unwrap_err().contains("isn't on the conversation's branch"));
        // Only config entries after the ref: nothing to drop.
        assert_eq!(listing("m2").cut_after("a1"), Ok(None));
    }

    #[test]
    fn malformed_listings_are_refused() {
        assert!(parse(&json!({ "entries": [] })).is_none());
        assert!(parse(&json!({ "leafId": null, "entries": [] })).unwrap().leaf.is_none());
        let dup = json!({ "leafId": "a", "entries": [entry("a", None, "message", Some("user")), entry("a", None, "message", Some("user"))] });
        assert!(parse(&dup).is_none());
        let cycle = parse(&json!({ "leafId": "a", "entries": [entry("a", Some("b"), "message", Some("user")), entry("b", Some("a"), "message", Some("user"))] })).unwrap();
        assert!(cycle.cut_after("a").is_err());
    }
}
