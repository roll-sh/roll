//! Joining queries: `nested`, `has_child`, `has_parent` and `parent_id`, with `inner_hits`.

use super::xparser::{Field, Tokens};
use super::{Common, Token, XParser, check_boost, parse_inner};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};

/// `inner_hits` (`InnerHitBuilder`). The fetch options (`_source`, `sort`, `highlight`, …) are
/// kept as given until the search phase's builders for them parse them.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct InnerHits {
    pub name: Option<String>,
    pub ignore_unmapped: bool,
    pub from: Option<i32>,
    pub size: Option<i32>,
    pub explain: bool,
    pub version: bool,
    pub seq_no_primary_term: bool,
    pub track_scores: bool,
    pub source: Option<Value>,
    pub stored_fields: Option<Value>,
    pub docvalue_fields: Option<Value>,
    pub fields: Option<Value>,
    pub script_fields: Option<Value>,
    pub sort: Option<Value>,
    pub highlight: Option<Value>,
    pub collapse: Option<Value>,
}

impl InnerHits {
    /// `InnerHitBuilder.fromXContent`.
    pub fn parse(p: &mut XParser<'_>) -> EsResult<InnerHits> {
        const FIELDS: &[Field] = &[
            Field::new("name", Tokens::STRING),
            Field::new("ignore_unmapped", Tokens::BOOLEAN),
            Field::new("from", Tokens::INT),
            Field::new("size", Tokens::INT),
            Field::new("explain", Tokens::BOOLEAN),
            Field::new("version", Tokens::BOOLEAN),
            Field::new("seq_no_primary_term", Tokens::BOOLEAN),
            Field::new("track_scores", Tokens::BOOLEAN),
            Field::new("stored_fields", Tokens::STRING_ARRAY),
            Field::new("docvalue_fields", Tokens::OBJECT_ARRAY_OR_STRING),
            Field::new("fields", Tokens::OBJECT_ARRAY_OR_STRING),
            Field::new("script_fields", Tokens::OBJECT),
            Field::new("_source", Tokens::of(&[Token::Boolean, Token::StartObject, Token::StartArray, Token::String])),
            Field::new("highlight", Tokens::OBJECT),
            Field::new("sort", Tokens::OBJECT_ARRAY_OR_STRING),
            Field::new("collapse", Tokens::OBJECT),
        ];
        let mut ih = InnerHits::default();
        p.object("inner_hits", FIELDS, |p, i| {
            match i {
                0 => ih.name = Some(p.text()?),
                1 => ih.ignore_unmapped = p.boolean_value()?,
                2 => ih.from = Some(p.int_value()?),
                3 => ih.size = Some(p.int_value()?),
                4 => ih.explain = p.boolean_value()?,
                5 => ih.version = p.boolean_value()?,
                6 => ih.seq_no_primary_term = p.boolean_value()?,
                7 => ih.track_scores = p.boolean_value()?,
                8 => ih.stored_fields = Some(p.take_value()),
                9 => ih.docvalue_fields = Some(p.take_value()),
                10 => ih.fields = Some(p.take_value()),
                11 => ih.script_fields = Some(p.take_value()),
                12 => ih.source = Some(p.take_value()),
                13 => ih.highlight = Some(p.take_value()),
                14 => ih.sort = Some(p.take_value()),
                _ => ih.collapse = Some(p.take_value()),
            }
            Ok(())
        })?;
        Ok(ih)
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        super::opt_str(&mut m, "name", &self.name);
        if self.ignore_unmapped {
            m.insert("ignore_unmapped".into(), true.into());
        }
        if let Some(f) = self.from.filter(|f| *f != 0) {
            m.insert("from".into(), f.into());
        }
        if let Some(s) = self.size.filter(|s| *s != 3) {
            m.insert("size".into(), s.into());
        }
        for (k, v) in [("version", self.version), ("seq_no_primary_term", self.seq_no_primary_term), ("explain", self.explain), ("track_scores", self.track_scores)] {
            if v {
                m.insert(k.into(), true.into());
            }
        }
        for (k, v) in [
            ("_source", &self.source),
            ("stored_fields", &self.stored_fields),
            ("docvalue_fields", &self.docvalue_fields),
            ("fields", &self.fields),
            ("script_fields", &self.script_fields),
            ("sort", &self.sort),
            ("highlight", &self.highlight),
            ("collapse", &self.collapse),
        ] {
            if let Some(v) = v {
                m.insert(k.into(), v.clone());
            }
        }
        Value::Obj(m)
    }
}

/// The score mode of `nested` and `has_child` (`ScoreMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinScoreMode {
    None,
    Avg,
    Max,
    Min,
    Total,
}

impl JoinScoreMode {
    /// `NestedQueryBuilder.parseScoreMode`.
    pub fn parse(s: &str) -> EsResult<JoinScoreMode> {
        Ok(match s {
            "none" => JoinScoreMode::None,
            "min" => JoinScoreMode::Min,
            "max" => JoinScoreMode::Max,
            "avg" => JoinScoreMode::Avg,
            "sum" => JoinScoreMode::Total,
            _ => return Err(EsError::illegal_argument(format!("No score mode for child query [{s}] found"))),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            JoinScoreMode::None => "none",
            JoinScoreMode::Avg => "avg",
            JoinScoreMode::Max => "max",
            JoinScoreMode::Min => "min",
            JoinScoreMode::Total => "sum",
        }
    }
}

/// `nested`.
#[derive(Clone, Debug, PartialEq)]
pub struct Nested {
    pub path: String,
    pub query: super::Query,
    pub score_mode: JoinScoreMode,
    pub ignore_unmapped: bool,
    pub inner_hits: Option<InnerHits>,
    pub common: Common,
}

impl Nested {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Nested> {
        let (mut query, mut path, mut inner_hits) = (None, None, None);
        let (mut boost, mut name, mut ignore_unmapped) = (1.0f32, None, false);
        let mut score_mode = JoinScoreMode::Avg;
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => match current.as_str() {
                    "query" => query = Some(parse_inner(p)?),
                    "inner_hits" => inner_hits = Some(InnerHits::parse(p)?),
                    _ => return Err(p.parsing_error(format!("[nested] query does not support [{current}]"))),
                },
                t if t.is_value() => match current.as_str() {
                    "path" => path = Some(p.text()?),
                    "boost" => boost = p.float_value()?,
                    "ignore_unmapped" => ignore_unmapped = p.boolean_value()?,
                    "score_mode" => score_mode = JoinScoreMode::parse(&p.text()?)?,
                    "_name" => name = Some(p.text()?),
                    _ => return Err(p.parsing_error(format!("[nested] query does not support [{current}]"))),
                },
                _ => {}
            }
        }
        let path = path.ok_or_else(|| EsError::illegal_argument("[nested] requires 'path' field"))?;
        let query = query.ok_or_else(|| EsError::illegal_argument("[nested] requires 'query' field"))?;
        let mut q = Nested { path, query, score_mode, ignore_unmapped, inner_hits, common: Common::default() };
        if let Some(ih) = &mut q.inner_hits {
            ih.ignore_unmapped = ignore_unmapped;
        }
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.to_value());
        m.insert("path".into(), self.path.clone().into());
        m.insert("ignore_unmapped".into(), self.ignore_unmapped.into());
        m.insert("score_mode".into(), self.score_mode.name().into());
        self.common.write_always(&mut m);
        if let Some(ih) = &self.inner_hits {
            m.insert("inner_hits".into(), ih.to_value());
        }
        Value::Obj(m)
    }
}

/// `has_child`.
#[derive(Clone, Debug, PartialEq)]
pub struct HasChild {
    pub ty: String,
    pub query: super::Query,
    pub score_mode: JoinScoreMode,
    pub min_children: i32,
    pub max_children: i32,
    pub ignore_unmapped: bool,
    pub inner_hits: Option<InnerHits>,
    pub common: Common,
}

impl HasChild {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<HasChild> {
        let (mut query, mut ty, mut inner_hits) = (None, None, None);
        let (mut boost, mut name, mut ignore_unmapped) = (1.0f32, None, false);
        let mut score_mode = JoinScoreMode::None;
        let (mut min_children, mut max_children) = (1, i32::MAX);
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => match current.as_str() {
                    "query" => query = Some(parse_inner(p)?),
                    "inner_hits" => inner_hits = Some(InnerHits::parse(p)?),
                    _ => return Err(p.parsing_error(format!("[has_child] query does not support [{current}]"))),
                },
                t if t.is_value() => match current.as_str() {
                    "type" => ty = Some(p.text()?),
                    "score_mode" => score_mode = JoinScoreMode::parse(&p.text()?)?,
                    "boost" => boost = p.float_value()?,
                    "min_children" => min_children = p.int_value()?,
                    "max_children" => max_children = p.int_value()?,
                    "ignore_unmapped" => ignore_unmapped = p.boolean_value()?,
                    "_name" => name = Some(p.text()?),
                    _ => return Err(p.parsing_error(format!("[has_child] query does not support [{current}]"))),
                },
                _ => {}
            }
        }
        let ty = ty.ok_or_else(|| EsError::illegal_argument("[has_child] requires 'type' field"))?;
        let query = query.ok_or_else(|| EsError::illegal_argument("[has_child] requires 'query' field"))?;
        if min_children < 0 {
            return Err(EsError::illegal_argument("[has_child] requires positive 'min_children' field"));
        }
        if max_children < 0 {
            return Err(EsError::illegal_argument("[has_child] requires positive 'max_children' field"));
        }
        if max_children < min_children {
            return Err(EsError::illegal_argument("[has_child] 'max_children' is less than 'min_children'"));
        }
        let mut q = HasChild { ty, query, score_mode, min_children, max_children, ignore_unmapped: false, inner_hits: None, common: Common::default() };
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.ignore_unmapped = ignore_unmapped;
        q.inner_hits = inner_hits.map(|mut ih| {
            ih.ignore_unmapped = ignore_unmapped;
            ih
        });
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.to_value());
        m.insert("type".into(), self.ty.clone().into());
        if self.score_mode != JoinScoreMode::None {
            m.insert("score_mode".into(), self.score_mode.name().into());
        }
        if self.min_children != 1 {
            m.insert("min_children".into(), self.min_children.into());
        }
        if self.max_children != i32::MAX {
            m.insert("max_children".into(), self.max_children.into());
        }
        if self.ignore_unmapped {
            m.insert("ignore_unmapped".into(), true.into());
        }
        self.common.write_if_set(&mut m);
        if let Some(ih) = &self.inner_hits {
            m.insert("inner_hits".into(), ih.to_value());
        }
        Value::Obj(m)
    }
}

/// `has_parent`.
#[derive(Clone, Debug, PartialEq)]
pub struct HasParent {
    pub parent_type: String,
    pub query: super::Query,
    pub score: bool,
    pub ignore_unmapped: bool,
    pub inner_hits: Option<InnerHits>,
    pub common: Common,
}

impl HasParent {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<HasParent> {
        let (mut query, mut ty, mut inner_hits) = (None, None, None);
        let (mut boost, mut name, mut ignore_unmapped, mut score) = (1.0f32, None, false, false);
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => match current.as_str() {
                    "query" => query = Some(parse_inner(p)?),
                    "inner_hits" => inner_hits = Some(InnerHits::parse(p)?),
                    _ => return Err(p.parsing_error(format!("[has_parent] query does not support [{current}]"))),
                },
                t if t.is_value() => match current.as_str() {
                    "parent_type" => ty = Some(p.text()?),
                    "score" => score = p.boolean_value()?,
                    "ignore_unmapped" => ignore_unmapped = p.boolean_value()?,
                    "boost" => boost = p.float_value()?,
                    "_name" => name = Some(p.text()?),
                    _ => return Err(p.parsing_error(format!("[has_parent] query does not support [{current}]"))),
                },
                _ => {}
            }
        }
        let parent_type = ty.ok_or_else(|| EsError::illegal_argument("[has_parent] requires 'parent_type' field"))?;
        let query = query.ok_or_else(|| EsError::illegal_argument("[has_parent] requires 'query' field"))?;
        let mut q = HasParent { parent_type, query, score, ignore_unmapped, inner_hits: None, common: Common::default() };
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.inner_hits = inner_hits.map(|mut ih| {
            ih.ignore_unmapped = ignore_unmapped;
            ih
        });
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.to_value());
        m.insert("parent_type".into(), self.parent_type.clone().into());
        if self.score {
            m.insert("score".into(), true.into());
        }
        if self.ignore_unmapped {
            m.insert("ignore_unmapped".into(), true.into());
        }
        self.common.write_if_set(&mut m);
        if let Some(ih) = &self.inner_hits {
            m.insert("inner_hits".into(), ih.to_value());
        }
        Value::Obj(m)
    }
}

/// `parent_id`.
#[derive(Clone, Debug, PartialEq)]
pub struct ParentId {
    pub ty: String,
    pub id: String,
    pub ignore_unmapped: bool,
    pub common: Common,
}

impl ParentId {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<ParentId> {
        let (mut ty, mut id) = (None, None);
        let (mut boost, mut name, mut ignore_unmapped) = (1.0f32, None, false);
        let mut current = String::new();
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                t if t.is_value() => match current.as_str() {
                    "type" => ty = Some(p.text()?),
                    "id" => id = Some(p.text()?),
                    "ignore_unmapped" => ignore_unmapped = p.boolean_value()?,
                    "boost" => boost = p.float_value()?,
                    "_name" => name = Some(p.text()?),
                    _ => return Err(p.parsing_error(format!("[parent_id] query does not support [{current}]"))),
                },
                _ => return Err(p.parsing_error(format!("[parent_id] query does not support [{current}]"))),
            }
        }
        let ty = ty.ok_or_else(|| EsError::illegal_argument("[parent_id] requires 'type' field"))?;
        let id = id.ok_or_else(|| EsError::illegal_argument("[parent_id] requires 'id' field"))?;
        let mut q = ParentId { ty, id, ignore_unmapped: false, common: Common::default() };
        q.common.name = name;
        q.common.boost = check_boost(boost, || q.clone().into())?;
        q.ignore_unmapped = ignore_unmapped;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), self.ty.clone().into());
        m.insert("id".into(), self.id.clone().into());
        if self.ignore_unmapped {
            m.insert("ignore_unmapped".into(), true.into());
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}
