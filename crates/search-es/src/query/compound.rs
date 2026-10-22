//! Compound queries: `match_all`, `match_none`, `bool`, `boosting`, `constant_score`, `dis_max`,
//! `function_score` (with its score functions) and `script_score`.

use super::script::Script;
use super::xparser::{Field, Tokens, float_value, is_illegal_argument};
use super::{Common, Query, Token, XParser, check_boost, parse_inner};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};

/// `match_all`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct MatchAll {
    pub common: Common,
}

/// `declareStandardFields`: `boost` and `_name`.
const STANDARD: &[Field] = &[Field::new("boost", Tokens::FLOAT), Field::new("_name", Tokens::STRING)];

impl MatchAll {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<MatchAll> {
        let mut q = MatchAll::default();
        let r = p.object("match_all", STANDARD, |p, i| {
            if i == 0 {
                let b = p.float_value()?;
                q.common.boost = check_boost(b, || q.clone().into())?;
            } else {
                q.common.name = Some(p.text()?);
            }
            Ok(())
        });
        match r {
            Ok(()) => Ok(q),
            Err(e) if is_illegal_argument(&e) => Err(p.parsing_error(e.reason().to_string()).caused_by(e)),
            Err(e) => Err(e),
        }
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// `match_none`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct MatchNone {
    pub common: Common,
}

impl MatchNone {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<MatchNone> {
        let mut boost = 1.0f32;
        let mut name = None;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject | Token::EndArray => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                t if t.is_value() => match current.as_str() {
                    "_name" => name = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    _ => return Err(p.parsing_error(format!("[match_none] query does not support [{current}]"))),
                },
                t => return Err(p.parsing_error(format!("[match_none] unknown token [{t}] after [{current}]"))),
            }
        }
        let mut q = MatchNone::default();
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// `bool`.
#[derive(Clone, Debug, PartialEq)]
pub struct Bool {
    pub must: Vec<Query>,
    pub filter: Vec<Query>,
    pub must_not: Vec<Query>,
    pub should: Vec<Query>,
    pub adjust_pure_negative: bool,
    pub minimum_should_match: Option<String>,
    pub common: Common,
}

impl Default for Bool {
    fn default() -> Self {
        Bool {
            must: Vec::new(),
            filter: Vec::new(),
            must_not: Vec::new(),
            should: Vec::new(),
            adjust_pure_negative: true,
            minimum_should_match: None,
            common: Common::default(),
        }
    }
}

/// The clauses of an `OBJECT_ARRAY_OR_NULL` field: one query, an array of them, or null.
fn clause_list(p: &mut XParser<'_>) -> EsResult<Option<Vec<Query>>> {
    if p.token() == Token::Null {
        return Ok(None);
    }
    p.array_items(parse_inner).map(Some)
}

impl Bool {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Bool> {
        const FIELDS: &[Field] = &[
            Field::new("must", Tokens::of(&[Token::StartObject, Token::StartArray, Token::Null])),
            Field::new("should", Tokens::of(&[Token::StartObject, Token::StartArray, Token::Null])),
            Field::with_deprecated("must_not", &["mustNot"], Tokens::of(&[Token::StartObject, Token::StartArray, Token::Null])),
            Field::new("filter", Tokens::of(&[Token::StartObject, Token::StartArray, Token::Null])),
            Field::new("adjust_pure_negative", Tokens::BOOLEAN),
            Field::new("minimum_should_match", Tokens::VALUE),
            Field::new("_name", Tokens::STRING),
            Field::new("boost", Tokens::FLOAT),
        ];
        let mut q = Bool::default();
        p.object("bool", FIELDS, |p, i| {
            match i {
                0..=3 => {
                    // A null clause list adds nothing.
                    let clauses = clause_list(p)?.unwrap_or_default();
                    let target = match i {
                        0 => &mut q.must,
                        1 => &mut q.should,
                        2 => &mut q.must_not,
                        _ => &mut q.filter,
                    };
                    target.extend(clauses);
                }
                4 => q.adjust_pure_negative = p.boolean_value()?,
                5 => q.minimum_should_match = p.text_or_null()?,
                6 => q.common.name = Some(p.text()?),
                _ => {
                    let b = p.float_value()?;
                    q.common.boost = check_boost(b, || q.clone().into())?;
                }
            }
            Ok(())
        })?;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        for (k, v) in [("must", &self.must), ("filter", &self.filter), ("must_not", &self.must_not), ("should", &self.should)] {
            if !v.is_empty() {
                m.insert(k.into(), Value::Arr(v.iter().map(Query::to_value).collect()));
            }
        }
        // OpenSearch always prints it.
        if !self.adjust_pure_negative || super::opensearch_print() {
            m.insert("adjust_pure_negative".into(), self.adjust_pure_negative.into());
        }
        super::opt_str(&mut m, "minimum_should_match", &self.minimum_should_match);
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// `boosting`.
#[derive(Clone, Debug, PartialEq)]
pub struct Boosting {
    pub positive: Query,
    pub negative: Query,
    pub negative_boost: f32,
    pub common: Common,
}

impl Boosting {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Boosting> {
        let (mut positive, mut negative) = (None, None);
        let mut negative_boost = -1.0f32;
        let mut boost = 1.0f32;
        let mut name = None;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => match current.as_str() {
                    "positive" => positive = Some(parse_inner(p)?),
                    "negative" => negative = Some(parse_inner(p)?),
                    _ => return Err(p.parsing_error(format!("[boosting] query does not support [{current}]"))),
                },
                t if t.is_value() => match current.as_str() {
                    "negative_boost" => negative_boost = p.float_value()?,
                    "_name" => name = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    _ => return Err(p.parsing_error(format!("[boosting] query does not support [{current}]"))),
                },
                _ => {}
            }
        }
        let Some(positive) = positive else { return Err(p.parsing_error("[boosting] query requires 'positive' query to be set'")) };
        let Some(negative) = negative else { return Err(p.parsing_error("[boosting] query requires 'negative' query to be set'")) };
        if negative_boost < 0.0 {
            return Err(p.parsing_error("[boosting] query requires 'negative_boost' to be set to be a positive value'"));
        }
        let mut q = Boosting { positive, negative, negative_boost, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("positive".into(), self.positive.to_value());
        m.insert("negative".into(), self.negative.to_value());
        m.insert("negative_boost".into(), float_value(self.negative_boost));
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// `constant_score`.
#[derive(Clone, Debug, PartialEq)]
pub struct ConstantScore {
    pub filter: Query,
    pub common: Common,
}

impl ConstantScore {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<ConstantScore> {
        let mut filter = None;
        let mut boost = 1.0f32;
        let mut name = None;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => {
                    if current == "filter" {
                        filter = Some(parse_inner(p)?);
                    } else {
                        return Err(p.parsing_error(format!("[constant_score] query does not support [{current}]")));
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "_name" => name = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    _ => return Err(p.parsing_error(format!("[constant_score] query does not support [{current}]"))),
                },
                t => return Err(p.parsing_error(format!("unexpected token [{t}]"))),
            }
        }
        let Some(filter) = filter else { return Err(p.parsing_error("[constant_score] requires a 'filter' element")) };
        let mut q = ConstantScore { filter, common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("filter".into(), self.filter.to_value());
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// `dis_max`.
#[derive(Clone, Debug, PartialEq)]
pub struct DisMax {
    pub queries: Vec<Query>,
    pub tie_breaker: f32,
    pub common: Common,
}

impl DisMax {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<DisMax> {
        let mut queries = Vec::new();
        let mut found = false;
        let mut tie_breaker = 0.0f32;
        let mut boost = 1.0f32;
        let mut name = None;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => {
                    if current == "queries" {
                        found = true;
                        queries.push(parse_inner(p)?);
                    } else {
                        return Err(p.parsing_error(format!("[dis_max] query does not support [{current}]")));
                    }
                }
                Token::StartArray => {
                    if current == "queries" {
                        found = true;
                        let mut t = Token::StartArray;
                        while t != Token::EndArray {
                            queries.push(parse_inner(p)?);
                            t = p.next_token().unwrap_or(Token::EndArray);
                        }
                    } else {
                        return Err(p.parsing_error(format!("[dis_max] query does not support [{current}]")));
                    }
                }
                _ => match current.as_str() {
                    "boost" => boost = p.float_value()?,
                    "tie_breaker" => tie_breaker = p.float_value()?,
                    "_name" => name = Some(p.text()?),
                    _ => return Err(p.parsing_error(format!("[dis_max] query does not support [{current}]"))),
                },
            }
        }
        if !found {
            return Err(p.parsing_error("[dis_max] requires 'queries' field with at least one clause"));
        }
        let mut q = DisMax { queries: Vec::new(), tie_breaker, common: Common::default() };
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.queries = queries;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("tie_breaker".into(), float_value(self.tie_breaker));
        m.insert("queries".into(), Value::Arr(self.queries.iter().map(Query::to_value).collect()));
        self.common.write_always(&mut m);
        Value::Obj(m)
    }
}

/// How `function_score` combines function scores (`FunctionScoreQuery.ScoreMode`).
pub const SCORE_MODES: &[&str] = &["first", "avg", "max", "sum", "min", "multiply"];
/// How the function score combines with the query score (`CombineFunction`).
pub const BOOST_MODES: &[&str] = &["multiply", "replace", "sum", "avg", "max", "min"];
/// `FieldValueFactorFunction.Modifier`.
pub const MODIFIERS: &[&str] = &["none", "log", "log1p", "log2p", "ln", "ln1p", "ln2p", "square", "sqrt", "reciprocal"];
/// The registered score function names (candidates of "did you mean").
pub const SCORE_FUNCTION_NAMES: &[&str] = &["script_score", "random_score", "field_value_factor", "gauss", "linear", "exp", "weight"];

/// `MultiValueMode`.
pub const MULTI_VALUE_MODES: &[&str] = &["SUM", "AVG", "MEDIAN", "MIN", "MAX"];

/// `Enum.valueOf(text.toUpperCase())` over lower-case names, failing with Java's text.
fn java_enum(text: &str, names: &'static [&'static str], class: &str) -> EsResult<&'static str> {
    let upper = text.to_uppercase();
    names
        .iter()
        .find(|n| n.to_uppercase() == upper)
        .copied()
        .ok_or_else(|| EsError::illegal_argument(format!("No enum constant {class}.{upper}")))
}

/// A score function body.
#[derive(Clone, Debug, PartialEq)]
pub enum ScoreFunctionKind {
    Weight,
    Script(Script),
    Random { seed: Option<i32>, field: Option<String> },
    FieldValueFactor { field: String, factor: f32, missing: Option<f64>, modifier: &'static str },
    /// `gauss`, `exp` or `linear`: the field's settings kept as given.
    Decay { kind: &'static str, field: String, body: Value, multi_value_mode: &'static str },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScoreFunction {
    pub weight: Option<f32>,
    pub kind: ScoreFunctionKind,
}

impl ScoreFunction {
    pub fn name(&self) -> &'static str {
        match &self.kind {
            ScoreFunctionKind::Weight => "weight",
            ScoreFunctionKind::Script(_) => "script_score",
            ScoreFunctionKind::Random { .. } => "random_score",
            ScoreFunctionKind::FieldValueFactor { .. } => "field_value_factor",
            ScoreFunctionKind::Decay { kind, .. } => kind,
        }
    }

    fn set_weight(&mut self, w: f32) -> EsResult<()> {
        if w < 0.0 {
            return Err(EsError::illegal_argument("[weight] cannot be negative for a filtering function"));
        }
        self.weight = Some(w);
        Ok(())
    }

    /// Parses the named function (`parser.namedObject(ScoreFunctionBuilder.class, name)`).
    fn parse_named(name: &str, p: &mut XParser<'_>) -> EsResult<ScoreFunction> {
        let kind = match name {
            "script_score" => {
                let mut script = None;
                let mut current = String::new();
                while let Some(t) = p.next_token() {
                    match t {
                        Token::EndObject => break,
                        Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                        _ => {
                            if current == "script" {
                                script = Some(Script::parse(p)?);
                            } else {
                                return Err(p.parsing_error(format!("script_score query does not support [{current}]")));
                            }
                        }
                    }
                }
                let Some(s) = script else { return Err(p.parsing_error("script_score requires 'script' field")) };
                ScoreFunctionKind::Script(s)
            }
            "random_score" => {
                let (mut seed, mut field) = (None, None);
                let mut current = String::new();
                while let Some(t) = p.next_token() {
                    match t {
                        Token::EndObject => break,
                        Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                        t if t.is_value() => match current.as_str() {
                            "seed" => match (t, p.value()) {
                                (Token::Number, Value::Num(crate::json::Number::Int(i))) => {
                                    seed = Some(match i32::try_from(*i) {
                                        Ok(v) => v,
                                        // Long.hashCode.
                                        Err(_) => ((*i as u64) ^ ((*i as u64) >> 32)) as i32,
                                    });
                                }
                                (Token::Number, _) => {
                                    return Err(p.parsing_error("random_score seed must be an int, long or string, not 'VALUE_NUMBER'"));
                                }
                                (Token::String, _) => seed = Some(crate::json::java_string_hash(&p.text()?)),
                                (t, _) => {
                                    return Err(p.parsing_error(format!("random_score seed must be an int/long or string, not '{t}'")));
                                }
                            },
                            "field" => field = Some(p.text()?),
                            _ => return Err(p.parsing_error(format!("random_score query does not support [{current}]"))),
                        },
                        _ => {}
                    }
                }
                ScoreFunctionKind::Random { seed, field }
            }
            "field_value_factor" => {
                let (mut field, mut factor, mut missing, mut modifier) = (None, 1.0f32, None, "none");
                let mut current = String::new();
                while let Some(t) = p.next_token() {
                    match t {
                        Token::EndObject => break,
                        Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                        t if t.is_value() => match current.as_str() {
                            "field" => field = Some(p.text()?),
                            "factor" => factor = p.float_value()?,
                            "modifier" => {
                                modifier = java_enum(
                                    &p.text()?,
                                    MODIFIERS,
                                    "org.elasticsearch.common.lucene.search.function.FieldValueFactorFunction.Modifier",
                                )?
                            }
                            "missing" => missing = Some(p.double_value()?),
                            _ => return Err(p.parsing_error(format!("field_value_factor query does not support [{current}]"))),
                        },
                        Token::StartArray | Token::StartObject if current == "factor" => {
                            return Err(p.parsing_error("[field_value_factor] field 'factor' does not support lists or objects"));
                        }
                        _ => {}
                    }
                }
                let Some(field) = field else { return Err(p.parsing_error("[field_value_factor] required field 'field' missing")) };
                ScoreFunctionKind::FieldValueFactor { field, factor, missing, modifier }
            }
            "gauss" | "exp" | "linear" => {
                let kind: &'static str = match name {
                    "gauss" => "gauss",
                    "exp" => "exp",
                    _ => "linear",
                };
                let mut mvm = "MIN";
                let (mut field, mut body) = (None, None);
                while p.next_token() == Some(Token::FieldName) {
                    let current = p.current_name().unwrap_or("").to_string();
                    let t = p.next_token();
                    if t == Some(Token::StartObject) {
                        field = Some(current);
                        body = Some(p.take_value());
                    } else if current == "multi_value_mode" {
                        let text = p.text()?;
                        mvm = MULTI_VALUE_MODES
                            .iter()
                            .find(|m| **m == text.to_uppercase())
                            .copied()
                            .ok_or_else(|| EsError::illegal_argument(format!("Illegal sort mode: {text}")))?;
                    } else {
                        return Err(p.parsing_error("malformed score function score parameters."));
                    }
                }
                let (Some(field), Some(body)) = (field, body) else {
                    return Err(p.parsing_error("malformed score function score parameters."));
                };
                ScoreFunctionKind::Decay { kind, field, body, multi_value_mode: mvm }
            }
            _ => {
                let (l, c) = p.token_location();
                return Err(EsError::new(400, "named_object_not_found_exception", format!("[{l}:{c}] unknown field [{name}]")));
            }
        };
        Ok(ScoreFunction { weight: None, kind })
    }

    /// `ScoreFunctionBuilder.toXContent` into the enclosing object.
    fn write(&self, m: &mut Map) {
        if let Some(w) = self.weight {
            m.insert("weight".into(), float_value(w));
        }
        let body = match &self.kind {
            ScoreFunctionKind::Weight => return,
            ScoreFunctionKind::Script(s) => {
                let mut b = Map::new();
                b.insert("script".into(), s.to_value());
                b
            }
            ScoreFunctionKind::Random { seed, field } => {
                let mut b = Map::new();
                if let Some(s) = seed {
                    b.insert("seed".into(), (*s).into());
                }
                super::opt_str(&mut b, "field", field);
                b
            }
            ScoreFunctionKind::FieldValueFactor { field, factor, missing, modifier } => {
                let mut b = Map::new();
                b.insert("field".into(), field.clone().into());
                b.insert("factor".into(), float_value(*factor));
                if let Some(x) = missing {
                    b.insert("missing".into(), (*x).into());
                }
                b.insert("modifier".into(), (*modifier).into());
                b
            }
            ScoreFunctionKind::Decay { field, body, multi_value_mode, .. } => {
                let mut b = Map::new();
                b.insert(field.clone(), body.clone());
                b.insert("multi_value_mode".into(), (*multi_value_mode).into());
                b
            }
        };
        m.insert(self.name().into(), Value::Obj(body));
    }
}

/// One entry of `function_score.functions`.
#[derive(Clone, Debug, PartialEq)]
pub struct FilterFunction {
    pub filter: Query,
    pub name: Option<String>,
    pub function: ScoreFunction,
}

/// `function_score`.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionScore {
    pub query: Query,
    pub functions: Vec<FilterFunction>,
    pub score_mode: &'static str,
    pub boost_mode: Option<&'static str>,
    pub max_boost: f32,
    pub min_score: Option<f32>,
    pub common: Common,
}

const MISPLACED: &str = "you can either define [functions] array or a single function, not both. ";

impl FunctionScore {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<FunctionScore> {
        let mut query: Option<Query> = None;
        let mut boost = 1.0f32;
        let mut name = None;
        let mut score_mode = "multiply";
        let mut max_boost = f32::MAX;
        let mut min_score = None;
        let mut boost_mode = None;
        let mut array_found = false;
        let mut single: Option<String> = None;
        let mut functions: Vec<FilterFunction> = Vec::new();
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => {
                    if current == "query" {
                        if query.is_some() {
                            return Err(p.parsing_error("failed to parse [function_score] query. [query] is already defined."));
                        }
                        query = Some(parse_inner(p)?);
                    } else {
                        if let Some(s) = &single {
                            return Err(p.parsing_error(format!(
                                "failed to parse [function_score] query. already found function [{s}], now encountering [{current}]. \
                                 use [functions] array if you want to define several functions."
                            )));
                        }
                        if array_found {
                            return Err(p.parsing_error(format!(
                                "failed to parse [function_score] query. [{MISPLACED}already found [functions] array, now encountering [{current}].]"
                            )));
                        }
                        single = Some(current.clone());
                        let f = ScoreFunction::parse_named(&current, p)?;
                        functions.push(FilterFunction { filter: super::compound::MatchAll::default().into(), name: None, function: f });
                    }
                }
                Token::StartArray => {
                    if current == "functions" {
                        if let Some(s) = &single {
                            return Err(p.parsing_error(format!(
                                "failed to parse [function_score] query. [{MISPLACED}already found [{s}], now encountering [functions].]"
                            )));
                        }
                        array_found = true;
                        if let Some(c) = parse_functions(p, &mut functions)? {
                            current = c;
                        }
                    } else {
                        return Err(p.parsing_error(format!("failed to parse [function_score] query. array [{current}] is not supported")));
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "score_mode" => {
                        score_mode = java_enum(&p.text()?, SCORE_MODES, "org.elasticsearch.common.lucene.search.function.FunctionScoreQuery.ScoreMode")?
                    }
                    "boost_mode" => {
                        boost_mode =
                            Some(java_enum(&p.text()?, BOOST_MODES, "org.elasticsearch.common.lucene.search.function.CombineFunction")?)
                    }
                    "max_boost" => max_boost = p.float_value()?,
                    "boost" => boost = p.float_value()?,
                    "_name" => name = Some(p.text()?),
                    "min_score" => min_score = Some(p.float_value()?),
                    _ => {
                        if let Some(s) = &single {
                            return Err(p.parsing_error(format!(
                                "failed to parse [function_score] query. already found function [{s}], now encountering [{current}]. \
                                 use [functions] array if you want to define several functions."
                            )));
                        }
                        if array_found {
                            return Err(p.parsing_error(format!(
                                "failed to parse [function_score] query. [{MISPLACED}already found [functions] array, now encountering [{current}].]"
                            )));
                        }
                        if current == "weight" {
                            let mut f = ScoreFunction { weight: None, kind: ScoreFunctionKind::Weight };
                            f.set_weight(p.float_value()?)?;
                            functions.push(FilterFunction { filter: MatchAll::default().into(), name: None, function: f });
                            single = Some(current.clone());
                        } else {
                            return Err(p.parsing_error(format!(
                                "failed to parse [function_score] query. field [{current}] is not supported"
                            )));
                        }
                    }
                },
                _ => {}
            }
        }
        let query = query.unwrap_or_else(|| MatchAll::default().into());
        let mut q = FunctionScore { query, functions, score_mode: "multiply", boost_mode: None, max_boost: f32::MAX, min_score: None, common: Common::default() };
        q.boost_mode = boost_mode;
        q.score_mode = score_mode;
        q.max_boost = max_boost;
        q.min_score = min_score;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.to_value());
        let fs: Vec<Value> = self
            .functions
            .iter()
            .map(|f| {
                let mut fm = Map::new();
                fm.insert("filter".into(), f.filter.to_value());
                f.function.write(&mut fm);
                super::opt_str(&mut fm, "_name", &f.name);
                Value::Obj(fm)
            })
            .collect();
        m.insert("functions".into(), Value::Arr(fs));
        if self.score_mode != "multiply" {
            m.insert("score_mode".into(), self.score_mode.into());
        }
        if let Some(b) = self.boost_mode {
            m.insert("boost_mode".into(), b.into());
        }
        if self.max_boost != f32::MAX {
            m.insert("max_boost".into(), float_value(self.max_boost));
        }
        if let Some(s) = self.min_score {
            m.insert("min_score".into(), float_value(s));
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// `parseFiltersAndFunctions`: the `functions` array. Returns the last field name seen.
fn parse_functions(p: &mut XParser<'_>, out: &mut Vec<FilterFunction>) -> EsResult<Option<String>> {
    let mut current: Option<String> = None;
    while let Some(t) = p.next_token() {
        if t == Token::EndArray {
            break;
        }
        let mut filter = None;
        let mut function: Option<ScoreFunction> = None;
        let mut name = None;
        let mut weight = None;
        if t != Token::StartObject {
            return Err(p.parsing_error(format!(
                "failed to parse [START_OBJECT]. malformed query, expected a [{t}] while parsing functions but got a [function_score] instead"
            )));
        }
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().map(str::to_string),
                Token::StartObject => {
                    let key = current.clone().unwrap_or_default();
                    if key == "filter" {
                        filter = Some(parse_inner(p)?);
                    } else {
                        if let Some(f) = &function {
                            return Err(p.parsing_error(format!(
                                "failed to parse function_score functions. already found [{}], now encountering [{key}].",
                                f.name()
                            )));
                        }
                        function = Some(ScoreFunction::parse_named(&key, p)?);
                    }
                }
                t if t.is_value() => {
                    let key = current.clone().unwrap_or_default();
                    match key.as_str() {
                        "weight" => weight = Some(p.float_value()?),
                        "_name" => name = Some(p.text()?),
                        _ => {
                            return Err(p.parsing_error(format!("failed to parse [function_score] query. field [{key}] is not supported")));
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(w) = weight {
            match &mut function {
                Some(f) => f.set_weight(w)?,
                None => {
                    let mut f = ScoreFunction { weight: None, kind: ScoreFunctionKind::Weight };
                    f.set_weight(w)?;
                    function = Some(f);
                }
            }
        }
        let filter = filter.unwrap_or_else(|| MatchAll::default().into());
        let Some(function) = function else {
            return Err(p.parsing_error("failed to parse [function_score] query. an entry in functions list is missing a function."));
        };
        out.push(FilterFunction { filter, name, function });
    }
    Ok(current)
}

/// `script_score` (the query).
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptScore {
    pub query: Query,
    pub script: Script,
    pub min_score: Option<f32>,
    pub common: Common,
}

impl ScriptScore {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<ScriptScore> {
        const FIELDS: &[Field] = &[
            Field::new("query", Tokens::OBJECT),
            Field::new("script", Tokens::OBJECT),
            Field::new("min_score", Tokens::FLOAT),
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
        ];
        let (mut query, mut script, mut min_score, mut boost, mut name) = (None, None, None, None, None);
        p.object("script_score", FIELDS, |p, i| {
            match i {
                0 => query = Some(parse_inner(p)?),
                1 => script = Some(Script::parse(p)?),
                2 => min_score = Some(p.float_value()?),
                3 => boost = Some(p.float_value()?),
                _ => name = Some(p.text()?),
            }
            Ok(())
        })?;
        let missing: Vec<&str> = [("query", query.is_none()), ("script", script.is_none())].iter().filter(|x| x.1).map(|x| x.0).collect();
        if !missing.is_empty() {
            return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
        }
        let mut q = ScriptScore { query: query.unwrap_or_else(|| MatchAll::default().into()), script: script.unwrap_or_else(|| Script::inline("")), min_score: None, common: Common::default() };
        q.min_score = min_score;
        if let Some(b) = boost {
            q.common.boost = check_boost(b, || q.clone().into())?;
        }
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.to_value());
        m.insert("script".into(), self.script.to_value());
        if let Some(s) = self.min_score {
            m.insert("min_score".into(), float_value(s));
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}
