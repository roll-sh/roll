//! Document and field level security: what a principal's roles let it see of the documents of
//! an index it may read (`IndicesAccessControl`, `DocumentPermissions`, `FieldPermissions`).
//!
//! For one action on one index, the role entries (`indices` groups) that grant the action on
//! the index (by its name, or its data stream's) decide:
//!
//! - **documents**: the entries' `query`s, any of which a document must match; an entry without
//!   a query lifts the restriction. An API key's own descriptors and its owner's (`limited_by`)
//!   each restrict on their own, so a document must pass both.
//! - **fields**: the fields an entry's `field_security` grants (`grant` patterns minus `except`
//!   patterns), the union over the entries; an entry without one grants every field. Again an
//!   API key passes both sets. Metadata fields are always visible.
//!
//! Searches take the document restriction as a filter of each target, as an alias filter; gets
//! answer "not found" for a document outside it. Hidden fields are left out of `_source`, and
//! queries and aggregations see them as unmapped.

use super::privileges::{self, IndexGrant, Permission};
use super::{Effective, Principal};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::Ctx;
use std::sync::Arc;

/// The action document and field level security are computed for (every read privilege that
/// grants searches grants gets too).
pub const READ: &str = "indices:data/read/search";

/// The restrictions of a principal's reads; `None` from [`ReadSecurity::of`] when none of its
/// role entries restricts documents or fields.
#[derive(Clone)]
pub struct ReadSecurity(Arc<Inner>);

struct Inner {
    eff: Effective,
    /// `_user` of templated role queries.
    user: Value,
    /// The salt of masked fields' hashes (OpenSearch Security).
    salt: [u8; 16],
}

impl std::fmt::Debug for ReadSecurity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadSecurity")
    }
}

/// The fields one set of role entries lets a principal see (`None`: all of them).
#[derive(Clone, Debug, PartialEq)]
pub struct FieldFilter {
    /// Each set's entries as `(grant, except)` patterns: a field is visible when every set has
    /// an entry granting it.
    sets: Vec<Vec<(Vec<String>, Vec<String>)>>,
    /// OpenSearch Security's masked fields: the granting entries' rules. A field is masked when
    /// any entry masks it (the plugin combines the roles' masks), by the first such rule; masked
    /// values are shown hashed and the field is unmapped for queries and aggregations.
    masks: Vec<Vec<String>>,
    salt: [u8; 16],
}

/// Metadata fields every role sees (`FieldPermissions` adds them to every grant).
const METADATA: &[&str] = &[
    "_id", "_index", "_routing", "_source", "_seq_no", "_primary_term", "_version", "_ignored", "_ignored_source", "_nested_path", "_field_names",
    "_doc_count", "_tier", "_tsid", "_ts_routing_hash", "_data_stream_timestamp", "_feature", "_score", "_doc", "_shard_doc", "_inference_fields",
];

impl FieldFilter {
    /// Whether the field at `path` is visible.
    pub fn allows(&self, path: &str) -> bool {
        METADATA.contains(&path) || self.sets.iter().all(|set| set.iter().any(|(grant, except)| matches(grant, path) && !matches(except, path)))
    }

    /// Whether queries and aggregations see the field: visible and not masked.
    pub fn searchable(&self, path: &str) -> bool {
        self.allows(path) && self.mask_of(path).is_none()
    }

    /// The masking rule of the field at `path`, when it is masked.
    fn mask_of<'a>(&'a self, path: &str) -> Option<&'a str> {
        if self.masks.is_empty() || METADATA.contains(&path) {
            return None;
        }
        self.masks.iter().flatten().find(|r| privileges::glob(super::opensearch::mask_field(r), path)).map(String::as_str)
    }

    /// Whether some field under the object at `path` may be visible.
    fn may_allow_under(&self, path: &str) -> bool {
        let probe = format!("{path}.");
        self.sets.iter().all(|set| set.iter().any(|(grant, _)| grant.iter().any(|g| prefix_may_match(g, &probe))))
    }

    /// `_source` with the hidden fields left out (`FieldSubsetReader.filter`): objects and arrays
    /// are walked by path, a value is kept when its full path is visible, and objects or arrays
    /// left empty are dropped.
    pub fn filter_source(&self, v: Value) -> Value {
        match v {
            Value::Obj(m) => Value::Obj(self.filter_map(m, "")),
            other => other,
        }
    }

    /// A mapping (`{"properties": …}`, as `GET _mapping` prints it) without the hidden fields
    /// (`FieldPermissions` filtering `MappingMetadata`): a field goes when its full path is
    /// hidden, a multi-field when its own path is, an object when nothing under it is left.
    pub fn filter_mapping(&self, mapping: Value) -> Value {
        match mapping {
            Value::Obj(mut m) => {
                if let Some(Value::Obj(props)) = m.shift_remove("properties") {
                    m.insert("properties".into(), Value::Obj(self.filter_properties(props, "")));
                }
                if let Some(Value::Obj(rt)) = m.shift_remove("runtime") {
                    m.insert("runtime".into(), Value::Obj(rt.into_iter().filter(|(k, _)| self.allows(k)).collect()));
                }
                Value::Obj(m)
            }
            other => other,
        }
    }

    fn filter_properties(&self, props: Map, prefix: &str) -> Map {
        let mut out = Map::new();
        for (k, v) in props {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            let Value::Obj(mut m) = v else { continue };
            if let Some(Value::Obj(inner)) = m.shift_remove("properties") {
                let kept = self.filter_properties(inner, &path);
                if kept.is_empty() && !self.allows(&path) {
                    continue;
                }
                m.insert("properties".into(), Value::Obj(kept));
                out.insert(k, Value::Obj(m));
                continue;
            }
            if !self.allows(&path) {
                continue;
            }
            if let Some(Value::Obj(multi)) = m.shift_remove("fields") {
                let kept: Map = multi.into_iter().filter(|(s, _)| self.allows(&format!("{path}.{s}"))).collect();
                if !kept.is_empty() {
                    m.insert("fields".into(), Value::Obj(kept));
                }
            }
            out.insert(k, Value::Obj(m));
        }
        out
    }

    /// A `fields` (or stored fields) response map, keyed by full field paths, without the
    /// hidden fields.
    pub fn filter_fields(&self, m: Map) -> Map {
        self.filter_map(m, "")
    }

    fn filter_map(&self, m: Map, prefix: &str) -> Map {
        let mut out = Map::new();
        for (k, v) in m {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            if let Some(v) = self.filter_value(v, &path) {
                out.insert(k, v);
            }
        }
        out
    }

    fn filter_value(&self, v: Value, path: &str) -> Option<Value> {
        match v {
            Value::Obj(m) => {
                if !self.may_allow_under(path) {
                    return None;
                }
                let inner = self.filter_map(m, path);
                (!inner.is_empty()).then_some(Value::Obj(inner))
            }
            Value::Arr(a) => {
                let kept: Vec<Value> = a.into_iter().filter_map(|x| self.filter_value(x, path)).collect();
                (!kept.is_empty()).then_some(Value::Arr(kept))
            }
            _ if !self.allows(path) => None,
            leaf => Some(match (self.mask_of(path), leaf) {
                (Some(rule), Value::Str(s)) => Value::Str(super::opensearch::mask(rule, &self.salt, &s)),
                (_, leaf) => leaf,
            }),
        }
    }
}

fn matches(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|p| privileges::glob(p, path))
}

/// Whether a field pattern could match some path starting with `prefix` (`*` spans dots).
fn prefix_may_match(pattern: &str, prefix: &str) -> bool {
    // Compare the literal start of the pattern with the prefix: a `*` matches the rest.
    let lit = pattern.split('*').next().unwrap_or_default();
    if pattern.contains('*') {
        lit.starts_with(prefix) || prefix.starts_with(lit)
    } else {
        pattern.starts_with(prefix)
    }
}

impl ReadSecurity {
    /// The read restrictions of the request's principal.
    pub fn of(ctx: &Ctx) -> Option<ReadSecurity> {
        let p = ctx.principal.get().filter(|p| !p.is_superuser())?;
        let eff = super::effective(&super::file_roles(ctx.cluster), &ctx.state(), p);
        let restricts = eff.own.iter().chain(eff.limited_by.iter().flatten()).flat_map(|r| r.indices.iter()).any(|g| g.query.is_some() || g.field_security.is_some() || !g.masked.is_empty());
        restricts.then(|| ReadSecurity(Arc::new(Inner { eff, user: user_params(p), salt: super::opensearch::masking_salt(ctx.cluster) })))
    }

    fn sets(&self) -> impl Iterator<Item = &Vec<Permission>> {
        std::iter::once(&self.0.eff.own).chain(self.0.eff.limited_by.iter())
    }

    /// A set's entries granting `action` on `index` (or on its data stream).
    fn granting<'a>(&self, set: &'a [Permission], action: &str, index: &str) -> Vec<&'a IndexGrant> {
        let parent = self.0.eff.parents.get(index);
        let restricted = privileges::is_restricted(index);
        set.iter()
            .flat_map(|r| r.indices.iter())
            .filter(|g| {
                (!restricted || g.allow_restricted)
                    && g.privileges.iter().any(|x| privileges::index_grants(x, action))
                    && g.names.iter().any(|n| privileges::index_pattern_matches(n, index) || parent.is_some_and(|ds| privileges::index_pattern_matches(n, ds)))
            })
            .collect()
    }

    /// The query a document of `index` must match to be read by `action`, `None` when every
    /// document may be.
    pub fn documents(&self, cluster: &Cluster, action: &str, index: &str) -> EsResult<Option<crate::query::Query>> {
        let mut filters = Vec::new();
        for set in self.sets() {
            let entries = self.granting(set, action, index);
            if entries.is_empty() || entries.iter().any(|g| g.query.is_none()) {
                continue;
            }
            let mut should = Vec::new();
            for g in entries {
                if let Some(q) = &g.query {
                    should.push(self.role_query(cluster, q)?);
                }
            }
            filters.push(obj! { "bool" => obj! { "should" => Value::Arr(should), "minimum_should_match" => 1 } });
        }
        let q = match filters.len() {
            0 => return Ok(None),
            1 => filters.pop().unwrap_or(Value::Null),
            _ => obj! { "bool" => obj! { "filter" => Value::Arr(filters) } },
        };
        crate::query::parse_stored(&q).map(Some)
    }

    /// Whether document or field level security applies to `action` on `index`.
    pub fn restricts(&self, action: &str, index: &str) -> bool {
        self.sets().any(|set| {
            let entries = self.granting(set, action, index);
            !entries.is_empty() && (entries.iter().all(|g| g.query.is_some()) || entries.iter().all(|g| g.field_security.is_some()) || entries.iter().any(|g| !g.masked.is_empty()))
        })
    }

    /// The fields of `index` that `action` may read, `None` when every field.
    pub fn fields(&self, action: &str, index: &str) -> Option<FieldFilter> {
        let mut sets = Vec::new();
        let mut masks = Vec::new();
        for set in self.sets() {
            let entries = self.granting(set, action, index);
            if masks.is_empty() {
                masks = entries.iter().filter(|g| !g.masked.is_empty()).map(|g| g.masked.clone()).collect();
            }
            if entries.is_empty() || entries.iter().any(|g| g.field_security.is_none()) {
                continue;
            }
            let strs = |v: Option<&Value>| -> Vec<String> { v.and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default() };
            sets.push(entries.iter().filter_map(|g| g.field_security.as_ref()).map(|fs| (strs(fs.get("grant")), strs(fs.get("except")))).collect());
        }
        (!sets.is_empty() || !masks.is_empty()).then_some(FieldFilter { sets, masks, salt: self.0.salt })
    }

    /// A role's query as JSON: a template (`{"template": {"source": …}}`) rendered with `_user`.
    fn role_query(&self, cluster: &Cluster, q: &Value) -> EsResult<Value> {
        let text = match q {
            Value::Str(s) => s.clone(),
            other => other.to_json(),
        };
        let v = crate::json::parse(text.as_bytes())?.unwrap_or_else(Value::object);
        let Some(t) = v.get("template") else { return Ok(v) };
        let source = match t.get("source") {
            Some(Value::Str(s)) => s.clone(),
            Some(other) => other.to_json(),
            None => return Err(EsError::illegal_argument("role query templates must have an inline [source]")),
        };
        let mut params = t.get("params").and_then(Value::as_object).cloned().unwrap_or_default();
        params.insert("_user".into(), self.0.user.clone());
        let compiled = cluster.scripts.engine.compile_template(&source, search_painless::Context::Template, &[]).map_err(crate::scripting::script_error)?;
        let rendered = compiled.render_json(&crate::scripting::to_serde(&Value::Obj(params))).map_err(crate::scripting::script_error)?;
        Ok(crate::json::parse(rendered.as_bytes())?.unwrap_or_else(Value::object))
    }
}

/// `_user` of role query templates (`SecurityQueryTemplateEvaluator`).
fn user_params(p: &Principal) -> Value {
    obj! {
        "username" => p.username.clone(),
        "full_name" => p.full_name.clone(),
        "email" => p.email.clone(),
        "roles" => Value::Arr(p.roles.iter().map(Value::from).collect()),
        "metadata" => p.metadata.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(sets: &[&[(&[&str], &[&str])]]) -> FieldFilter {
        let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        FieldFilter { sets: sets.iter().map(|set| set.iter().map(|(g, e)| (s(g), s(e))).collect()).collect(), masks: Vec::new(), salt: [0; 16] }
    }

    #[test]
    fn hidden_fields_leave_the_source() {
        let src = crate::json::parse(br#"{"name":"a","secret":"s","object":{"secret":"m","public":"i"},"list":[{"secret":1,"public":2}]}"#).unwrap().unwrap();
        let f = filter(&[&[(&["*"], &["secret", "object.secret", "list.secret"])]]);
        assert_eq!(f.filter_source(src.clone()).to_json(), r#"{"name":"a","object":{"public":"i"},"list":[{"public":2}]}"#);
        let only = filter(&[&[(&["name"], &[])]]);
        assert_eq!(only.filter_source(src.clone()).to_json(), r#"{"name":"a"}"#);
        let objects = filter(&[&[(&["object.*"], &[])]]);
        assert_eq!(objects.filter_source(src).to_json(), r#"{"object":{"secret":"m","public":"i"}}"#);
        assert!(only.allows("_id") && !only.allows("secret"));
    }

    #[test]
    fn every_set_must_grant() {
        let f = filter(&[&[(&["a", "b"], &[])], &[(&["b", "c"], &[])]]);
        assert!(f.allows("b"));
        assert!(!f.allows("a") && !f.allows("c"));
    }
}
