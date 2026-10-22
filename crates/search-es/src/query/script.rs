//! Scripts inside queries (`Script.parse` / `Script.toXContent`): an inline source with its
//! language, options and parameters, or the id of a stored script.
//!
//! A string is an inline script in the default language. An object goes through Elasticsearch's
//! `script` object parser; an object `source` (search templates) is kept as its JSON text with the
//! `content_type` option, and printed back as an object.

use super::xparser::{Field, Tokens};
use super::{Token, XParser};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum ScriptType {
    Inline,
    Stored,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Script {
    pub ty: ScriptType,
    /// The language: always set for inline scripts, never for stored ones.
    pub lang: Option<String>,
    /// The source of an inline script or the id of a stored one.
    pub id_or_code: String,
    /// Compiler options; `content_type` marks an object source.
    pub options: Map,
    pub params: Map,
}

pub const DEFAULT_LANG: &str = "painless";
const CONTENT_TYPE: &str = "content_type";

impl Script {
    /// An inline painless script.
    pub fn inline(source: &str) -> Script {
        Script { ty: ScriptType::Inline, lang: Some(DEFAULT_LANG.into()), id_or_code: source.into(), options: Map::new(), params: Map::new() }
    }

    /// `Script.parse(parser)` with the default language `painless`.
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Script> {
        Script::parse_with_lang(p, DEFAULT_LANG)
    }

    /// `Script.parse(parser, defaultLang)`.
    pub fn parse_with_lang(p: &mut XParser<'_>, default_lang: &str) -> EsResult<Script> {
        if p.current_token().is_none() {
            p.next_token();
        }
        if p.current_token() == Some(Token::String) {
            return Ok(Script {
                ty: ScriptType::Inline,
                lang: Some(default_lang.into()),
                id_or_code: p.text()?,
                options: Map::new(),
                params: Map::new(),
            });
        }
        const FIELDS: &[Field] = &[
            Field::with_deprecated("source", &["inline"], Tokens::OBJECT_OR_STRING),
            Field::with_deprecated("id", &["stored"], Tokens::STRING),
            Field::new("lang", Tokens::STRING),
            Field::new("options", Tokens::OBJECT),
            Field::new("params", Tokens::OBJECT),
        ];
        let mut ty: Option<ScriptType> = None;
        let mut code: Option<String> = None;
        let mut lang: Option<String> = None;
        let mut options = Map::new();
        let mut params = Map::new();
        let only_one = || EsError::illegal_argument("must only use one of [source, id] when specifying a script");
        p.object("script", FIELDS, |p, i| {
            match i {
                0 => {
                    if ty.is_some() {
                        return Err(only_one());
                    }
                    ty = Some(ScriptType::Inline);
                    if p.token() == Token::StartObject {
                        let v = p.take_value();
                        code = Some(v.to_json());
                        options.insert(CONTENT_TYPE.into(), Value::Str("application/json".into()));
                    } else {
                        code = Some(p.text()?);
                    }
                }
                1 => {
                    if ty.is_some() {
                        return Err(only_one());
                    }
                    ty = Some(ScriptType::Stored);
                    code = Some(p.text()?);
                }
                2 => lang = Some(p.text()?),
                3 => {
                    // `mapStrings`: every value as its text.
                    for (k, v) in p.map()? {
                        let text = match v {
                            Value::Str(s) => s,
                            Value::Null => {
                                return Err(EsError::illegal_argument(format!("Expected text but found VALUE_NULL for [{k}]")));
                            }
                            other => other.to_es_string(),
                        };
                        options.insert(k, Value::Str(text));
                    }
                }
                _ => params = p.map()?,
            }
            Ok(())
        })?;
        let Some(ty) = ty else {
            return Err(EsError::illegal_argument("must specify either [source] for an inline script or [id] for a stored script"));
        };
        match ty {
            ScriptType::Inline => {
                let lang = lang.unwrap_or_else(|| default_lang.to_string());
                let Some(code) = code else { return Err(EsError::illegal_argument("must specify <id> for an inline script")) };
                if options.len() > 1 || (options.len() == 1 && !options.contains_key(CONTENT_TYPE)) {
                    options.shift_remove(CONTENT_TYPE);
                    let shown: Vec<String> = options.iter().map(|(k, v)| format!("{k}={}", v.to_es_string())).collect();
                    return Err(EsError::illegal_argument(format!("illegal compiler options [{{{}}}] specified", shown.join(", "))));
                }
                Ok(Script { ty, lang: Some(lang), id_or_code: code, options, params })
            }
            ScriptType::Stored => {
                if lang.is_some() {
                    return Err(EsError::illegal_argument("illegally specified <lang> for a stored script"));
                }
                let Some(code) = code else { return Err(EsError::illegal_argument("must specify <code> for a stored script")) };
                if !options.is_empty() {
                    return Err(EsError::illegal_argument("field [options] cannot be specified using a stored script"));
                }
                Ok(Script { ty, lang: None, id_or_code: code, options, params })
            }
        }
    }

    /// `Script.toXContent`.
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        match self.ty {
            ScriptType::Inline => {
                let json_source = self.options.get(CONTENT_TYPE).and_then(Value::as_str) == Some("application/json");
                let source = if json_source {
                    crate::json::parse(self.id_or_code.as_bytes()).ok().flatten().unwrap_or_else(|| Value::Str(self.id_or_code.clone()))
                } else {
                    Value::Str(self.id_or_code.clone())
                };
                m.insert("source".into(), source);
            }
            ScriptType::Stored => {
                m.insert("id".into(), Value::Str(self.id_or_code.clone()));
            }
        }
        if let Some(l) = &self.lang {
            m.insert("lang".into(), Value::Str(l.clone()));
        }
        if !self.options.is_empty() {
            m.insert("options".into(), Value::Obj(self.options.clone()));
        }
        if !self.params.is_empty() {
            m.insert("params".into(), Value::Obj(self.params.clone()));
        }
        Value::Obj(m)
    }
}
