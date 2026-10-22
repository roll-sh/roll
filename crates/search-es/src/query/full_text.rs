//! Full-text queries: the `match` family, `multi_match`, `combined_fields`, `query_string`,
//! `simple_query_string` and `intervals`.
//!
//! Analysis happens when a query runs against an index; here the builders only keep what the
//! request asked for, validated and printed as Elasticsearch does (defaults left out).

use super::script::Script;
use super::term_level::{Fuzziness, java_zone_id, wrap_java};
use super::xparser::{Field, Tokens, float_value, java_parse_float};
use super::{Common, Query, Token, XParser, check_boost, check_single_field};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;

/// `Operator`: `OR` (default) or `AND`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Operator {
    #[default]
    Or,
    And,
}

impl Operator {
    /// `Operator.fromString`: `Enum.valueOf` on the upper-cased name.
    pub fn parse_name(s: &str) -> EsResult<Operator> {
        match s.to_uppercase().as_str() {
            "OR" => Ok(Operator::Or),
            "AND" => Ok(Operator::And),
            u => Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.index.query.Operator.{u}"))),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Operator::Or => "OR",
            Operator::And => "AND",
        }
    }
}

/// `ZeroTermsQueryOption`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ZeroTerms {
    #[default]
    None,
    All,
}

impl ZeroTerms {
    pub fn name(self) -> &'static str {
        match self {
            ZeroTerms::None => "NONE",
            ZeroTerms::All => "ALL",
        }
    }

    fn parse(p: &XParser<'_>) -> EsResult<ZeroTerms> {
        let v = p.text()?;
        if v.eq_ignore_ascii_case("none") {
            Ok(ZeroTerms::None)
        } else if v.eq_ignore_ascii_case("all") {
            Ok(ZeroTerms::All)
        } else {
            Err(p.parsing_error(format!("Unsupported zero_terms_query value [{v}]")))
        }
    }
}

/// The `match` family's shape: `{"<field>": "text"}` or `{"<field>": {"query": …, options}}`.
fn parse_field_query<'a>(
    p: &mut XParser<'a>,
    query: &str,
    mut option: impl FnMut(&mut XParser<'a>, &str) -> EsResult<bool>,
) -> EsResult<(Option<String>, Option<Value>)> {
    let mut field: Option<String> = None;
    let mut value: Option<Value> = None;
    let mut current: Option<String> = None;
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => current = p.current_name().map(str::to_string),
            Token::StartObject => {
                check_single_field(p, query, &field, current.as_deref())?;
                field = current.clone();
                while let Some(t) = p.next_token() {
                    match t {
                        Token::EndObject => break,
                        Token::FieldName => current = p.current_name().map(str::to_string),
                        t if t.is_value() => {
                            let key = current.clone().unwrap_or_default();
                            if key == "query" {
                                value = p.object_text()?;
                            } else if !option(p, &key)? {
                                return Err(p.parsing_error(format!("[{query}] query does not support [{key}]")));
                            }
                        }
                        t => {
                            return Err(p.parsing_error(format!(
                                "[{query}] unknown token [{t}] after [{}]",
                                current.as_deref().unwrap_or("null")
                            )));
                        }
                    }
                }
            }
            _ => {
                check_single_field(p, query, &field, p.current_name())?;
                field = p.current_name().map(str::to_string);
                value = p.object_text()?;
            }
        }
    }
    Ok((field, value))
}

fn require_field_value(query: &str, field: Option<String>, value: Option<Value>) -> EsResult<(String, Value)> {
    let field = field.ok_or_else(|| EsError::illegal_argument(format!("[{query}] requires fieldName")))?;
    let value = value.ok_or_else(|| EsError::illegal_argument(format!("[{query}] requires query value")))?;
    Ok((field, value))
}

/// Options shared by the fuzzy-capable full-text queries.
#[derive(Clone, Debug, PartialEq)]
pub struct FuzzyOpts {
    pub fuzziness: Option<Fuzziness>,
    pub prefix_length: i32,
    pub max_expansions: i32,
    pub fuzzy_transpositions: bool,
    pub fuzzy_rewrite: Option<String>,
}

impl Default for FuzzyOpts {
    fn default() -> Self {
        FuzzyOpts { fuzziness: None, prefix_length: 0, max_expansions: 50, fuzzy_transpositions: true, fuzzy_rewrite: None }
    }
}

/// `match`.
#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub field: String,
    pub query: Value,
    pub operator: Operator,
    pub analyzer: Option<String>,
    pub fuzzy: FuzzyOpts,
    pub minimum_should_match: Option<String>,
    pub lenient: bool,
    pub zero_terms_query: ZeroTerms,
    pub auto_generate_synonyms_phrase_query: bool,
    pub common: Common,
}

impl Match {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Match> {
        let mut boost = 1.0f32;
        let (mut msm, mut analyzer, mut name) = (None, None, None);
        let mut operator = Operator::Or;
        let mut fz = FuzzyOpts::default();
        let (mut lenient, mut zero, mut auto_syn) = (false, ZeroTerms::None, true);
        let (field, value) = parse_field_query(p, "match", |p, k| {
            match k {
                "analyzer" => analyzer = Some(p.text()?),
                "boost" => boost = p.float_value()?,
                "fuzziness" => fz.fuzziness = Some(Fuzziness::parse(p)?),
                "prefix_length" => fz.prefix_length = p.int_value()?,
                "max_expansions" => fz.max_expansions = p.int_value()?,
                "operator" => operator = Operator::parse_name(&p.text()?)?,
                "minimum_should_match" => msm = p.text_or_null()?,
                "fuzzy_rewrite" => fz.fuzzy_rewrite = p.text_or_null()?,
                "fuzzy_transpositions" => fz.fuzzy_transpositions = p.boolean_value()?,
                "lenient" => lenient = p.boolean_value()?,
                "zero_terms_query" => zero = ZeroTerms::parse(p)?,
                "_name" => name = Some(p.text()?),
                "auto_generate_synonyms_phrase_query" => auto_syn = p.boolean_value()?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        if value.is_none() {
            return Err(p.parsing_error("No text specified for text query"));
        }
        let (field, query) = require_field_value("match", field, value)?;
        if fz.prefix_length < 0 {
            return Err(EsError::illegal_argument("[match] requires prefix length to be non-negative."));
        }
        if fz.max_expansions <= 0 {
            return Err(EsError::illegal_argument("[match] requires maxExpansions to be positive."));
        }
        let mut q = Match {
            field,
            query,
            operator,
            analyzer,
            fuzzy: fz,
            minimum_should_match: msm,
            lenient,
            zero_terms_query: zero,
            auto_generate_synonyms_phrase_query: auto_syn,
            common: Common::default(),
        };
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.clone());
        if self.operator != Operator::Or {
            m.insert("operator".into(), self.operator.name().into());
        }
        super::opt_str(&mut m, "analyzer", &self.analyzer);
        if let Some(f) = &self.fuzzy.fuzziness {
            m.insert("fuzziness".into(), f.as_string().into());
        }
        if self.fuzzy.prefix_length != 0 {
            m.insert("prefix_length".into(), self.fuzzy.prefix_length.into());
        }
        if self.fuzzy.max_expansions != 50 {
            m.insert("max_expansions".into(), self.fuzzy.max_expansions.into());
        }
        super::opt_str(&mut m, "minimum_should_match", &self.minimum_should_match);
        super::opt_str(&mut m, "fuzzy_rewrite", &self.fuzzy.fuzzy_rewrite);
        if !self.fuzzy.fuzzy_transpositions {
            m.insert("fuzzy_transpositions".into(), false.into());
        }
        if self.lenient {
            m.insert("lenient".into(), true.into());
        }
        if self.zero_terms_query != ZeroTerms::None {
            m.insert("zero_terms_query".into(), self.zero_terms_query.name().into());
        }
        if !self.auto_generate_synonyms_phrase_query {
            m.insert("auto_generate_synonyms_phrase_query".into(), false.into());
        }
        self.common.write_if_set(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// `match_phrase`.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchPhrase {
    pub field: String,
    pub query: Value,
    pub analyzer: Option<String>,
    pub slop: i32,
    pub zero_terms_query: ZeroTerms,
    pub common: Common,
}

impl MatchPhrase {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<MatchPhrase> {
        let mut boost = 1.0f32;
        let (mut analyzer, mut name) = (None, None);
        let (mut slop, mut zero) = (0, ZeroTerms::None);
        let (field, value) = parse_field_query(p, "match_phrase", |p, k| {
            match k {
                "analyzer" => analyzer = Some(p.text()?),
                "boost" => boost = p.float_value()?,
                "slop" => slop = p.int_value()?,
                "_name" => name = Some(p.text()?),
                "zero_terms_query" => zero = ZeroTerms::parse(p)?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        let (field, query) = require_field_value("match_phrase", field, value)?;
        if slop < 0 {
            return Err(EsError::illegal_argument("No negative slop allowed."));
        }
        let mut q = MatchPhrase { field, query, analyzer, slop, zero_terms_query: zero, common: Common::default() };
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.clone());
        super::opt_str(&mut m, "analyzer", &self.analyzer);
        if self.slop != 0 {
            m.insert("slop".into(), self.slop.into());
        }
        if self.zero_terms_query != ZeroTerms::None {
            m.insert("zero_terms_query".into(), self.zero_terms_query.name().into());
        }
        self.common.write_if_set(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// `match_phrase_prefix`.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchPhrasePrefix {
    pub field: String,
    pub query: Value,
    pub analyzer: Option<String>,
    pub slop: i32,
    pub max_expansions: i32,
    pub zero_terms_query: ZeroTerms,
    pub common: Common,
}

impl MatchPhrasePrefix {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<MatchPhrasePrefix> {
        let mut boost = 1.0f32;
        let (mut analyzer, mut name) = (None, None);
        let (mut slop, mut max_expansions, mut zero) = (0, 50, ZeroTerms::None);
        let (field, value) = parse_field_query(p, "match_phrase_prefix", |p, k| {
            match k {
                "analyzer" => analyzer = Some(p.text()?),
                "boost" => boost = p.float_value()?,
                "slop" => slop = p.int_value()?,
                "max_expansions" => max_expansions = p.int_value()?,
                "_name" => name = Some(p.text()?),
                "zero_terms_query" => zero = ZeroTerms::parse(p)?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        let (field, query) = require_field_value("match_phrase_prefix", field, value)?;
        if slop < 0 {
            return Err(EsError::illegal_argument("No negative slop allowed."));
        }
        if max_expansions < 0 {
            return Err(EsError::illegal_argument("No negative maxExpansions allowed."));
        }
        let mut q = MatchPhrasePrefix { field, query, analyzer, slop, max_expansions, zero_terms_query: ZeroTerms::None, common: Common::default() };
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.zero_terms_query = zero;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.clone());
        super::opt_str(&mut m, "analyzer", &self.analyzer);
        if self.slop != 0 {
            m.insert("slop".into(), self.slop.into());
        }
        if self.max_expansions != 50 {
            m.insert("max_expansions".into(), self.max_expansions.into());
        }
        if self.zero_terms_query != ZeroTerms::None {
            m.insert("zero_terms_query".into(), self.zero_terms_query.name().into());
        }
        self.common.write_if_set(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// `match_bool_prefix`.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchBoolPrefix {
    pub field: String,
    pub query: Value,
    pub analyzer: Option<String>,
    pub operator: Operator,
    pub minimum_should_match: Option<String>,
    pub fuzzy: FuzzyOpts,
    pub common: Common,
}

impl MatchBoolPrefix {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<MatchBoolPrefix> {
        let mut boost = 1.0f32;
        let (mut analyzer, mut msm, mut name) = (None, None, None);
        let mut operator = Operator::Or;
        let mut fz = FuzzyOpts::default();
        let (field, value) = parse_field_query(p, "match_bool_prefix", |p, k| {
            match k {
                "analyzer" => analyzer = Some(p.text()?),
                "operator" => operator = Operator::parse_name(&p.text()?)?,
                "minimum_should_match" => msm = p.text_or_null()?,
                "fuzziness" => fz.fuzziness = Some(Fuzziness::parse(p)?),
                "prefix_length" => fz.prefix_length = p.int_value()?,
                "max_expansions" => fz.max_expansions = p.int_value()?,
                "fuzzy_transpositions" => fz.fuzzy_transpositions = p.boolean_value()?,
                "fuzzy_rewrite" => fz.fuzzy_rewrite = p.text_or_null()?,
                "boost" => boost = p.float_value()?,
                "_name" => name = Some(p.text()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        let (field, query) = require_field_value("match_bool_prefix", field, value)?;
        let mut q = MatchBoolPrefix { field, query, analyzer, operator, minimum_should_match: msm, fuzzy: FuzzyOpts::default(), common: Common::default() };
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        if fz.prefix_length < 0 {
            return Err(EsError::illegal_argument("[match_bool_prefix] requires prefix length to be non-negative."));
        }
        if fz.max_expansions <= 0 {
            return Err(EsError::illegal_argument("[match_bool_prefix] requires maxExpansions to be positive."));
        }
        q.fuzzy = fz;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.clone());
        super::opt_str(&mut m, "analyzer", &self.analyzer);
        if self.operator != Operator::Or {
            m.insert("operator".into(), self.operator.name().into());
        }
        super::opt_str(&mut m, "minimum_should_match", &self.minimum_should_match);
        if let Some(f) = &self.fuzzy.fuzziness {
            m.insert("fuzziness".into(), f.as_string().into());
        }
        if self.fuzzy.prefix_length != 0 {
            m.insert("prefix_length".into(), self.fuzzy.prefix_length.into());
        }
        if self.fuzzy.max_expansions != 50 {
            m.insert("max_expansions".into(), self.fuzzy.max_expansions.into());
        }
        if !self.fuzzy.fuzzy_transpositions {
            m.insert("fuzzy_transpositions".into(), false.into());
        }
        super::opt_str(&mut m, "fuzzy_rewrite", &self.fuzzy.fuzzy_rewrite);
        self.common.write_if_set(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}

/// `MultiMatchQueryBuilder.Type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MultiMatchType {
    #[default]
    BestFields,
    MostFields,
    CrossFields,
    Phrase,
    PhrasePrefix,
    BoolPrefix,
}

impl MultiMatchType {
    pub fn name(self) -> &'static str {
        match self {
            MultiMatchType::BestFields => "best_fields",
            MultiMatchType::MostFields => "most_fields",
            MultiMatchType::CrossFields => "cross_fields",
            MultiMatchType::Phrase => "phrase",
            MultiMatchType::PhrasePrefix => "phrase_prefix",
            MultiMatchType::BoolPrefix => "bool_prefix",
        }
    }

    /// `Type.parse`: `boolean` is a deprecated name of `best_fields`.
    fn parse(p: &mut XParser<'_>, text: &str, query: &str) -> EsResult<MultiMatchType> {
        Ok(match text {
            "best_fields" => MultiMatchType::BestFields,
            "boolean" => {
                p.deprecated_name("boolean", "best_fields", None);
                MultiMatchType::BestFields
            }
            "most_fields" => MultiMatchType::MostFields,
            "cross_fields" => MultiMatchType::CrossFields,
            "phrase" => MultiMatchType::Phrase,
            "phrase_prefix" => MultiMatchType::PhrasePrefix,
            "bool_prefix" => MultiMatchType::BoolPrefix,
            _ => return Err(EsError::parse(format!("failed to parse [{query}] query type [{text}]. unknown type."))),
        })
    }
}

/// Fields with boosts, sorted by name (`TreeMap`), printed as `field^boost`.
pub type FieldBoosts = Vec<(String, f32)>;

fn put_field(fields: &mut FieldBoosts, name: String, boost: f32) {
    match fields.binary_search_by(|(f, _)| f.as_str().cmp(&name)) {
        Ok(i) => fields[i].1 = boost,
        Err(i) => fields.insert(i, (name, boost)),
    }
}

fn fields_value(fields: &FieldBoosts) -> Value {
    Value::Arr(fields.iter().map(|(f, b)| Value::Str(format!("{f}^{}", crate::json::write::java_float(*b)))).collect())
}

/// `QueryParserHelper.parseFieldsAndWeights`, duplicate handling included.
fn parse_fields_and_weights(fields: &[String]) -> EsResult<FieldBoosts> {
    let mut out: FieldBoosts = Vec::new();
    for f in fields {
        let (name, mut boost) = match f.find('^') {
            Some(i) => (f[..i].to_string(), java_parse_float(&f[i + 1..])?),
            None => (f.clone(), 1.0),
        };
        // Elasticsearch looks the duplicate up by the text with its boost suffix.
        if let Some((_, b)) = out.iter().find(|(n, _)| n == f) {
            boost *= *b;
        }
        put_field(&mut out, name, boost);
    }
    Ok(out)
}

/// `multi_match`.
#[derive(Clone, Debug, PartialEq)]
pub struct MultiMatch {
    pub query: Value,
    pub fields: FieldBoosts,
    pub ty: MultiMatchType,
    pub operator: Operator,
    pub analyzer: Option<String>,
    pub slop: i32,
    pub fuzzy: FuzzyOpts,
    pub minimum_should_match: Option<String>,
    pub tie_breaker: Option<f32>,
    pub lenient: Option<bool>,
    pub zero_terms_query: ZeroTerms,
    pub auto_generate_synonyms_phrase_query: bool,
    pub common: Common,
}

impl MultiMatch {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<MultiMatch> {
        let mut value = None;
        let mut fields: FieldBoosts = Vec::new();
        let mut ty = MultiMatchType::BestFields;
        let (mut analyzer, mut msm, mut name) = (None, None, None);
        let mut boost = 1.0f32;
        let mut slop = 0;
        let mut fz = FuzzyOpts::default();
        let mut operator = Operator::Or;
        let (mut tie_breaker, mut lenient) = (None, None);
        let (mut zero, mut auto_syn) = (ZeroTerms::None, true);
        let mut current = String::new();
        let field_and_boost = |p: &XParser<'_>, fields: &mut FieldBoosts| -> EsResult<()> {
            let text = p.text()?;
            match text.find('^') {
                Some(i) => put_field(fields, text[..i].to_string(), java_parse_float(&text[i + 1..])?),
                None => put_field(fields, text, 1.0),
            }
            Ok(())
        };
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                _ if current == "fields" => {
                    if t == Token::StartArray {
                        while let Some(t) = p.next_token() {
                            if t == Token::EndArray {
                                break;
                            }
                            field_and_boost(p, &mut fields)?;
                        }
                    } else if t.is_value() {
                        field_and_boost(p, &mut fields)?;
                    } else {
                        return Err(p.parsing_error(format!("[multi_match] query does not support [{current}]")));
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "query" => value = p.object_text()?,
                    "type" => {
                        let text = p.text()?;
                        ty = MultiMatchType::parse(p, &text, "multi_match")?;
                    }
                    "analyzer" => analyzer = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    "slop" => slop = p.int_value()?,
                    "fuzziness" => fz.fuzziness = Some(Fuzziness::parse(p)?),
                    "prefix_length" => fz.prefix_length = p.int_value()?,
                    "max_expansions" => fz.max_expansions = p.int_value()?,
                    "operator" => operator = Operator::parse_name(&p.text()?)?,
                    "minimum_should_match" => msm = p.text_or_null()?,
                    "fuzzy_rewrite" => fz.fuzzy_rewrite = p.text_or_null()?,
                    "tie_breaker" => tie_breaker = Some(p.float_value()?),
                    "lenient" => lenient = Some(p.boolean_value()?),
                    "zero_terms_query" => zero = ZeroTerms::parse(p)?,
                    "_name" => name = Some(p.text()?),
                    "auto_generate_synonyms_phrase_query" => auto_syn = p.boolean_value()?,
                    "fuzzy_transpositions" => fz.fuzzy_transpositions = p.boolean_value()?,
                    _ => return Err(p.parsing_error(format!("[multi_match] query does not support [{current}]"))),
                },
                t => return Err(p.parsing_error(format!("[multi_match] unknown token [{t}] after [{current}]"))),
            }
        }
        let Some(query) = value else { return Err(p.parsing_error("No text specified for multi_match query")) };
        if fz.fuzziness.is_some() && matches!(ty, MultiMatchType::CrossFields | MultiMatchType::Phrase | MultiMatchType::PhrasePrefix) {
            return Err(p.parsing_error(format!("Fuzziness not allowed for type [{}]", ty.name())));
        }
        if slop != 0 && ty == MultiMatchType::BoolPrefix {
            return Err(p.parsing_error(format!("[slop] not allowed for type [{}]", ty.name())));
        }
        if fields.iter().any(|(f, _)| f.is_empty()) {
            return Err(EsError::illegal_argument("supplied field is null or empty."));
        }
        let mut q = MultiMatch {
            query,
            fields: Vec::new(),
            ty,
            operator,
            analyzer,
            slop: 0,
            fuzzy: FuzzyOpts::default(),
            minimum_should_match: msm,
            tie_breaker,
            lenient: None,
            zero_terms_query: zero,
            auto_generate_synonyms_phrase_query: auto_syn,
            common: Common::default(),
        };
        for (f, b) in &fields {
            if *b < 0.0 {
                return Err(EsError::illegal_argument(format!(
                    "negative [boost] are not allowed in [{}], use a value between 0 and 1 to deboost",
                    Query::from(q.clone()).to_pretty_string()
                )));
            }
            let _ = f;
        }
        q.fields = fields;
        let fuzziness = fz.fuzziness.take();
        if slop < 0 {
            return Err(EsError::illegal_argument("No negative slop allowed."));
        }
        if fz.prefix_length < 0 {
            return Err(EsError::illegal_argument("No negative prefix length allowed."));
        }
        if fz.max_expansions <= 0 {
            return Err(EsError::illegal_argument("Max expansions must be strictly great than zero."));
        }
        q.slop = slop;
        q.fuzzy = fz;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        q.lenient = lenient;
        q.fuzzy.fuzziness = fuzziness;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.clone());
        m.insert("fields".into(), fields_value(&self.fields));
        if self.ty != MultiMatchType::BestFields {
            m.insert("type".into(), self.ty.name().into());
        }
        if self.operator != Operator::Or {
            m.insert("operator".into(), self.operator.name().into());
        }
        super::opt_str(&mut m, "analyzer", &self.analyzer);
        if self.slop != 0 {
            m.insert("slop".into(), self.slop.into());
        }
        if let Some(f) = &self.fuzzy.fuzziness {
            m.insert("fuzziness".into(), f.as_string().into());
        }
        if self.fuzzy.prefix_length != 0 {
            m.insert("prefix_length".into(), self.fuzzy.prefix_length.into());
        }
        if self.fuzzy.max_expansions != 50 {
            m.insert("max_expansions".into(), self.fuzzy.max_expansions.into());
        }
        super::opt_str(&mut m, "minimum_should_match", &self.minimum_should_match);
        super::opt_str(&mut m, "fuzzy_rewrite", &self.fuzzy.fuzzy_rewrite);
        if let Some(t) = self.tie_breaker {
            m.insert("tie_breaker".into(), float_value(t));
        }
        if let Some(l) = self.lenient {
            m.insert("lenient".into(), l.into());
        }
        if self.zero_terms_query != ZeroTerms::None {
            m.insert("zero_terms_query".into(), self.zero_terms_query.name().into());
        }
        if !self.auto_generate_synonyms_phrase_query {
            m.insert("auto_generate_synonyms_phrase_query".into(), false.into());
        }
        if !self.fuzzy.fuzzy_transpositions {
            m.insert("fuzzy_transpositions".into(), false.into());
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// `combined_fields`.
#[derive(Clone, Debug, PartialEq)]
pub struct CombinedFields {
    pub query: String,
    pub fields: FieldBoosts,
    pub operator: Operator,
    pub minimum_should_match: Option<String>,
    pub zero_terms_query: ZeroTerms,
    pub auto_generate_synonyms_phrase_query: bool,
    pub common: Common,
}

impl CombinedFields {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<CombinedFields> {
        const FIELDS: &[Field] = &[
            Field::new("query", Tokens::STRING),
            Field::new("fields", Tokens::STRING_ARRAY),
            Field::new("operator", Tokens::STRING),
            Field::new("minimum_should_match", Tokens::of(&[Token::Number, Token::String, Token::Null])),
            Field::new("auto_generate_synonyms_phrase_query", Tokens::BOOLEAN),
            Field::new("zero_terms_query", Tokens::STRING),
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
        ];
        let mut query = None;
        let mut q = CombinedFields {
            query: String::new(),
            fields: Vec::new(),
            operator: Operator::Or,
            minimum_should_match: None,
            zero_terms_query: ZeroTerms::None,
            auto_generate_synonyms_phrase_query: true,
            common: Common::default(),
        };
        p.object("combined_fields", FIELDS, |p, i| {
            match i {
                0 => query = Some(p.text()?),
                1 => {
                    let names = p.array_items(|p| p.text())?;
                    let fb = parse_fields_and_weights(&names)?;
                    for (_, b) in &fb {
                        if *b < 1.0 {
                            return Err(EsError::illegal_argument("[combined_fields] requires field boosts to be >= 1.0"));
                        }
                    }
                    for (f, b) in fb {
                        put_field(&mut q.fields, f, b);
                    }
                }
                2 => q.operator = Operator::parse_name(&p.text()?)?,
                3 => q.minimum_should_match = p.text_or_null()?,
                4 => q.auto_generate_synonyms_phrase_query = p.boolean_value()?,
                5 => {
                    let v = p.text()?;
                    q.zero_terms_query = if v.eq_ignore_ascii_case("none") {
                        ZeroTerms::None
                    } else if v.eq_ignore_ascii_case("all") {
                        ZeroTerms::All
                    } else {
                        return Err(EsError::illegal_argument(format!("Unsupported [zero_terms_query] value [{v}]")));
                    };
                }
                6 => {
                    let b = p.float_value()?;
                    q.common.boost = check_boost(b, || q.clone().into())?;
                }
                _ => q.common.name = Some(p.text()?),
            }
            Ok(())
        })?;
        q.query = query.ok_or_else(|| EsError::illegal_argument("Required [query]"))?;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.clone().into());
        m.insert("fields".into(), fields_value(&self.fields));
        if self.operator != Operator::Or {
            m.insert("operator".into(), self.operator.name().into());
        }
        super::opt_str(&mut m, "minimum_should_match", &self.minimum_should_match);
        if self.zero_terms_query != ZeroTerms::None {
            m.insert("zero_terms_query".into(), self.zero_terms_query.name().into());
        }
        if !self.auto_generate_synonyms_phrase_query {
            m.insert("auto_generate_synonyms_phrase_query".into(), false.into());
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// `query_string`.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryString {
    pub query: String,
    pub default_field: Option<String>,
    pub fields: FieldBoosts,
    pub ty: MultiMatchType,
    pub tie_breaker: Option<f32>,
    pub default_operator: Operator,
    pub analyzer: Option<String>,
    pub quote_analyzer: Option<String>,
    pub max_determinized_states: i32,
    pub allow_leading_wildcard: Option<bool>,
    pub enable_position_increments: bool,
    pub fuzziness: Fuzziness,
    pub fuzzy_prefix_length: i32,
    pub fuzzy_max_expansions: i32,
    pub fuzzy_rewrite: Option<String>,
    pub phrase_slop: i32,
    pub analyze_wildcard: Option<bool>,
    pub rewrite: Option<String>,
    pub minimum_should_match: Option<String>,
    pub quote_field_suffix: Option<String>,
    pub lenient: Option<bool>,
    pub time_zone: Option<String>,
    pub escape: bool,
    pub auto_generate_synonyms_phrase_query: bool,
    pub fuzzy_transpositions: bool,
    pub common: Common,
}

impl QueryString {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<QueryString> {
        let mut q = QueryString {
            query: String::new(),
            default_field: None,
            fields: Vec::new(),
            ty: MultiMatchType::BestFields,
            tie_breaker: None,
            default_operator: Operator::Or,
            analyzer: None,
            quote_analyzer: None,
            max_determinized_states: 10000,
            allow_leading_wildcard: None,
            enable_position_increments: true,
            fuzziness: Fuzziness::AUTO,
            fuzzy_prefix_length: 0,
            fuzzy_max_expansions: 50,
            fuzzy_rewrite: None,
            phrase_slop: 0,
            analyze_wildcard: None,
            rewrite: None,
            minimum_should_match: None,
            quote_field_suffix: None,
            lenient: None,
            time_zone: None,
            escape: false,
            auto_generate_synonyms_phrase_query: true,
            fuzzy_transpositions: true,
            common: Common::default(),
        };
        let mut query = None;
        let mut fields = None;
        let mut boost = 1.0f32;
        let mut time_zone = None;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartArray => {
                    if current == "fields" {
                        let mut names = Vec::new();
                        while p.next_token().is_some_and(|t| t != Token::EndArray) {
                            names.push(p.text()?);
                        }
                        fields = Some(parse_fields_and_weights(&names)?);
                    } else {
                        return Err(p.parsing_error(format!("[query_string] query does not support [{current}]")));
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "query" => query = Some(p.text()?),
                    "default_field" => q.default_field = Some(p.text()?),
                    "default_operator" => q.default_operator = Operator::parse_name(&p.text()?)?,
                    "analyzer" => q.analyzer = Some(p.text()?),
                    "quote_analyzer" => q.quote_analyzer = Some(p.text()?),
                    "allow_leading_wildcard" => q.allow_leading_wildcard = Some(p.boolean_value()?),
                    "max_determinized_states" => q.max_determinized_states = p.int_value()?,
                    "enable_position_increments" => q.enable_position_increments = p.boolean_value()?,
                    "escape" => q.escape = p.boolean_value()?,
                    "fuzzy_prefix_length" => q.fuzzy_prefix_length = p.int_value()?,
                    "fuzzy_max_expansions" => q.fuzzy_max_expansions = p.int_value()?,
                    "fuzzy_rewrite" => q.fuzzy_rewrite = p.text_or_null()?,
                    "phrase_slop" => q.phrase_slop = p.int_value()?,
                    "fuzziness" => q.fuzziness = Fuzziness::parse(p)?,
                    "boost" => boost = p.float_value()?,
                    "type" => {
                        let text = p.text()?;
                        q.ty = MultiMatchType::parse(p, &text, "multi_match")?;
                    }
                    "tie_breaker" => q.tie_breaker = Some(p.float_value()?),
                    "analyze_wildcard" => q.analyze_wildcard = Some(p.boolean_value()?),
                    "rewrite" => q.rewrite = p.text_or_null()?,
                    "minimum_should_match" => q.minimum_should_match = p.text_or_null()?,
                    "quote_field_suffix" => q.quote_field_suffix = p.text_or_null()?,
                    "lenient" => q.lenient = Some(p.boolean_value()?),
                    "time_zone" => time_zone = Some(p.text()?),
                    "_name" => q.common.name = Some(p.text()?),
                    "auto_generate_synonyms_phrase_query" => q.auto_generate_synonyms_phrase_query = p.boolean_value()?,
                    "fuzzy_transpositions" => q.fuzzy_transpositions = p.boolean_value()?,
                    _ => return Err(p.parsing_error(format!("[query_string] query does not support [{current}]"))),
                },
                t => return Err(p.parsing_error(format!("[query_string] unknown token [{t}] after [{current}]"))),
            }
        }
        let Some(query) = query else { return Err(p.parsing_error("[query_string] must be provided with a [query]")) };
        q.query = query;
        let name = q.common.name.take();
        if let Some(fields) = fields {
            for (_, b) in &fields {
                if *b < 0.0 {
                    let shown = QueryString { fields: Vec::new(), ..q.clone() };
                    return Err(EsError::illegal_argument(format!(
                        "negative [boost] are not allowed in [{}], use a value between 0 and 1 to deboost",
                        Query::from(shown).to_pretty_string()
                    )));
                }
            }
            for (f, b) in fields {
                put_field(&mut q.fields, f, b);
            }
        }
        if let Some(tz) = time_zone {
            // `ZoneId.of` outside the parser's try: a bad zone fails as Java's exception.
            q.time_zone = Some(java_zone_id(&tz).map_err(|(k, m)| {
                let e = wrap_java(k, &m);
                e.caused_by.map(|c| *c).unwrap_or_else(|| EsError::illegal_argument(m.clone()))
            })?);
        }
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.clone().into());
        super::opt_str(&mut m, "default_field", &self.default_field);
        m.insert("fields".into(), fields_value(&self.fields));
        if self.ty != MultiMatchType::BestFields {
            m.insert("type".into(), self.ty.name().into());
        }
        if let Some(t) = self.tie_breaker {
            m.insert("tie_breaker".into(), float_value(t));
        }
        if self.default_operator != Operator::Or {
            m.insert("default_operator".into(), self.default_operator.name().to_lowercase().into());
        }
        super::opt_str(&mut m, "analyzer", &self.analyzer);
        super::opt_str(&mut m, "quote_analyzer", &self.quote_analyzer);
        if self.max_determinized_states != 10000 {
            m.insert("max_determinized_states".into(), self.max_determinized_states.into());
        }
        if let Some(a) = self.allow_leading_wildcard {
            m.insert("allow_leading_wildcard".into(), a.into());
        }
        if !self.enable_position_increments {
            m.insert("enable_position_increments".into(), false.into());
        }
        if self.fuzziness != Fuzziness::AUTO {
            m.insert("fuzziness".into(), self.fuzziness.as_string().into());
        }
        if self.fuzzy_prefix_length != 0 {
            m.insert("fuzzy_prefix_length".into(), self.fuzzy_prefix_length.into());
        }
        if self.fuzzy_max_expansions != 50 {
            m.insert("fuzzy_max_expansions".into(), self.fuzzy_max_expansions.into());
        }
        super::opt_str(&mut m, "fuzzy_rewrite", &self.fuzzy_rewrite);
        if self.phrase_slop != 0 {
            m.insert("phrase_slop".into(), self.phrase_slop.into());
        }
        if let Some(a) = self.analyze_wildcard {
            m.insert("analyze_wildcard".into(), a.into());
        }
        super::opt_str(&mut m, "rewrite", &self.rewrite);
        super::opt_str(&mut m, "minimum_should_match", &self.minimum_should_match);
        super::opt_str(&mut m, "quote_field_suffix", &self.quote_field_suffix);
        if let Some(l) = self.lenient {
            m.insert("lenient".into(), l.into());
        }
        super::opt_str(&mut m, "time_zone", &self.time_zone);
        if self.escape {
            m.insert("escape".into(), true.into());
        }
        if !self.auto_generate_synonyms_phrase_query {
            m.insert("auto_generate_synonyms_phrase_query".into(), false.into());
        }
        if !self.fuzzy_transpositions {
            m.insert("fuzzy_transpositions".into(), false.into());
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// `SimpleQueryStringFlag` values.
pub mod sqs_flags {
    pub const ALL: i32 = -1;
    pub const NONE: i32 = 0;
    const NAMES: &[(&str, i32)] = &[
        ("AND", 1),
        ("NOT", 1 << 1),
        ("OR", 1 << 2),
        ("PREFIX", 1 << 3),
        ("PHRASE", 1 << 4),
        ("PRECEDENCE", 1 << 5),
        ("ESCAPE", 1 << 6),
        ("WHITESPACE", 1 << 7),
        ("FUZZY", 1 << 8),
        ("NEAR", 1 << 9),
        ("SLOP", 1 << 9),
    ];

    /// `SimpleQueryStringFlag.resolveFlags`.
    pub fn resolve(flags: &str) -> Result<i32, String> {
        if flags.is_empty() {
            return Ok(ALL);
        }
        let mut magic = NONE;
        for s in flags.split('|') {
            if s.is_empty() {
                continue;
            }
            match s.to_uppercase().as_str() {
                "NONE" => return Ok(0),
                "ALL" => return Ok(-1),
                u => match NAMES.iter().find(|(n, _)| *n == u) {
                    Some((_, v)) => magic |= v,
                    None => return Err(format!("Unknown simple_query_string flag [{s}]")),
                },
            }
        }
        Ok(magic)
    }
}

/// `simple_query_string`.
#[derive(Clone, Debug, PartialEq)]
pub struct SimpleQueryString {
    pub query: String,
    pub fields: FieldBoosts,
    pub analyzer: Option<String>,
    pub flags: i32,
    pub default_operator: Operator,
    pub lenient: Option<bool>,
    pub analyze_wildcard: bool,
    pub quote_field_suffix: Option<String>,
    pub minimum_should_match: Option<String>,
    pub auto_generate_synonyms_phrase_query: bool,
    pub fuzzy_prefix_length: i32,
    pub fuzzy_max_expansions: i32,
    pub fuzzy_transpositions: bool,
    pub ty: MultiMatchType,
    pub common: Common,
}

impl SimpleQueryString {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<SimpleQueryString> {
        let mut q = SimpleQueryString {
            query: String::new(),
            fields: Vec::new(),
            analyzer: None,
            flags: sqs_flags::ALL,
            default_operator: Operator::Or,
            lenient: None,
            analyze_wildcard: false,
            quote_field_suffix: None,
            minimum_should_match: None,
            auto_generate_synonyms_phrase_query: true,
            fuzzy_prefix_length: 0,
            fuzzy_max_expansions: 50,
            fuzzy_transpositions: true,
            ty: MultiMatchType::MostFields,
            common: Common::default(),
        };
        let (mut query, mut fields, mut boost, mut name) = (None, None, 1.0f32, None);
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartArray => {
                    if current == "fields" {
                        let mut names = Vec::new();
                        while p.next_token().is_some_and(|t| t != Token::EndArray) {
                            names.push(p.text()?);
                        }
                        fields = Some(parse_fields_and_weights(&names)?);
                    } else {
                        return Err(p.parsing_error(format!("[simple_query_string] query does not support [{current}]")));
                    }
                }
                t if t.is_value() => match current.as_str() {
                    "query" => query = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    "analyzer" => q.analyzer = Some(p.text()?),
                    "default_operator" => q.default_operator = Operator::parse_name(&p.text()?)?,
                    "flags" => {
                        q.flags = if t != Token::Number {
                            sqs_flags::resolve(&p.text()?).map_err(EsError::illegal_argument)?
                        } else {
                            let f = p.int_value()?;
                            if f < 0 { sqs_flags::ALL } else { f }
                        };
                    }
                    "lenient" => q.lenient = Some(p.boolean_value()?),
                    "analyze_wildcard" => q.analyze_wildcard = p.boolean_value()?,
                    "_name" => name = Some(p.text()?),
                    "minimum_should_match" => q.minimum_should_match = p.text_or_null()?,
                    "quote_field_suffix" => q.quote_field_suffix = p.text_or_null()?,
                    "auto_generate_synonyms_phrase_query" => q.auto_generate_synonyms_phrase_query = p.boolean_value()?,
                    "fuzzy_prefix_length" => q.fuzzy_prefix_length = p.int_value()?,
                    "fuzzy_max_expansions" => q.fuzzy_max_expansions = p.int_value()?,
                    "fuzzy_transpositions" => q.fuzzy_transpositions = p.boolean_value()?,
                    "type" => {
                        let text = p.text()?;
                        q.ty = MultiMatchType::parse(p, &text, "multi_match")?;
                    }
                    _ => {
                        return Err(p.parsing_error(format!(
                            "[simple_query_string] unsupported field [{}]",
                            p.current_name().unwrap_or("null")
                        )));
                    }
                },
                t => return Err(p.parsing_error(format!("[simple_query_string] unknown token [{t}] after [{current}]"))),
            }
        }
        let Some(query) = query else { return Err(p.parsing_error("[simple_query_string] query text missing")) };
        q.query = query;
        if let Some(fields) = fields {
            for (f, b) in &fields {
                if f.is_empty() {
                    return Err(EsError::illegal_argument("supplied field is null or empty"));
                }
                if *b < 0.0 {
                    let shown = SimpleQueryString { query: q.query.clone(), ..SimpleQueryString { fields: Vec::new(), ..q.clone() } };
                    return Err(EsError::illegal_argument(format!(
                        "negative [boost] are not allowed in [{}], use a value between 0 and 1 to deboost",
                        Query::from(shown).to_pretty_string()
                    )));
                }
            }
            for (f, b) in fields {
                put_field(&mut q.fields, f, b);
            }
        }
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.clone().into());
        if !self.fields.is_empty() {
            m.insert("fields".into(), fields_value(&self.fields));
        }
        super::opt_str(&mut m, "analyzer", &self.analyzer);
        if self.flags != sqs_flags::ALL {
            m.insert("flags".into(), self.flags.into());
        }
        if self.default_operator != Operator::Or {
            m.insert("default_operator".into(), self.default_operator.name().to_lowercase().into());
        }
        if let Some(l) = self.lenient {
            m.insert("lenient".into(), l.into());
        }
        if self.analyze_wildcard {
            m.insert("analyze_wildcard".into(), true.into());
        }
        super::opt_str(&mut m, "quote_field_suffix", &self.quote_field_suffix);
        super::opt_str(&mut m, "minimum_should_match", &self.minimum_should_match);
        if !self.auto_generate_synonyms_phrase_query {
            m.insert("auto_generate_synonyms_phrase_query".into(), false.into());
        }
        if self.fuzzy_prefix_length != 0 {
            m.insert("fuzzy_prefix_length".into(), self.fuzzy_prefix_length.into());
        }
        if self.fuzzy_max_expansions != 50 {
            m.insert("fuzzy_max_expansions".into(), self.fuzzy_max_expansions.into());
        }
        if !self.fuzzy_transpositions {
            m.insert("fuzzy_transpositions".into(), false.into());
        }
        if self.ty != MultiMatchType::MostFields {
            m.insert("type".into(), self.ty.name().into());
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// An interval rule (`IntervalsSourceProvider`).
#[derive(Clone, Debug, PartialEq)]
pub enum IntervalsSource {
    Match { query: String, max_gaps: i32, ordered: bool, analyzer: Option<String>, filter: Option<Box<IntervalFilter>>, use_field: Option<String> },
    AnyOf { intervals: Vec<IntervalsSource>, filter: Option<Box<IntervalFilter>> },
    AllOf { intervals: Vec<IntervalsSource>, ordered: bool, max_gaps: i32, filter: Option<Box<IntervalFilter>> },
    Prefix { prefix: String, analyzer: Option<String>, use_field: Option<String> },
    Wildcard { pattern: String, analyzer: Option<String>, use_field: Option<String> },
    Regexp { pattern: String, analyzer: Option<String>, use_field: Option<String> },
    Fuzzy { term: String, prefix_length: i32, transpositions: bool, fuzziness: Fuzziness, analyzer: Option<String>, use_field: Option<String> },
    Range { lower: String, upper: String, include_lower: bool, include_upper: bool, analyzer: Option<String>, use_field: Option<String> },
}

/// A filter on an interval rule: another rule with a relation, or a script.
#[derive(Clone, Debug, PartialEq)]
pub enum IntervalFilter {
    Rule { ty: String, source: IntervalsSource },
    Script(Script),
}

/// The filter relations an interval filter may use.
pub const FILTER_TYPES: &[&str] =
    &["containing", "contained_by", "not_containing", "not_contained_by", "overlapping", "not_overlapping", "before", "after"];

impl IntervalFilter {
    fn parse(p: &mut XParser<'_>) -> EsResult<IntervalFilter> {
        let expect = |p: &XParser<'_>, what: &str| p.parsing_error(format!("Expected [{what}] but got [{}]", p.current_token().map(Token::name).unwrap_or("null")));
        if p.next_token() != Some(Token::FieldName) {
            return Err(expect(p, "FIELD_NAME"));
        }
        let ty = p.current_name().unwrap_or("").to_string();
        if ty == "script" {
            let s = Script::parse(p)?;
            if p.next_token() != Some(Token::EndObject) {
                return Err(expect(p, "END_OBJECT"));
            }
            return Ok(IntervalFilter::Script(s));
        }
        if p.next_token() != Some(Token::StartObject) {
            return Err(expect(p, "START_OBJECT"));
        }
        if p.next_token() != Some(Token::FieldName) {
            return Err(expect(p, "FIELD_NAME"));
        }
        let source = IntervalsSource::parse(p)?;
        if p.next_token() != Some(Token::EndObject) {
            return Err(expect(p, "END_OBJECT"));
        }
        if p.next_token() != Some(Token::EndObject) {
            return Err(expect(p, "END_OBJECT"));
        }
        // Unknown filter types fail when the query runs.
        Ok(IntervalFilter::Rule { ty: ty.to_lowercase(), source })
    }

    fn to_value(&self) -> Value {
        match self {
            IntervalFilter::Rule { ty, source } => {
                let mut inner = Map::new();
                source.write(&mut inner);
                obj! { ty.clone() => Value::Obj(inner) }
            }
            IntervalFilter::Script(s) => obj! { "script" => s.to_value() },
        }
    }
}

impl IntervalsSource {
    /// `IntervalsSourceProvider.fromXContent`, on the rule's name.
    fn parse(p: &mut XParser<'_>) -> EsResult<IntervalsSource> {
        crate::stack::maybe_grow(|| Self::parse_level(p))
    }

    fn parse_level(p: &mut XParser<'_>) -> EsResult<IntervalsSource> {
        let name = p.current_name().unwrap_or("").to_string();
        let mut vals: Vec<Option<Value>> = vec![None; 6];
        let mut filter: Option<Box<IntervalFilter>> = None;
        let mut intervals: Option<Vec<IntervalsSource>> = None;
        let mut fuzziness: Option<Fuzziness> = None;
        let fields: &[Field] = match name.as_str() {
            "match" => &[
                Field::new("query", Tokens::STRING),
                Field::new("max_gaps", Tokens::INT),
                Field::new("ordered", Tokens::BOOLEAN),
                Field::new("analyzer", Tokens::STRING),
                Field::new("filter", Tokens::OBJECT),
                Field::new("use_field", Tokens::STRING),
            ],
            "any_of" => &[Field::new("intervals", Tokens::OBJECT_ARRAY), Field::new("filter", Tokens::OBJECT)],
            "all_of" => &[
                Field::new("ordered", Tokens::BOOLEAN),
                Field::new("intervals", Tokens::OBJECT_ARRAY),
                Field::new("max_gaps", Tokens::INT),
                Field::new("filter", Tokens::OBJECT),
            ],
            "prefix" => &[Field::new("prefix", Tokens::STRING), Field::new("analyzer", Tokens::STRING), Field::new("use_field", Tokens::STRING)],
            "wildcard" | "regexp" => &[Field::new("pattern", Tokens::STRING), Field::new("analyzer", Tokens::STRING), Field::new("use_field", Tokens::STRING)],
            "fuzzy" => &[
                Field::new("term", Tokens::STRING),
                Field::new("prefix_length", Tokens::INT),
                Field::new("transpositions", Tokens::BOOLEAN),
                Field::new("fuzziness", Tokens::VALUE),
                Field::new("analyzer", Tokens::STRING),
                Field::new("use_field", Tokens::STRING),
            ],
            "range" => &[
                Field::new("gte", Tokens::STRING),
                Field::new("gt", Tokens::STRING),
                Field::new("lte", Tokens::STRING),
                Field::new("lt", Tokens::STRING),
                Field::new("analyzer", Tokens::STRING),
                Field::new("use_field", Tokens::STRING),
            ],
            _ => {
                return Err(p.parsing_error(format!(
                    "Unknown interval type [{name}], expecting one of [match, any_of, all_of, prefix, wildcard, regexp, range]"
                )));
            }
        };
        p.object(&name, fields, |p, i| {
            let fname = fields[i].name;
            match fname {
                "filter" => filter = Some(Box::new(IntervalFilter::parse(p)?)),
                "intervals" => {
                    intervals = Some(p.array_items(|p| {
                        let expect = |p: &XParser<'_>, what: &str| {
                            p.parsing_error(format!("Expected [{what}] but got [{}]", p.current_token().map(Token::name).unwrap_or("null")))
                        };
                        if p.next_token() != Some(Token::FieldName) {
                            return Err(expect(p, "FIELD_NAME"));
                        }
                        let s = IntervalsSource::parse(p)?;
                        if p.next_token() != Some(Token::EndObject) {
                            return Err(expect(p, "END_OBJECT"));
                        }
                        Ok(s)
                    })?)
                }
                "fuzziness" => fuzziness = Some(Fuzziness::parse(p)?),
                "max_gaps" | "prefix_length" => vals[i] = Some(Value::from(p.int_value()?)),
                "ordered" | "transpositions" => vals[i] = Some(Value::Bool(p.boolean_value()?)),
                _ => vals[i] = Some(Value::Str(p.text()?)),
            }
            Ok(())
        })?;
        let s = |i: usize| vals[i].as_ref().and_then(Value::as_str).map(str::to_string);
        let int = |i: usize, d: i32| vals[i].as_ref().and_then(Value::as_i64).map(|v| v as i32).unwrap_or(d);
        let flag = |i: usize, d: bool| vals[i].as_ref().and_then(Value::as_bool).unwrap_or(d);
        let required = |v: Option<String>, n: &str| v.ok_or_else(|| EsError::illegal_argument(format!("Required [{n}]")));
        Ok(match name.as_str() {
            "match" => IntervalsSource::Match {
                query: required(s(0), "query")?,
                max_gaps: int(1, -1),
                ordered: flag(2, false),
                analyzer: s(3),
                filter,
                use_field: s(5),
            },
            "any_of" => IntervalsSource::AnyOf { intervals: intervals.ok_or_else(|| EsError::illegal_argument("Required [intervals]"))?, filter },
            "all_of" => IntervalsSource::AllOf {
                ordered: flag(0, false),
                intervals: intervals.ok_or_else(|| EsError::illegal_argument("Required [intervals]"))?,
                max_gaps: int(2, -1),
                filter,
            },
            "prefix" => IntervalsSource::Prefix { prefix: required(s(0), "prefix")?, analyzer: s(1), use_field: s(2) },
            "wildcard" => IntervalsSource::Wildcard { pattern: required(s(0), "pattern")?, analyzer: s(1), use_field: s(2) },
            "regexp" => IntervalsSource::Regexp { pattern: required(s(0), "pattern")?, analyzer: s(1), use_field: s(2) },
            "fuzzy" => IntervalsSource::Fuzzy {
                term: required(s(0), "term")?,
                prefix_length: int(1, 0),
                transpositions: flag(2, true),
                fuzziness: fuzziness.unwrap_or(Fuzziness::AUTO),
                analyzer: s(4),
                use_field: s(5),
            },
            _ => {
                let (gte, gt, lte, lt) = (s(0), s(1), s(2), s(3));
                let failed = |m: &str| {
                    EsError::x_content_parse_bare("Failed to build [range] after last required field arrived").caused_by(EsError::illegal_argument(m))
                };
                if gte.is_some() == gt.is_some() {
                    return Err(failed("Either [gte] or [gt], one of them must be provided"));
                }
                if lte.is_some() == lt.is_some() {
                    return Err(failed("Either [lte] or [lt], one of them must be provided"));
                }
                IntervalsSource::Range {
                    include_lower: gte.is_some(),
                    lower: gte.or(gt).unwrap_or_default(),
                    include_upper: lte.is_some(),
                    upper: lte.or(lt).unwrap_or_default(),
                    analyzer: s(4),
                    use_field: s(5),
                }
            }
        })
    }

    /// `toXContent`: `"<rule>": {...}` into the enclosing object.
    fn write(&self, m: &mut Map) {
        crate::stack::maybe_grow(|| self.write_level(m))
    }

    fn write_level(&self, m: &mut Map) {
        let mut b = Map::new();
        let name = match self {
            IntervalsSource::Match { query, max_gaps, ordered, analyzer, filter, use_field } => {
                b.insert("query".into(), query.clone().into());
                b.insert("max_gaps".into(), (*max_gaps).into());
                b.insert("ordered".into(), (*ordered).into());
                super::opt_str(&mut b, "analyzer", analyzer);
                if let Some(f) = filter {
                    b.insert("filter".into(), f.to_value());
                }
                super::opt_str(&mut b, "use_field", use_field);
                "match"
            }
            IntervalsSource::AnyOf { intervals, filter } => {
                b.insert("intervals".into(), Value::Arr(intervals.iter().map(|s| { let mut x = Map::new(); s.write(&mut x); Value::Obj(x) }).collect()));
                if let Some(f) = filter {
                    b.insert("filter".into(), f.to_value());
                }
                "any_of"
            }
            IntervalsSource::AllOf { intervals, ordered, max_gaps, filter } => {
                b.insert("ordered".into(), (*ordered).into());
                b.insert("max_gaps".into(), (*max_gaps).into());
                b.insert("intervals".into(), Value::Arr(intervals.iter().map(|s| { let mut x = Map::new(); s.write(&mut x); Value::Obj(x) }).collect()));
                if let Some(f) = filter {
                    b.insert("filter".into(), f.to_value());
                }
                "all_of"
            }
            IntervalsSource::Prefix { prefix, analyzer, use_field } => {
                b.insert("prefix".into(), prefix.clone().into());
                super::opt_str(&mut b, "analyzer", analyzer);
                super::opt_str(&mut b, "use_field", use_field);
                "prefix"
            }
            IntervalsSource::Wildcard { pattern, analyzer, use_field } | IntervalsSource::Regexp { pattern, analyzer, use_field } => {
                b.insert("pattern".into(), pattern.clone().into());
                super::opt_str(&mut b, "analyzer", analyzer);
                super::opt_str(&mut b, "use_field", use_field);
                if matches!(self, IntervalsSource::Wildcard { .. }) { "wildcard" } else { "regexp" }
            }
            IntervalsSource::Fuzzy { term, prefix_length, transpositions, fuzziness, analyzer, use_field } => {
                b.insert("term".into(), term.clone().into());
                b.insert("prefix_length".into(), (*prefix_length).into());
                b.insert("transpositions".into(), (*transpositions).into());
                b.insert("fuzziness".into(), fuzziness.as_string().into());
                super::opt_str(&mut b, "analyzer", analyzer);
                super::opt_str(&mut b, "use_field", use_field);
                "fuzzy"
            }
            IntervalsSource::Range { lower, upper, include_lower, include_upper, analyzer, use_field } => {
                b.insert(if *include_lower { "gte" } else { "gt" }.into(), lower.clone().into());
                b.insert(if *include_upper { "lte" } else { "lt" }.into(), upper.clone().into());
                super::opt_str(&mut b, "analyzer", analyzer);
                super::opt_str(&mut b, "use_field", use_field);
                "range"
            }
        };
        m.insert(name.into(), Value::Obj(b));
    }
}

/// `intervals`.
#[derive(Clone, Debug, PartialEq)]
pub struct Intervals {
    pub field: String,
    pub source: IntervalsSource,
    pub common: Common,
}

impl Intervals {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Intervals> {
        let expect = |p: &XParser<'_>, what: &str| p.parsing_error(format!("Expected [{what}] but got [{}]", p.current_token().map(Token::name).unwrap_or("null")));
        if p.next_token() != Some(Token::FieldName) {
            return Err(expect(p, "FIELD_NAME"));
        }
        let field = p.current_name().unwrap_or("").to_string();
        if p.next_token() != Some(Token::StartObject) {
            return Err(expect(p, "START_OBJECT"));
        }
        let (mut name, mut boost) = (None, 1.0f32);
        let mut source = None;
        let mut provider: Option<String> = None;
        while p.next_token() != Some(Token::EndObject) {
            if p.current_token() != Some(Token::FieldName) {
                return Err(expect(p, "FIELD_NAME"));
            }
            match p.current_name().unwrap_or("") {
                "_name" => {
                    p.next_token();
                    name = Some(p.text()?);
                }
                "boost" => {
                    p.next_token();
                    boost = p.float_value()?;
                }
                other => {
                    if let Some(pn) = &provider {
                        return Err(p.parsing_error(format!("Only one interval rule can be specified, found [{pn}] and [{other}]")));
                    }
                    provider = Some(other.to_string());
                    source = Some(IntervalsSource::parse(p)?);
                }
            }
        }
        if p.next_token() != Some(Token::EndObject) {
            return Err(expect(p, "END_OBJECT"));
        }
        let Some(source) = source else { return Err(p.parsing_error("Missing intervals from interval query definition")) };
        let mut q = Intervals { field, source, common: Common::default() };
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        self.source.write(&mut m);
        self.common.write_if_set(&mut m);
        obj! { self.field.clone() => Value::Obj(m) }
    }
}
