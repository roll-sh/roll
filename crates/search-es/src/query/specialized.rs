//! Specialized queries: `more_like_this`, `script`, `wrapper`, `distance_feature`,
//! `rank_feature`, `percolate`, `pinned`, `knn`, and the vector/semantic family
//! (`sparse_vector`, `text_expansion`, `weighted_tokens`, `semantic`, `rule`).

use super::script::Script;
use super::xparser::{Field, Tokens, float_value};
use super::{Common, Query, Token, XParser, check_boost, parse_inner};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::obj;

/// Wraps a constructor failure the way `ConstructingObjectParser` does.
fn failed_to_build(parser: &str, e: EsError) -> EsError {
    EsError::x_content_parse_bare(format!("Failed to build [{parser}] after last required field arrived")).caused_by(e)
}

/// A `more_like_this` document: an indexed one (`_index`, `_id`) or an artificial one (`doc`).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct MltItem {
    pub index: Option<String>,
    pub id: Option<String>,
    pub doc: Option<Value>,
    pub fields: Option<Vec<String>>,
    pub per_field_analyzer: Option<Map>,
    pub routing: Option<String>,
    pub version: Option<i64>,
    pub version_type: Option<String>,
}

impl MltItem {
    fn parse(p: &mut XParser<'_>) -> EsResult<MltItem> {
        let mut item = MltItem::default();
        let mut current: Option<String> = None;
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().map(str::to_string),
                _ => {
                    let Some(key) = current.clone() else { continue };
                    match key.as_str() {
                        "_index" => item.index = Some(p.text()?),
                        "_id" => item.id = Some(p.text()?),
                        "doc" => item.doc = Some(p.take_value()),
                        "fields" => {
                            if t != Token::StartArray {
                                return Err(EsError::parse("failed to parse More Like This item. field [fields] must be an array"));
                            }
                            let mut fs = Vec::new();
                            while p.next_token().is_some_and(|t| t != Token::EndArray) {
                                fs.push(p.text()?);
                            }
                            item.fields = Some(fs);
                        }
                        "per_field_analyzer" => item.per_field_analyzer = Some(p.map()?),
                        "routing" => item.routing = Some(p.text()?),
                        "version" => item.version = Some(p.long_value()?),
                        "version_type" => {
                            let v = p.text()?;
                            if !matches!(v.as_str(), "internal" | "external" | "external_gt" | "external_gte") {
                                return Err(EsError::illegal_argument(format!("No version type match [{v}]")));
                            }
                            item.version_type = Some(v);
                        }
                        other => return Err(EsError::parse(format!("failed to parse More Like This item. unknown field [{other}]"))),
                    }
                }
            }
        }
        if item.id.is_some() && item.doc.is_some() {
            return Err(EsError::parse("failed to parse More Like This item. either [id] or [doc] can be specified, but not both!"));
        }
        if item.id.is_none() && item.doc.is_none() {
            return Err(EsError::parse("failed to parse More Like This item. neither [id] nor [doc] is specified!"));
        }
        Ok(item)
    }

    fn to_value(&self) -> Value {
        let mut m = Map::new();
        super::opt_str(&mut m, "_index", &self.index);
        super::opt_str(&mut m, "_id", &self.id);
        if let Some(d) = &self.doc {
            m.insert("doc".into(), d.clone());
        }
        if let Some(f) = &self.fields {
            m.insert("fields".into(), Value::from(f.clone()));
        }
        if let Some(a) = &self.per_field_analyzer {
            m.insert("per_field_analyzer".into(), Value::Obj(a.clone()));
        }
        super::opt_str(&mut m, "routing", &self.routing);
        if let Some(v) = self.version {
            m.insert("version".into(), v.into());
        }
        if let Some(v) = &self.version_type.as_ref().filter(|v| *v != "internal") {
            m.insert("version_type".into(), (*v).clone().into());
        }
        Value::Obj(m)
    }
}

/// `more_like_this`.
#[derive(Clone, Debug, PartialEq)]
pub struct MoreLikeThis {
    pub fields: Option<Vec<String>>,
    pub like_texts: Vec<String>,
    pub like_items: Vec<MltItem>,
    pub unlike_texts: Vec<String>,
    pub unlike_items: Vec<MltItem>,
    pub max_query_terms: i32,
    pub min_term_freq: i32,
    pub min_doc_freq: i32,
    pub max_doc_freq: i32,
    pub min_word_length: i32,
    pub max_word_length: i32,
    pub stop_words: Option<Vec<String>>,
    pub analyzer: Option<String>,
    pub minimum_should_match: String,
    pub boost_terms: f32,
    pub include: bool,
    pub fail_on_unsupported_field: bool,
    pub common: Common,
}

impl MoreLikeThis {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<MoreLikeThis> {
        let mut q = MoreLikeThis {
            fields: None,
            like_texts: Vec::new(),
            like_items: Vec::new(),
            unlike_texts: Vec::new(),
            unlike_items: Vec::new(),
            max_query_terms: 25,
            min_term_freq: 2,
            min_doc_freq: 5,
            max_doc_freq: i32::MAX,
            min_word_length: 0,
            max_word_length: 0,
            stop_words: None,
            analyzer: None,
            minimum_should_match: "30%".into(),
            boost_terms: 0.0,
            include: false,
            fail_on_unsupported_field: true,
            common: Common::default(),
        };
        let (mut boost, mut name) = (1.0f32, None);
        let like_field = |p: &mut XParser<'_>, texts: &mut Vec<String>, items: &mut Vec<MltItem>| -> EsResult<()> {
            match p.token() {
                t if t.is_value() => texts.push(p.text()?),
                Token::StartObject => items.push(MltItem::parse(p)?),
                _ => return Err(EsError::illegal_argument("Content of 'like' parameter should either be a string or an object")),
            }
            Ok(())
        };
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                t if t.is_value() => match current.as_str() {
                    "like" => like_field(p, &mut q.like_texts, &mut q.like_items)?,
                    "unlike" => like_field(p, &mut q.unlike_texts, &mut q.unlike_items)?,
                    "max_query_terms" => q.max_query_terms = p.int_value()?,
                    "min_term_freq" => q.min_term_freq = p.int_value()?,
                    "min_doc_freq" => q.min_doc_freq = p.int_value()?,
                    "max_doc_freq" => q.max_doc_freq = p.int_value()?,
                    "min_word_length" => q.min_word_length = p.int_value()?,
                    "max_word_length" => q.max_word_length = p.int_value()?,
                    "analyzer" => q.analyzer = Some(p.text()?),
                    "minimum_should_match" => q.minimum_should_match = p.text()?,
                    "boost_terms" => q.boost_terms = p.float_value()?,
                    "include" => q.include = p.boolean_value()?,
                    "fail_on_unsupported_field" => q.fail_on_unsupported_field = p.boolean_value()?,
                    "boost" => boost = p.float_value()?,
                    "_name" => name = Some(p.text()?),
                    _ => return Err(p.parsing_error(format!("[mlt] query does not support [{current}]"))),
                },
                Token::StartArray => match current.as_str() {
                    "fields" => {
                        let mut fs = Vec::new();
                        while p.next_token().is_some_and(|t| t != Token::EndArray) {
                            fs.push(p.text()?);
                        }
                        q.fields = Some(fs);
                    }
                    "like" | "unlike" => {
                        while p.next_token().is_some_and(|t| t != Token::EndArray) {
                            if current == "like" {
                                like_field(p, &mut q.like_texts, &mut q.like_items)?;
                            } else {
                                like_field(p, &mut q.unlike_texts, &mut q.unlike_items)?;
                            }
                        }
                    }
                    "stop_words" => {
                        let mut sw = Vec::new();
                        while p.next_token().is_some_and(|t| t != Token::EndArray) {
                            sw.push(p.text()?);
                        }
                        q.stop_words = Some(sw);
                    }
                    _ => return Err(p.parsing_error(format!("[mlt] query does not support [{current}]"))),
                },
                Token::StartObject => match current.as_str() {
                    "like" => like_field(p, &mut q.like_texts, &mut q.like_items)?,
                    "unlike" => like_field(p, &mut q.unlike_texts, &mut q.unlike_items)?,
                    _ => return Err(p.parsing_error(format!("[mlt] query does not support [{current}]"))),
                },
                _ => {}
            }
        }
        if q.like_texts.is_empty() && q.like_items.is_empty() {
            return Err(p.parsing_error("more_like_this requires 'like' to be specified"));
        }
        if q.fields.as_ref().is_some_and(Vec::is_empty) {
            return Err(p.parsing_error("more_like_this requires 'fields' to be non-empty"));
        }
        if q.max_query_terms <= 0 {
            return Err(EsError::illegal_argument("requires 'maxQueryTerms' to be greater than 0"));
        }
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        if let Some(f) = &self.fields {
            m.insert("fields".into(), Value::from(f.clone()));
        }
        for (k, texts, items) in [("like", &self.like_texts, &self.like_items), ("unlike", &self.unlike_texts, &self.unlike_items)] {
            if !texts.is_empty() || !items.is_empty() {
                let mut a: Vec<Value> = texts.iter().map(|t| Value::Str(t.clone())).collect();
                a.extend(items.iter().map(MltItem::to_value));
                m.insert(k.into(), Value::Arr(a));
            }
        }
        m.insert("max_query_terms".into(), self.max_query_terms.into());
        m.insert("min_term_freq".into(), self.min_term_freq.into());
        m.insert("min_doc_freq".into(), self.min_doc_freq.into());
        m.insert("max_doc_freq".into(), self.max_doc_freq.into());
        m.insert("min_word_length".into(), self.min_word_length.into());
        m.insert("max_word_length".into(), self.max_word_length.into());
        if let Some(sw) = &self.stop_words {
            m.insert("stop_words".into(), Value::from(sw.clone()));
        }
        super::opt_str(&mut m, "analyzer", &self.analyzer);
        m.insert("minimum_should_match".into(), self.minimum_should_match.clone().into());
        m.insert("boost_terms".into(), float_value(self.boost_terms));
        m.insert("include".into(), self.include.into());
        m.insert("fail_on_unsupported_field".into(), self.fail_on_unsupported_field.into());
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// `script` (the query).
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptQuery {
    pub script: Script,
    pub common: Common,
}

impl ScriptQuery {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<ScriptQuery> {
        let mut script = None;
        let (mut boost, mut name) = (1.0f32, None);
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => {
                    if current == "script" {
                        script = Some(Script::parse(p)?);
                    } else {
                        return Err(p.parsing_error(format!("[script] query does not support [{current}]")));
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "_name" => name = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    "script" => script = Some(Script::parse(p)?),
                    _ => return Err(p.parsing_error(format!("[script] query does not support [{current}]"))),
                },
                _ => {
                    if current != "script" {
                        return Err(p.parsing_error(format!("[script] query does not support [{current}]")));
                    }
                }
            }
        }
        let Some(script) = script else { return Err(p.parsing_error("script must be provided with a [script] filter")) };
        let mut q = ScriptQuery { script, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("script".into(), self.script.to_value());
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// `wrapper`: a base64-encoded query.
#[derive(Clone, Debug, PartialEq)]
pub struct Wrapper {
    pub source: Vec<u8>,
    pub common: Common,
}

impl Wrapper {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Wrapper> {
        use base64::Engine as _;
        let t = p.next_token();
        if t != Some(Token::FieldName) {
            return Err(p.parsing_error("[wrapper] query malformed"));
        }
        if p.current_name() != Some("query") {
            return Err(p.parsing_error(format!("[wrapper] query malformed, expected `query` but was {}", p.current_name().unwrap_or(""))));
        }
        p.next_token();
        let text = p.text()?;
        let source = base64::engine::general_purpose::STANDARD.decode(text.as_bytes()).map_err(|e| {
            let (l, c) = p.token_location();
            let (detail, at) = match e {
                base64::DecodeError::InvalidByte(i, b) => {
                    (format!("Illegal character '{}' (code 0x{:x}) in base64 content", b as char, b), (l, c + i + 2))
                }
                _ => (
                    "Unexpected end of base64-encoded String: base64 variant 'MIME-NO-LINEFEEDS' expects padding (one or more '=' characters) at the end"
                        .into(),
                    p.current_location(),
                ),
            };
            // Jackson decodes while reading the string, so it fails just past the bad character.
            p.jackson_error_at(at, "json_parse_exception", &format!("Failed to decode VALUE_STRING as base64 (MIME-NO-LINEFEEDS): {detail}"))
        })?;
        p.next_token();
        Ok(Wrapper { source, common: Common::default() })
    }

    /// The wrapped query, parsed (Elasticsearch does this when rewriting).
    pub fn wrapped(&self) -> EsResult<Query> {
        let v = crate::json::parse(&self.source)
            .map_err(|e| EsError::x_content_parse(e.line, e.column, e.full_message()))?
            .unwrap_or(Value::Null);
        let mut p = XParser::new(&v, Some(&self.source), Vec::new());
        parse_inner(&mut p)
    }

    pub fn inner(&self) -> Value {
        use base64::Engine as _;
        obj! { "query" => base64::engine::general_purpose::STANDARD.encode(&self.source) }
    }
}

/// `distance_feature`.
#[derive(Clone, Debug, PartialEq)]
pub struct DistanceFeature {
    pub field: String,
    /// A date (string or number) or a geo point (`{lat, lon}`).
    pub origin: Value,
    pub pivot: String,
    pub common: Common,
}

impl DistanceFeature {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<DistanceFeature> {
        const FIELDS: &[Field] = &[
            Field::new("field", Tokens::STRING),
            Field::new("origin", Tokens::of(&[Token::StartObject, Token::StartArray, Token::String, Token::Number])),
            Field::new("pivot", Tokens::STRING),
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
        ];
        let (mut field, mut origin, mut pivot, mut boost, mut name) = (None, None, None, None, None);
        p.object("distance_feature", FIELDS, |p, i| {
            match i {
                0 => field = Some(p.text()?),
                1 => {
                    origin = Some(match p.token() {
                        Token::StartObject | Token::StartArray => {
                            let pt = super::geo::parse_point(p)?;
                            obj! { "lat" => pt.lat, "lon" => pt.lon }
                        }
                        Token::String => {
                            let s = p.text()?;
                            Value::Str(s)
                        }
                        _ => p.value().clone(),
                    })
                }
                2 => pivot = Some(p.text()?),
                3 => boost = Some(p.float_value()?),
                _ => name = Some(p.text()?),
            }
            Ok(())
        })?;
        let missing: Vec<&str> =
            [("field", field.is_none()), ("origin", origin.is_none()), ("pivot", pivot.is_none())].iter().filter(|x| x.1).map(|x| x.0).collect();
        if !missing.is_empty() {
            return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
        }
        let mut q = DistanceFeature { field: field.unwrap_or_default(), origin: origin.unwrap_or(Value::Null), pivot: pivot.unwrap_or_default(), common: Common::default() };
        if let Some(b) = boost {
            q.common.boost = check_boost(b, || q.clone().into())?;
        }
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("field".into(), self.field.clone().into());
        m.insert("origin".into(), self.origin.clone());
        m.insert("pivot".into(), self.pivot.clone().into());
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// A `rank_feature` scoring function.
#[derive(Clone, Debug, PartialEq)]
pub enum RankFeatureFunction {
    Saturation { pivot: Option<f32> },
    Log { scaling_factor: f32 },
    Sigmoid { pivot: f32, exponent: f32 },
    Linear,
}

/// `rank_feature`.
#[derive(Clone, Debug, PartialEq)]
pub struct RankFeature {
    pub field: String,
    pub function: RankFeatureFunction,
    pub common: Common,
}

impl RankFeature {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<RankFeature> {
        const FIELDS: &[Field] = &[
            Field::new("field", Tokens::STRING),
            Field::new("log", Tokens::OBJECT),
            Field::new("saturation", Tokens::OBJECT),
            Field::new("sigmoid", Tokens::OBJECT),
            Field::new("linear", Tokens::OBJECT),
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
        ];
        let mut field = None;
        let mut functions: Vec<RankFeatureFunction> = Vec::new();
        let (mut boost, mut name) = (None, None);
        p.object("feature", FIELDS, |p, i| {
            match i {
                0 => field = Some(p.text()?),
                1..=4 => {
                    let fname = FIELDS[i].name;
                    let params: &[Field] = match fname {
                        "log" => &[Field::new("scaling_factor", Tokens::FLOAT)],
                        "saturation" => &[Field::new("pivot", Tokens::FLOAT)],
                        "sigmoid" => &[Field::new("pivot", Tokens::FLOAT), Field::new("exponent", Tokens::FLOAT)],
                        _ => &[],
                    };
                    let mut vals = [None, None];
                    p.object(fname, params, |p, j| {
                        vals[j] = Some(p.float_value()?);
                        Ok(())
                    })?;
                    let f = match fname {
                        "log" => RankFeatureFunction::Log {
                            scaling_factor: vals[0].ok_or_else(|| failed_to_build("log", EsError::illegal_argument("Required [scaling_factor]")))?,
                        },
                        "saturation" => RankFeatureFunction::Saturation { pivot: vals[0] },
                        "sigmoid" => {
                            let missing: Vec<&str> = [("pivot", vals[0].is_none()), ("exponent", vals[1].is_none())].iter().filter(|x| x.1).map(|x| x.0).collect();
                            if !missing.is_empty() {
                                return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
                            }
                            RankFeatureFunction::Sigmoid { pivot: vals[0].unwrap_or(0.0), exponent: vals[1].unwrap_or(0.0) }
                        }
                        _ => RankFeatureFunction::Linear,
                    };
                    functions.push(f);
                }
                5 => boost = Some(p.float_value()?),
                _ => name = Some(p.text()?),
            }
            Ok(())
        })?;
        let field = field.ok_or_else(|| EsError::illegal_argument("Required [field]"))?;
        if functions.len() > 1 {
            return Err(failed_to_build("feature", EsError::illegal_argument("Can only specify one of [log], [saturation], [sigmoid] and [linear]")));
        }
        let function = functions.pop().unwrap_or(RankFeatureFunction::Saturation { pivot: None });
        let mut q = RankFeature { field, function, common: Common::default() };
        if let Some(b) = boost {
            q.common.boost = check_boost(b, || q.clone().into())?;
        }
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("field".into(), self.field.clone().into());
        match &self.function {
            RankFeatureFunction::Saturation { pivot: None } => {}
            RankFeatureFunction::Saturation { pivot: Some(p) } => {
                m.insert("saturation".into(), obj! { "pivot" => float_value(*p) });
            }
            RankFeatureFunction::Log { scaling_factor } => {
                m.insert("log".into(), obj! { "scaling_factor" => float_value(*scaling_factor) });
            }
            RankFeatureFunction::Sigmoid { pivot, exponent } => {
                m.insert("sigmoid".into(), obj! { "pivot" => float_value(*pivot), "exponent" => float_value(*exponent) });
            }
            RankFeatureFunction::Linear => {
                m.insert("linear".into(), Value::object());
            }
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// `percolate`.
#[derive(Clone, Debug, PartialEq)]
pub struct Percolate {
    pub field: String,
    pub name: Option<String>,
    pub documents: Vec<Value>,
    pub index: Option<String>,
    pub id: Option<String>,
    pub routing: Option<String>,
    pub preference: Option<String>,
    pub version: Option<i64>,
    pub common: Common,
}

impl Percolate {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Percolate> {
        const FIELDS: &[Field] = &[
            Field::new("field", Tokens::STRING),
            Field::new("name", Tokens::STRING),
            Field::new("document", Tokens::OBJECT),
            Field::new("documents", Tokens::OBJECT_ARRAY),
            Field::new("index", Tokens::STRING),
            Field::new("id", Tokens::STRING),
            Field::new("routing", Tokens::STRING),
            Field::new("preference", Tokens::STRING),
            Field::new("version", Tokens::INT),
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
        ];
        let mut q = Percolate {
            field: String::new(),
            name: None,
            documents: Vec::new(),
            index: None,
            id: None,
            routing: None,
            preference: None,
            version: None,
            common: Common::default(),
        };
        let mut field = None;
        let mut boost = None;
        p.object("percolate", FIELDS, |p, i| {
            match i {
                0 => field = Some(p.text()?),
                1 => q.name = Some(p.text()?),
                2 => q.documents.push(p.take_value()),
                3 => {
                    let docs = p.array_items(|p| Ok(p.take_value()))?;
                    q.documents.extend(docs);
                }
                4 => q.index = Some(p.text()?),
                5 => q.id = Some(p.text()?),
                6 => q.routing = Some(p.text()?),
                7 => q.preference = Some(p.text()?),
                8 => q.version = Some(p.long_value()?),
                9 => boost = Some(p.float_value()?),
                _ => q.common.name = Some(p.text()?),
            }
            Ok(())
        })?;
        // `declareRequiredFieldSet` is checked before the constructor arguments.
        if q.documents.is_empty() && q.id.is_none() {
            return Err(EsError::illegal_argument("Required one of fields [document, documents, id], but none were specified."));
        }
        q.field = field.ok_or_else(|| EsError::illegal_argument("Required [field]"))?;
        if !q.documents.is_empty() && q.id.is_some() {
            return Err(EsError::illegal_argument("[percolate] query requires either [document], [documents] or [index] and [id], but not both"));
        }
        if let Some(b) = boost {
            q.common.boost = check_boost(b, || q.clone().into())?;
        }
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("field".into(), self.field.clone().into());
        super::opt_str(&mut m, "name", &self.name);
        if !self.documents.is_empty() {
            m.insert("documents".into(), Value::Arr(self.documents.clone()));
        }
        super::opt_str(&mut m, "index", &self.index);
        super::opt_str(&mut m, "id", &self.id);
        super::opt_str(&mut m, "routing", &self.routing);
        super::opt_str(&mut m, "preference", &self.preference);
        if let Some(v) = self.version {
            m.insert("version".into(), v.into());
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// A pinned document (`_index` optional).
#[derive(Clone, Debug, PartialEq)]
pub struct PinnedDoc {
    pub index: Option<String>,
    pub id: String,
}

/// `pinned`.
#[derive(Clone, Debug, PartialEq)]
pub struct Pinned {
    pub organic: Query,
    pub ids: Option<Vec<String>>,
    pub docs: Option<Vec<PinnedDoc>>,
    pub common: Common,
}

impl Pinned {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Pinned> {
        const FIELDS: &[Field] = &[
            Field::new("organic", Tokens::OBJECT),
            Field::new("ids", Tokens::STRING_ARRAY),
            Field::new("docs", Tokens::OBJECT_ARRAY),
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
        ];
        let (mut organic, mut ids, mut docs, mut boost, mut name): (Option<Query>, Option<Vec<String>>, Option<Vec<PinnedDoc>>, _, _) =
            (None, None, None, None, None);
        let r = p.object("pinned", FIELDS, |p, i| {
            match i {
                0 => organic = Some(parse_inner(p)?),
                1 => ids = Some(p.array_items(|p| p.text())?),
                2 => {
                    docs = Some(p.array_items(|p| {
                        const DOC: &[Field] = &[Field::new("_index", Tokens::STRING), Field::new("_id", Tokens::STRING)];
                        let (mut index, mut id) = (None, None);
                        p.object("item", DOC, |p, j| {
                            if j == 0 {
                                index = Some(p.text()?);
                            } else {
                                id = Some(p.text()?);
                            }
                            Ok(())
                        })?;
                        let id = id.ok_or_else(|| EsError::illegal_argument("Required [_id]"))?;
                        Ok(PinnedDoc { index, id })
                    })?)
                }
                3 => boost = Some(p.float_value()?),
                _ => name = Some(p.text()?),
            }
            if organic.is_some() && ids.is_some() && docs.is_some() {
                return Err(failed_to_build("pinned", EsError::illegal_argument("[pinned] ids and docs cannot both be used")));
            }
            Ok(())
        });
        let wrap = |p: &XParser<'_>, e: EsError| {
            if super::xparser::is_illegal_argument(&e) { p.parsing_error(e.reason().to_string()).caused_by(e) } else { e }
        };
        if let Err(e) = r {
            return Err(wrap(p, e));
        }
        let Some(organic) = organic else { return Err(wrap(p, EsError::illegal_argument("Required [organic]"))) };
        if ids.is_some() && docs.is_some() {
            return Err(wrap(p, failed_to_build("pinned", EsError::illegal_argument("[pinned] ids and docs cannot both be used"))));
        }
        if ids.is_none() && docs.is_none() {
            return Err(wrap(p, failed_to_build("pinned", EsError::illegal_argument("[pinned] either ids or docs must be provided"))));
        }
        let mut q = Pinned { organic, ids, docs, common: Common::default() };
        if let Some(b) = boost {
            q.common.boost = check_boost(b, || q.clone().into()).map_err(|e| wrap(p, e))?;
        }
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("organic".into(), self.organic.to_value());
        if let Some(ids) = &self.ids {
            m.insert("ids".into(), Value::from(ids.clone()));
        }
        if let Some(docs) = &self.docs {
            m.insert(
                "docs".into(),
                Value::Arr(
                    docs.iter()
                        .map(|d| {
                            let mut dm = Map::new();
                            super::opt_str(&mut dm, "_index", &d.index);
                            dm.insert("_id".into(), d.id.clone().into());
                            Value::Obj(dm)
                        })
                        .collect(),
                ),
            );
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// A `knn` query vector: floats, or bytes given as a hex string.
#[derive(Clone, Debug, PartialEq)]
pub enum VectorData {
    Floats(Vec<f32>),
    Bytes(Vec<i8>),
}

impl VectorData {
    /// `VectorData.parseXContent`.
    pub fn parse(p: &mut XParser<'_>) -> EsResult<VectorData> {
        match p.token() {
            Token::StartArray => {
                let mut out = Vec::new();
                while let Some(t) = p.next_token() {
                    if t == Token::EndArray {
                        break;
                    }
                    out.push(p.float_value()?);
                }
                Ok(VectorData::Floats(out))
            }
            Token::String => {
                let s = p.text()?;
                if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(EsError::parse(format!("Failed to parse object: invalid hex string [{s}]")));
                }
                let bytes = (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0) as i8).collect();
                Ok(VectorData::Bytes(bytes))
            }
            Token::Number => Ok(VectorData::Floats(vec![p.float_value()?])),
            t => Err(EsError::parse(format!("Unknown type [{t}] for parsing vector"))),
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            VectorData::Floats(f) => Value::Arr(f.iter().map(|x| float_value(*x)).collect()),
            VectorData::Bytes(b) => Value::Arr(b.iter().map(|x| Value::Num(Number::Int(*x as i64))).collect()),
        }
    }
}

/// `knn` (the query).
#[derive(Clone, Debug, PartialEq)]
pub struct Knn {
    pub field: String,
    pub query_vector: Option<VectorData>,
    /// `query_vector_builder`: `{"<builder>": {...}}`, kept as given (inference runs elsewhere).
    pub query_vector_builder: Option<Value>,
    pub k: Option<i32>,
    pub num_candidates: Option<i32>,
    pub visit_percentage: Option<f32>,
    pub similarity: Option<f32>,
    pub filter: Vec<Query>,
    pub rescore_oversample: Option<f32>,
    pub common: Common,
}

impl Knn {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Knn> {
        const FIELDS: &[Field] = &[
            Field::new("field", Tokens::STRING),
            Field::new("query_vector", Tokens::OBJECT_ARRAY_STRING_OR_NUMBER),
            Field::new("k", Tokens::INT),
            Field::new("num_candidates", Tokens::INT),
            Field::new("visit_percentage", Tokens::FLOAT),
            Field::new("similarity", Tokens::FLOAT),
            Field::new("query_vector_builder", Tokens::OBJECT),
            Field::new("rescore_vector", Tokens::OBJECT),
            Field::new("filter", Tokens::OBJECT_ARRAY),
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
        ];
        let mut q = Knn {
            field: String::new(),
            query_vector: None,
            query_vector_builder: None,
            k: None,
            num_candidates: None,
            visit_percentage: None,
            similarity: None,
            filter: Vec::new(),
            rescore_oversample: None,
            common: Common::default(),
        };
        let mut field = None;
        let mut boost = None;
        p.object("knn", FIELDS, |p, i| {
            match i {
                0 => field = Some(p.text()?),
                1 => q.query_vector = Some(VectorData::parse(p)?),
                2 => q.k = Some(p.int_value()?),
                3 => q.num_candidates = Some(p.int_value()?),
                4 => q.visit_percentage = Some(p.float_value()?),
                5 => q.similarity = Some(p.float_value()?),
                6 => q.query_vector_builder = Some(p.take_value()),
                7 => {
                    const R: &[Field] = &[Field::new("oversample", Tokens::FLOAT)];
                    let mut over = None;
                    p.object("rescore_vector", R, |p, _| {
                        over = Some(p.float_value()?);
                        Ok(())
                    })?;
                    let o = over.ok_or_else(|| EsError::illegal_argument("Required [oversample]"))?;
                    if !(o == 0.0 || (1.0..=10.0).contains(&o)) {
                        return Err(EsError::illegal_argument(format!(
                            "[oversample] must be between [1.0] and [10.0], got [{}]",
                            crate::json::write::java_float(o)
                        )));
                    }
                    q.rescore_oversample = Some(o);
                }
                8 => {
                    let fs = p.array_items(parse_inner)?;
                    q.filter.extend(fs);
                }
                9 => boost = Some(p.float_value()?),
                _ => q.common.name = Some(p.text()?),
            }
            Ok(())
        })?;
        q.field = field.ok_or_else(|| EsError::illegal_argument("Required [field]"))?;
        let check = || -> EsResult<()> {
            if q.k.is_some_and(|k| k < 1) {
                return Err(EsError::illegal_argument("[k] must be greater than 0"));
            }
            if q.num_candidates.is_some_and(|n| n > 10000) {
                return Err(EsError::illegal_argument("[num_candidates] cannot exceed [10000]"));
            }
            if let (Some(k), Some(n)) = (q.k, q.num_candidates)
                && n < k
            {
                return Err(EsError::illegal_argument("[num_candidates] cannot be less than [k]"));
            }
            if q.visit_percentage.is_some_and(|v| !(0.0..=100.0).contains(&v)) {
                return Err(EsError::illegal_argument("[visit_percentage] must be between 0.0 and 100.0"));
            }
            if q.query_vector.is_none() && q.query_vector_builder.is_none() {
                return Err(EsError::illegal_argument("either [query_vector] or [query_vector_builder] must be provided"));
            }
            if q.query_vector.is_some() && q.query_vector_builder.is_some() {
                return Err(EsError::illegal_argument("only one of [query_vector] and [query_vector_builder] must be provided"));
            }
            Ok(())
        };
        check().map_err(|e| failed_to_build("knn", e))?;
        if let Some(b) = boost {
            q.common.boost = check_boost(b, || q.clone().into())?;
        }
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("field".into(), self.field.clone().into());
        if let Some(v) = &self.query_vector {
            m.insert("query_vector".into(), v.to_value());
        }
        if let Some(k) = self.k {
            m.insert("k".into(), k.into());
        }
        if let Some(n) = self.num_candidates {
            m.insert("num_candidates".into(), n.into());
        }
        if let Some(v) = self.visit_percentage {
            m.insert("visit_percentage".into(), float_value(v));
        }
        if let Some(s) = self.similarity {
            m.insert("similarity".into(), float_value(s));
        }
        if let Some(b) = &self.query_vector_builder {
            m.insert("query_vector_builder".into(), b.clone());
        }
        if !self.filter.is_empty() {
            m.insert("filter".into(), Value::Arr(self.filter.iter().map(Query::to_value).collect()));
        }
        if let Some(o) = self.rescore_oversample {
            m.insert("rescore_vector".into(), obj! { "oversample" => float_value(o) });
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// Token pruning settings of the sparse vector queries.
#[derive(Clone, Debug, PartialEq)]
pub struct PruningConfig {
    pub tokens_freq_ratio_threshold: Option<f32>,
    pub tokens_weight_threshold: Option<f32>,
    pub only_score_pruned_tokens: Option<bool>,
}

impl PruningConfig {
    fn parse(p: &mut XParser<'_>) -> EsResult<PruningConfig> {
        const FIELDS: &[Field] = &[
            Field::new("tokens_freq_ratio_threshold", Tokens::FLOAT),
            Field::new("tokens_weight_threshold", Tokens::FLOAT),
            Field::new("only_score_pruned_tokens", Tokens::BOOLEAN),
        ];
        let mut c = PruningConfig { tokens_freq_ratio_threshold: None, tokens_weight_threshold: None, only_score_pruned_tokens: None };
        p.object("pruning_config", FIELDS, |p, i| {
            match i {
                0 => {
                    let v = p.float_value()?;
                    if !(1.0..=100.0).contains(&v) {
                        return Err(EsError::illegal_argument(format!(
                            "[tokens_freq_ratio_threshold] must be between [1] and [100], got {}",
                            crate::json::write::java_float(v)
                        )));
                    }
                    c.tokens_freq_ratio_threshold = Some(v);
                }
                1 => {
                    let v = p.float_value()?;
                    if !(0.0..=1.0).contains(&v) {
                        return Err(EsError::illegal_argument(format!(
                            "[tokens_weight_threshold] must be between 0 and 1, got {}",
                            crate::json::write::java_float(v)
                        )));
                    }
                    c.tokens_weight_threshold = Some(v);
                }
                _ => c.only_score_pruned_tokens = Some(p.boolean_value()?),
            }
            Ok(())
        })?;
        Ok(c)
    }

    fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("tokens_freq_ratio_threshold".into(), float_value(self.tokens_freq_ratio_threshold.unwrap_or(5.0)));
        m.insert("tokens_weight_threshold".into(), float_value(self.tokens_weight_threshold.unwrap_or(0.4)));
        m.insert("only_score_pruned_tokens".into(), self.only_score_pruned_tokens.unwrap_or(false).into());
        Value::Obj(m)
    }
}

/// Token weights (`{"token": weight}`), as floats.
fn token_weights(p: &mut XParser<'_>) -> EsResult<Vec<(String, f32)>> {
    let mut out = Vec::new();
    let mut current = String::new();
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
            _ => out.push((current.clone(), p.float_value()?)),
        }
    }
    Ok(out)
}

fn weights_value(w: &[(String, f32)]) -> Value {
    let mut m = Map::new();
    for (k, v) in w {
        m.insert(k.clone(), float_value(*v));
    }
    Value::Obj(m)
}

/// `sparse_vector`.
#[derive(Clone, Debug, PartialEq)]
pub struct SparseVector {
    pub field: String,
    pub query_vector: Option<Vec<(String, f32)>>,
    pub inference_id: Option<String>,
    pub query: Option<String>,
    pub prune: Option<bool>,
    pub pruning_config: Option<PruningConfig>,
    pub common: Common,
}

impl SparseVector {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<SparseVector> {
        const FIELDS: &[Field] = &[
            Field::new("field", Tokens::STRING),
            Field::new("query_vector", Tokens::OBJECT),
            Field::new("inference_id", Tokens::STRING),
            Field::new("query", Tokens::STRING),
            Field::new("prune", Tokens::BOOLEAN),
            Field::new("pruning_config", Tokens::OBJECT),
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
        ];
        let mut q = SparseVector { field: String::new(), query_vector: None, inference_id: None, query: None, prune: None, pruning_config: None, common: Common::default() };
        let mut field = None;
        let mut boost = None;
        p.object("sparse_vector", FIELDS, |p, i| {
            match i {
                0 => field = Some(p.text()?),
                1 => q.query_vector = Some(token_weights(p)?),
                2 => q.inference_id = Some(p.text()?),
                3 => q.query = Some(p.text()?),
                4 => q.prune = Some(p.boolean_value()?),
                5 => q.pruning_config = Some(PruningConfig::parse(p)?),
                6 => boost = Some(p.float_value()?),
                _ => q.common.name = Some(p.text()?),
            }
            Ok(())
        })?;
        q.field = field.ok_or_else(|| EsError::illegal_argument("Required [field]"))?;
        if q.query_vector.is_none() && q.inference_id.is_none() && q.query.is_none() {
            let e = failed_to_build(
                "sparse_vector",
                EsError::illegal_argument("[sparse_vector] requires one of [query_vector] or [inference_id] for sparse_vector fields"),
            );
            return Err(p.parsing_error(e.reason().to_string()).caused_by(e));
        }
        if let Some(b) = boost {
            q.common.boost = check_boost(b, || q.clone().into())?;
        }
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("field".into(), self.field.clone().into());
        if let Some(v) = &self.query_vector {
            m.insert("query_vector".into(), weights_value(v));
        }
        super::opt_str(&mut m, "inference_id", &self.inference_id);
        super::opt_str(&mut m, "query", &self.query);
        if let Some(pr) = self.prune {
            m.insert("prune".into(), pr.into());
        }
        if let Some(c) = &self.pruning_config {
            m.insert("pruning_config".into(), c.to_value());
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// The `{"<field>": {...}}` shape of `text_expansion` and `weighted_tokens`.
fn single_field_object<'a, T>(
    p: &mut XParser<'a>,
    query: &str,
    mut body: impl FnMut(&mut XParser<'a>, &str) -> EsResult<T>,
) -> EsResult<(String, T, Common)> {
    let mut out = None;
    let mut common = Common::default();
    let mut current = String::new();
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
            Token::StartObject => {
                if out.is_some() {
                    return Err(p.parsing_error(format!("[{query}] query doesn't support multiple fields")));
                }
                let field = current.clone();
                let v = body(p, &field)?;
                out = Some((field, v));
            }
            t if t.is_value() => match current.as_str() {
                "boost" => common.boost = p.float_value()?,
                "_name" => common.name = Some(p.text()?),
                _ => return Err(p.parsing_error(format!("[{query}] unknown token [{t}] after [{current}]"))),
            },
            t => return Err(p.parsing_error(format!("[{query}] unknown token [{t}] after [{current}]"))),
        }
    }
    let Some((field, v)) = out else { return Err(p.parsing_error(format!("No fieldname specified for query [{query}]"))) };
    Ok((field, v, common))
}

/// `text_expansion` (deprecated in favour of `sparse_vector`).
#[derive(Clone, Debug, PartialEq)]
pub struct TextExpansion {
    pub field: String,
    pub model_id: String,
    pub model_text: String,
    pub pruning_config: Option<PruningConfig>,
    pub common: Common,
}

impl TextExpansion {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<TextExpansion> {
        p.warnings.push("text_expansion is deprecated. Use sparse_vector instead.".into());
        let (field, (model_id, model_text, pruning_config, boost, name), _) = single_field_object(p, "text_expansion", |p, _| {
            let (mut id, mut text, mut cfg, mut boost, mut name) = (None, None, None, 1.0f32, None);
            let mut current = String::new();
            while let Some(t) = p.next_token() {
                match t {
                    Token::EndObject => break,
                    Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                    Token::StartObject if current == "pruning_config" => cfg = Some(PruningConfig::parse(p)?),
                    t if t.is_value() => match current.as_str() {
                        "model_id" => id = Some(p.text()?),
                        "model_text" => text = Some(p.text()?),
                        "boost" => boost = p.float_value()?,
                        "_name" => name = Some(p.text()?),
                        _ => return Err(p.parsing_error(format!("[text_expansion] query does not support [{current}]"))),
                    },
                    t => return Err(p.parsing_error(format!("[text_expansion] unknown token [{t}] after [{current}]"))),
                }
            }
            let id = id.ok_or_else(|| p.parsing_error("No model_id specified for text query"))?;
            let text = text.ok_or_else(|| p.parsing_error("No model_text specified for text query"))?;
            Ok((id, text, cfg, boost, name))
        })?;
        let mut q = TextExpansion { field, model_id, model_text, pruning_config, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("model_text".into(), self.model_text.clone().into());
        m.insert("model_id".into(), self.model_id.clone().into());
        if let Some(c) = &self.pruning_config {
            m.insert("pruning_config".into(), c.to_value());
        }
        self.common.write_if_set(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// `weighted_tokens` (deprecated in favour of `sparse_vector`).
#[derive(Clone, Debug, PartialEq)]
pub struct WeightedTokens {
    pub field: String,
    pub tokens: Vec<(String, f32)>,
    pub pruning_config: Option<PruningConfig>,
    pub common: Common,
}

impl WeightedTokens {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<WeightedTokens> {
        p.warnings.push("weighted_tokens is deprecated and will be removed. Use sparse_vector instead.".into());
        let (field, (tokens, pruning_config, boost, name), _) = single_field_object(p, "weighted_tokens", |p, _| {
            let (mut tokens, mut cfg, mut boost, mut name) = (None, None, 1.0f32, None);
            let mut current = String::new();
            while let Some(t) = p.next_token() {
                match t {
                    Token::EndObject => break,
                    Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                    Token::StartObject if current == "tokens" => tokens = Some(token_weights(p)?),
                    Token::StartObject if current == "pruning_config" => cfg = Some(PruningConfig::parse(p)?),
                    t if t.is_value() => match current.as_str() {
                        "boost" => boost = p.float_value()?,
                        "_name" => name = Some(p.text()?),
                        _ => return Err(p.parsing_error(format!("[weighted_tokens] query does not support [{current}]"))),
                    },
                    t => return Err(p.parsing_error(format!("[weighted_tokens] unknown token [{t}] after [{current}]"))),
                }
            }
            let tokens = tokens.ok_or_else(|| p.parsing_error("No tokens specified for weighted_tokens query"))?;
            Ok((tokens, cfg, boost, name))
        })?;
        let mut q = WeightedTokens { field, tokens, pruning_config, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("tokens".into(), weights_value(&self.tokens));
        if let Some(c) = &self.pruning_config {
            m.insert("pruning_config".into(), c.to_value());
        }
        self.common.write_if_set(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// `semantic` (needs a `semantic_text` field, which roll doesn't support).
#[derive(Clone, Debug, PartialEq)]
pub struct Semantic {
    pub field: String,
    pub query: String,
    pub lenient: Option<bool>,
    pub common: Common,
}

impl Semantic {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Semantic> {
        const FIELDS: &[Field] = &[
            Field::new("field", Tokens::STRING),
            Field::new("query", Tokens::STRING),
            Field::new("lenient", Tokens::BOOLEAN),
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
        ];
        let (mut field, mut query, mut lenient, mut boost, mut name) = (None, None, None, None, None);
        p.object("semantic", FIELDS, |p, i| {
            match i {
                0 => field = Some(p.text()?),
                1 => query = Some(p.text()?),
                2 => lenient = Some(p.boolean_value()?),
                3 => boost = Some(p.float_value()?),
                _ => name = Some(p.text()?),
            }
            Ok(())
        })?;
        let missing: Vec<&str> = [("field", field.is_none()), ("query", query.is_none())].iter().filter(|x| x.1).map(|x| x.0).collect();
        if !missing.is_empty() {
            return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
        }
        let mut q = Semantic { field: field.unwrap_or_default(), query: query.unwrap_or_default(), lenient, common: Common::default() };
        if let Some(b) = boost {
            q.common.boost = check_boost(b, || q.clone().into())?;
        }
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("field".into(), self.field.clone().into());
        m.insert("query".into(), self.query.clone().into());
        if let Some(l) = self.lenient {
            m.insert("lenient".into(), l.into());
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// `rule` (query rules over an organic query).
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub organic: Query,
    pub ruleset_ids: Vec<String>,
    pub match_criteria: Map,
    pub common: Common,
}

impl Rule {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Rule> {
        const FIELDS: &[Field] = &[
            Field::new("organic", Tokens::OBJECT),
            Field::new("ruleset_ids", Tokens::STRING_ARRAY),
            Field::new("ruleset_id", Tokens::STRING),
            Field::new("match_criteria", Tokens::OBJECT),
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
        ];
        let (mut organic, mut ids, mut criteria, mut boost, mut name) = (None, Vec::new(), None, None, None);
        p.object("rule", FIELDS, |p, i| {
            match i {
                0 => organic = Some(parse_inner(p)?),
                1 => ids.extend(p.array_items(|p| p.text())?),
                2 => ids.push(p.text()?),
                3 => criteria = Some(p.map()?),
                4 => boost = Some(p.float_value()?),
                _ => name = Some(p.text()?),
            }
            Ok(())
        })?;
        let missing: Vec<&str> = [("organic", organic.is_none()), ("match_criteria", criteria.is_none())].iter().filter(|x| x.1).map(|x| x.0).collect();
        if !missing.is_empty() {
            return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
        }
        if ids.is_empty() {
            return Err(failed_to_build("rule", EsError::illegal_argument("[ruleset_ids] must not be empty")));
        }
        let mut q = Rule { organic: organic.unwrap_or_else(|| super::compound::MatchAll::default().into()), ruleset_ids: ids, match_criteria: criteria.unwrap_or_default(), common: Common::default() };
        if let Some(b) = boost {
            q.common.boost = check_boost(b, || q.clone().into())?;
        }
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("organic".into(), self.organic.to_value());
        m.insert("ruleset_ids".into(), Value::from(self.ruleset_ids.clone()));
        m.insert("match_criteria".into(), Value::Obj(self.match_criteria.clone()));
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}
