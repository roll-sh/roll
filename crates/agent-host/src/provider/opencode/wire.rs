//! The slice of OpenCode 2's HTTP API (`@opencode/protocol`, as T3's v2 adapter and
//! `OpenCode2Client.ts` read it) that roll reads. Unknown fields are skipped.
//!
//! A model entry carries `settings` and `headers` (API keys, base URLs): [`Model`] has no field for
//! either, so serde skips them without keeping a byte, and nothing here is ever logged whole.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A permission rule (`Permission.Rule`): the last matching rule wins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub action: String,
    pub resource: String,
    pub effect: Effect,
}

impl Rule {
    pub fn any(action: &str, effect: Effect) -> Self {
        Rule { action: action.into(), resource: "*".into(), effect }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

/// A model as a session names it (`Model.Ref`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl ModelRef {
    /// `provider/model`, split at the first `/` (T3's `parseOpenCodeModelSlug`); the variant is
    /// the reasoning effort.
    pub fn parse(slug: &str, variant: Option<&str>) -> Option<Self> {
        let (provider, model) = slug.trim().split_once('/')?;
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        Some(ModelRef { provider_id: provider.into(), id: model.into(), variant: variant.filter(|v| !v.is_empty()).map(str::to_string) })
    }

    pub fn slug(&self) -> String {
        format!("{}/{}", self.provider_id, self.id)
    }

    /// T3's `sameModel`: no variant is the `default` one.
    pub fn same(&self, other: Option<&ModelRef>) -> bool {
        other.is_some_and(|o| {
            o.provider_id == self.provider_id
                && o.id == self.id
                && o.variant.as_deref().unwrap_or("default") == self.variant.as_deref().unwrap_or("default")
        })
    }
}

/// `{ "data": … }`, how most answers come.
#[derive(Debug, Deserialize)]
pub struct Data<T> {
    pub data: T,
}

#[derive(Debug, Deserialize)]
pub struct Info {
    #[allow(dead_code)]
    pub version: String,
    #[allow(dead_code)]
    pub pid: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Location {
    pub directory: String,
}

/// A session (`POST /api/session`, `GET /api/session/{id}`, `POST …/fork`).
#[derive(Clone, Debug, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub permissions: Option<Vec<Rule>>,
    #[serde(default)]
    pub location: Option<Location>,
}

/// A model from `GET /api/model`: only what roll shows and what sizes the context window.
#[derive(Debug, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub variants: Vec<Variant>,
    #[serde(default)]
    pub time: Option<ModelTime>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<Limit>,
    #[serde(default)]
    pub capabilities: Option<ModelCapabilities>,
}

impl Model {
    /// T3: `limit.input ?? limit.context`.
    pub fn window(&self) -> Option<u64> {
        self.limit.as_ref().and_then(|l| l.input.or(l.context)).filter(|w| *w > 0)
    }
}

/// A reasoning variant: only its id (its `settings` are the provider's request options).
#[derive(Debug, Deserialize)]
pub struct Variant {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct ModelTime {
    #[serde(default)]
    pub released: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct Limit {
    #[serde(default)]
    pub context: Option<u64>,
    #[serde(default)]
    pub input: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ModelCapabilities {
    #[serde(default)]
    pub input: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Agent {
    pub id: String,
    #[serde(default)]
    pub permissions: Vec<Rule>,
}

#[derive(Debug, Deserialize)]
pub struct Command {
    pub name: String,
}

/// A page of `GET /api/session/{id}/message`.
#[derive(Debug, Deserialize)]
pub struct MessagePage {
    pub data: Vec<Message>,
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize)]
pub struct Cursor {
    #[serde(default)]
    pub next: Option<String>,
}

/// A history entry; only its identity, kind and time.
#[derive(Debug, Deserialize)]
pub struct Message {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub time: Option<MessageTime>,
}

#[derive(Debug, Deserialize)]
pub struct MessageTime {
    #[serde(default)]
    pub created: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct Interrupted {
    #[serde(default)]
    pub interrupted: bool,
}

/// An event from `GET /api/event`: `{id, created, type, location?, data, durable?, metadata?}`.
#[derive(Clone, Debug, Deserialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub data: Value,
}

impl Event {
    /// The session it is about: `data.sessionID`, or a form's own (`form.created`).
    pub fn session(&self) -> Option<&str> {
        self.data.get("sessionID").or_else(|| self.data.get("form").and_then(|f| f.get("sessionID"))).and_then(Value::as_str)
    }
}

/// A step's tokens (`session.step.ended`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
pub struct Tokens {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
    #[serde(default)]
    pub cache: Cache,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
pub struct Cache {
    #[serde(default)]
    pub read: u64,
    #[serde(default)]
    pub write: u64,
}

/// A form field (`form.created`).
#[derive(Clone, Debug, Deserialize)]
pub struct FormField {
    pub key: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub options: Vec<FormOption>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub when: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FormOption {
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Form {
    pub id: String,
    #[serde(default)]
    pub fields: Vec<FormField>,
}

/// `permission.asked`.
#[derive(Clone, Debug, Deserialize)]
pub struct PermissionAsked {
    pub id: String,
    pub action: String,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub save: Vec<String>,
    #[serde(default)]
    pub source: Option<PermissionSource>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PermissionSource {
    #[serde(default)]
    pub id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_split_at_the_first_slash() {
        let m = ModelRef::parse("openrouter/deepseek/deepseek-v4-flash", Some("high")).unwrap();
        assert_eq!((m.provider_id.as_str(), m.id.as_str(), m.variant.as_deref()), ("openrouter", "deepseek/deepseek-v4-flash", Some("high")));
        assert_eq!(m.slug(), "openrouter/deepseek/deepseek-v4-flash");
        assert!(ModelRef::parse("big-pickle", None).is_none());
        assert!(ModelRef::parse("/x", None).is_none());
        let plain = ModelRef::parse("opencode/big-pickle", None).unwrap();
        let default = ModelRef { variant: Some("default".into()), ..plain.clone() };
        assert!(plain.same(Some(&default)));
        assert!(!m.same(Some(&plain)));
    }

    #[test]
    fn model_entries_never_keep_settings() {
        let body = r#"{"data":[{"id":"m","providerID":"p","name":"M","settings":{"apiKey":"sk-secret","baseURL":"https://key.example"},
            "headers":{"Authorization":"Bearer sk-secret"},"variants":[{"id":"high","settings":{"apiKey":"sk-secret"}}],
            "limit":{"context":1000,"input":800},"capabilities":{"input":["text","image"]}}]}"#;
        let models: Data<Vec<Model>> = serde_json::from_str(body).unwrap();
        let debug = format!("{models:?}");
        assert!(!debug.contains("sk-secret") && !debug.contains("key.example"), "{debug}");
        assert_eq!(models.data[0].window(), Some(800));
    }
}
