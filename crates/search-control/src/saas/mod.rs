//! Migrating into roll from Algolia, Typesense and Meilisearch: the same two routes as
//! [`crate::migrate`] (`…/migrate/inspect`, `…/migrate`) with `source.kind` naming the product.
//!
//! Every call to the source goes through search-es' guarded client
//! ([`search_es::api::remote`]: public addresses only unless the node allows private ones, no
//! redirects, bounded answers), under the same control slots, deadline and budgets as an
//! Elasticsearch migration. Algolia's host is fixed by its application id
//! (`https://{app}-dsn.algolia.net`; the operator's [`ALGOLIA_URL`] replaces it, for tests or a
//! proxy); Typesense and Meilisearch take a URL, which the guard checks. Credentials come in the
//! request body, are used for that request and the copy tasks it starts, and are never stored,
//! returned, logged or put in a task's description (the description names the application id or
//! the host).
//!
//! `inspect` lists the source's indices (Algolia's `GET /1/indexes`, Typesense's
//! `GET /collections`, Meilisearch's `GET /indexes` and `GET /stats`). `migrate` reads each picked
//! index's definition (settings, schema, synonyms, rules, a few records to see the attributes'
//! types) into a [`Plan`]: mappings, analyzers, a synonyms set wired to the index's search
//! analyzer, query rulesets, and a search template (`<dest>-search`) that searches as the source
//! did where Elasticsearch can say the same. What can't be said is listed in `not_mapped` with
//! why, never dropped silently; `dry_run` answers the plan without making anything. Then it
//! creates them and starts a copy: a task registered in the cluster's `_tasks` as a reindex
//! (`indices:data/write/reindex`), so the dashboard follows and cancels it as it does a reindex
//! from remote. The copy reads one page at a time (Algolia's `browse` cursor, Meilisearch's
//! offset pages, Typesense's JSONL export read line by line), charged to the node's reindex
//! budget as reindex from remote's pages are, and writes it with `_bulk` before reading the next.
//! It holds one of the node's reindex-from-remote task slots while it runs. A copy that failed or
//! was cancelled restarts by picking the index again: the index roll made for it (its
//! `_meta.roll_import`) is copied into again, documents overwritten by id.

pub(crate) mod algolia;
mod copy;
mod filters;
pub(crate) mod later;
pub(crate) mod meili;
mod template;
pub(crate) mod typesense;

use crate::control::{Control, ControlError, Result};
use crate::migrate::{remote_error, Inspected, Kind, MigrateRequest, Pick, Progress, Source, Stage};
use later::{now_ms, CopyState, Imported, Position};
use crate::store::SearchCluster;
use copy::{response, Copy, Reader};
use search_es::api::by_query::Live;
use search_es::api::remote::{self, Client};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};

/// Node setting: the URL roll calls for every Algolia application instead of
/// `https://{app}-dsn.algolia.net` (tests; an operator's proxy).
pub const ALGOLIA_URL: &str = "roll.migrate.algolia.url";
/// The most synonyms one index brings (one synonyms set; the rest are listed as not mapped).
pub const MAX_SYNONYMS: usize = 10_000;
/// The most rules one ruleset holds (Elasticsearch's limit), and the most rulesets one search
/// names: rules past 1,000 are listed as not mapped.
const RULES_PER_SET: usize = 100;
const MAX_RULESETS: usize = 10;
/// The most rules one index brings.
pub(crate) const MAX_RULES: usize = MAX_RULESETS * RULES_PER_SET;
/// The most documents one pinned or excluded rule names (Elasticsearch's limit).
const MAX_RULE_IDS: usize = 100;
/// Entries `not_mapped` lists per index (the rest are counted).
const MAX_NOT_MAPPED: usize = 500;
/// Algolia rules that become search templates of their own (filters, query rewrites).
const MAX_RULE_TEMPLATES: usize = 50;
/// Algolia replicas read for their sorts.
const MAX_REPLICAS: usize = 20;
/// Records read to see what types the attributes the settings name hold.
const SAMPLE: usize = 20;
/// Documents per page, unless the request says (Algolia's `browse` takes at most 1,000).
const PAGE_DOCS: u32 = 1000;
/// The task action the copies run as: what the dashboard and `_tasks?actions=*reindex` find.
const ACTION: &str = "indices:data/write/reindex";
/// What a document field the copy keeps may be named at most, and its `_id` (Elasticsearch's).
const MAX_ID_BYTES: usize = 512;
/// The breaker label of what the plan and the copy hold.
const LABEL: &str = "migrate_saas";

/// Where `saas_one` left an index: answered (planned, queued, failed), or read and ready to be
/// made and copied.
enum Step {
    Done(Value),
    Ready(Box<Ready>),
}

/// An index read from the source, ready to be made on roll and copied.
struct Ready {
    plan: Plan,
    mappings: Value,
    summary: Value,
    restart: bool,
    /// A stopped copy's record and where it resumes (`None`: from the start).
    resume: Option<(Imported, Option<Position>)>,
    permit: Option<remote::RemoteSlot>,
    index: String,
    dest: String,
}

/// The product, how to reach it and how to log in: the base URL (`scheme://host:port[/prefix]`)
/// and the headers carrying the key. Its `Debug` leaves the headers out.
#[derive(Clone)]
pub(crate) struct Spec {
    pub kind: Kind,
    pub base: String,
    /// Which application or server: Algolia's application id, or the URL (part of a copy's
    /// `_meta.roll_import`, so another source's index of the same name is never copied into).
    pub key: String,
    /// How answers and task descriptions name the source: Algolia's application id, or the host.
    pub label: String,
    headers: Vec<(String, String)>,
}

impl std::fmt::Debug for Spec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spec").field("kind", &self.kind).field("base", &self.base).finish_non_exhaustive()
    }
}

/// The product's display name.
pub(crate) fn product(k: Kind) -> &'static str {
    match k {
        Kind::Algolia => "Algolia",
        Kind::Typesense => "Typesense",
        Kind::Meilisearch => "Meilisearch",
        Kind::Elasticsearch | Kind::Opensearch => "Elasticsearch",
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Algolia => "algolia",
        Kind::Typesense => "typesense",
        Kind::Meilisearch => "meilisearch",
        Kind::Elasticsearch => "elasticsearch",
        Kind::Opensearch => "opensearch",
    }
}

/// The checked source: which fields each product takes, its URL, its key.
fn spec_of(s: &Source, algolia_url: Option<&str>) -> Result<Spec> {
    spec_for(s, algolia_url, true)
}

/// [`spec_of`]; `keyed: false` takes a source without its key (a cutover calls no source).
fn spec_for(s: &Source, algolia_url: Option<&str>, keyed: bool) -> Result<Spec> {
    let invalid = |m: &str| Err(ControlError::Invalid(m.to_string()));
    let key = s.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty());
    if s.username.is_some() || s.password.is_some() {
        return invalid(&format!("{} takes an API key, not a username and password", product(s.kind)));
    }
    // A key that can't be a header value is refused before it's used, without quoting it.
    if key.is_some_and(|k| k.len() > 4096 || k.bytes().any(|c| !(0x21..=0x7e).contains(&c))) {
        return invalid("the API key holds characters a key can't");
    }
    match s.kind {
        Kind::Algolia => {
            if !s.host.trim().is_empty() {
                return invalid("Algolia's host comes from the application id: give app_id, not host");
            }
            let app = s.app_id.as_deref().map(str::trim).unwrap_or_default();
            if app.is_empty() || app.len() > 32 || !app.bytes().all(|c| c.is_ascii_alphanumeric()) {
                return invalid("app_id is Algolia's application id (letters and digits, as the dashboard shows it)");
            }
            let key = match key {
                Some(k) => k,
                None if !keyed => "",
                None => return invalid("Algolia needs an API key that can browse, read settings, synonyms and rules (the admin key, or one with browse and settings)"),
            };
            let base = match algolia_url {
                Some(u) => crate::migrate::normalize_host(u)?,
                None => format!("https://{}-dsn.algolia.net:443", app.to_ascii_lowercase()),
            };
            Ok(Spec {
                kind: s.kind,
                base,
                key: app.to_ascii_lowercase(),
                label: format!("algolia app [{app}]"),
                headers: vec![("X-Algolia-Application-Id".into(), app.to_string()), ("X-Algolia-API-Key".into(), key.to_string())],
            })
        }
        Kind::Typesense | Kind::Meilisearch => {
            if s.app_id.is_some() {
                return invalid(&format!("{} takes a host (its URL), not app_id", product(s.kind)));
            }
            let base = crate::migrate::normalize_host(&s.host)?;
            let headers = match (s.kind, key) {
                (Kind::Typesense, Some(k)) => vec![("X-TYPESENSE-API-KEY".into(), k.to_string())],
                (Kind::Typesense, None) if keyed => return invalid("Typesense needs an API key (an admin key, or one with collections:get, documents:export, synonyms:get and overrides:get)"),
                (_, Some(k)) => vec![("Authorization".into(), format!("Bearer {k}"))],
                // A Meilisearch without a master key takes requests without one.
                (_, None) => Vec::new(),
            };
            Ok(Spec { kind: s.kind, label: base.clone(), key: base.clone(), base, headers })
        }
        Kind::Elasticsearch | Kind::Opensearch => unreachable!("not a SaaS source"),
    }
}

/// One index of the source, as `inspect` lists it.
pub(crate) struct Listed {
    pub index: String,
    pub docs: Option<i64>,
    pub size: Option<i64>,
    /// An Algolia replica: the index it's a replica of (copy the primary; its sort comes along).
    pub replica_of: Option<String>,
}

/// Reads definitions from the source for one plan, charging what it keeps to the control
/// budget, and says a failure as the caller sees it (never quoting the source).
pub(crate) struct Fetch<'a> {
    pub client: &'a Client,
    pub host: &'a str,
    pub held: search_es::breaker::Reservation,
}

impl Fetch<'_> {
    fn err(&self, e: search_es::EsError) -> String {
        match remote_error(self.host, e) {
            ControlError::Invalid(m) | ControlError::Busy(m) => m.replace("remote is likely not an Elasticsearch instance", "the source didn't answer with JSON"),
            e => e.to_string(),
        }
    }

    /// The charged tree as plain JSON, charged again while the plan holds it.
    fn plain(&mut self, v: &search_es::json::Value) -> std::result::Result<Value, String> {
        let text = v.to_json();
        self.held.add(3 * text.len() as u64, LABEL).map_err(|e| self.err(e))?;
        serde_json::from_str(&text).map_err(|_| "the source's answer isn't JSON roll reads".to_string())
    }

    pub async fn get(&mut self, path: &str) -> std::result::Result<Value, String> {
        let c = self.client.get(path).await.map_err(|e| self.err(e))?;
        self.plain(&c.value)
    }

    /// [`Fetch::get`] that answers `None` for a 404 (an endpoint an older or newer version
    /// doesn't have).
    pub async fn get_opt(&mut self, path: &str) -> std::result::Result<Option<Value>, String> {
        match self.client.get(path).await {
            Ok(c) => self.plain(&c.value).map(Some),
            Err(e) if e.status == 404 => Ok(None),
            Err(e) => Err(self.err(e)),
        }
    }

    pub async fn post(&mut self, path: &str, body: &Value) -> std::result::Result<Value, String> {
        let c = self.client.post(path, &body.to_string()).await.map_err(|e| self.err(e))?;
        self.plain(&c.value)
    }
}

/// What a mapped field becomes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Field {
    /// Full text (the index's default analyzer), with a `keyword` sub-field for facets, filters
    /// and sorts when `keyword`.
    Text { keyword: bool },
    Keyword,
    /// A mapping given whole (a Typesense schema's typed field).
    Typed(Value),
}

/// What one source index becomes on roll: its mappings, analysis, synonyms, rules and search
/// template, and everything that didn't map.
#[derive(Default, Debug)]
pub(crate) struct Plan {
    /// The document field whose value is `_id` (`objectID`, `id`, the primary key).
    pub id_field: String,
    /// The source's document count, when it says.
    pub total: Option<u64>,
    /// Explicit mappings by dotted path; everything else is mapped dynamically.
    pub fields: BTreeMap<String, Field>,
    /// `index.query.default_field`: what a search without fields searches, with boosts.
    pub default_fields: Vec<String>,
    pub source_excludes: Vec<String>,
    pub source_includes: Vec<String>,
    /// Fields returned by default (the template's `_source`).
    pub retrieve: Vec<String>,
    /// A stemmer language for plurals (`minimal_english`…), at index and search time.
    pub stemmer: Option<String>,
    /// Stop words removed at search time: Elasticsearch's lists (`_english_`) or words.
    pub stop: Vec<String>,
    /// Synonym rules in Solr's format (`a, b` or `a => b, c`), with their source ids.
    pub synonyms: Vec<(String, String)>,
    /// Elasticsearch query rules (`rule_id`, `type`, `criteria`, `actions`).
    pub rules: Vec<Value>,
    /// Pinned rules of Algolia rules with `filterPromotes`: applied inside the search's
    /// `filters`, so their pins must match them.
    pub filtered_rules: Vec<Value>,
    /// Algolia rules that filter or rewrite the query, each a search template of its own.
    pub rule_templates: Vec<RuleTemplate>,
    /// `fuzziness` for the template (`AUTO:4,8`), or none when typo tolerance is off.
    pub fuzziness: Option<String>,
    /// The last word matches as a prefix (search as you type): `bool_prefix`.
    pub prefix: bool,
    /// Sorts before relevance (an Algolia `ranking` led by `asc(…)`), and after it.
    pub sort_first: Vec<(String, &'static str)>,
    pub sort_after: Vec<(String, &'static str)>,
    /// A field collapse, and how many per group (Algolia's `distinct` past 1).
    pub collapse: Option<(String, u32)>,
    pub size: Option<u32>,
    /// Algolia replicas: their name and their sort.
    pub replicas: Vec<(String, Vec<(String, &'static str)>)>,
    /// Typesense `geopoint` fields (`[lat, lng]`, turned into `{lat, lon}` as they're copied).
    pub geo_points: Vec<String>,
    pub not_mapped: Vec<Value>,
    pub not_mapped_more: usize,
    pub notes: Vec<String>,
    /// The query rules' ids so far (each made unique).
    rule_ids: HashSet<String>,
}

impl Plan {
    /// Lists something that didn't map, and why (the first [`MAX_NOT_MAPPED`], then counted).
    pub fn skip(&mut self, kind: &str, name: impl Into<String>, reason: impl Into<String>) {
        if self.not_mapped.len() >= MAX_NOT_MAPPED {
            self.not_mapped_more += 1;
            return;
        }
        self.not_mapped.push(json!({ "kind": kind, "name": name.into(), "reason": reason.into() }));
    }

    /// Marks `path` as needing a keyword (facets, filters, sorts, distinct), keeping its text.
    pub fn keyword(&mut self, path: &str, sample: &Sample) {
        match self.fields.get_mut(path) {
            Some(Field::Text { keyword }) => *keyword = true,
            Some(_) => {}
            None if sample.may_be_text(path) => {
                self.fields.insert(path.to_string(), Field::Keyword);
            }
            None => {}
        }
    }

    /// The field a sort, a filter or a collapse names for `path`: its keyword sub-field if it
    /// has text.
    pub fn exact(&self, path: &str) -> String {
        match self.fields.get(path) {
            Some(Field::Text { .. }) => format!("{path}.keyword"),
            _ => path.to_string(),
        }
    }

    /// A searchable attribute: text with the default analyzer (an object's fields with `.*`),
    /// searched by default with `boost`.
    pub fn searchable(&mut self, path: &str, boost: u32, sample: &Sample) {
        let name = if sample.holds_objects(path) {
            format!("{path}.*")
        } else {
            if !matches!(self.fields.get(path), Some(Field::Typed(_))) && sample.may_be_text(path) {
                let keyword = matches!(self.fields.get(path), Some(Field::Keyword | Field::Text { keyword: true }));
                self.fields.insert(path.to_string(), Field::Text { keyword });
            }
            path.to_string()
        };
        self.default_fields.push(if boost > 1 { format!("{name}^{boost}") } else { name });
    }

    /// Leaves out what can't reach roll as named: attribute names that are Elasticsearch
    /// metadata fields (`_id`, `_source` …), and names the search template carries that aren't
    /// plain attribute names (a `{`, a `"`, a space …), each listed with why.
    pub fn guard(&mut self) {
        let fields = std::mem::take(&mut self.fields);
        for (k, v) in fields {
            if is_meta(&k) {
                self.skip("field", k, "an Elasticsearch metadata field: documents keep it under its own name only outside roll (it's left out of the copy)");
            } else {
                self.fields.insert(k, v);
            }
        }
        let defaults = std::mem::take(&mut self.default_fields);
        for d in defaults {
            let base = d.split('^').next().unwrap_or_default().trim_end_matches(".*").to_string();
            if is_meta(&base) || !template_safe(&base) {
                self.skip("setting", format!("searchable attribute: {base}"), "not an attribute name roll searches (a metadata field, or characters a field name can't hold)");
            } else {
                self.default_fields.push(d);
            }
        }
        let mut bad = Vec::new();
        let keep = |list: &mut Vec<(String, &'static str)>, bad: &mut Vec<String>| list.retain(|(f, _)| {
            let ok = template_safe(f) && !is_meta(f);
            if !ok {
                bad.push(f.clone());
            }
            ok
        });
        keep(&mut self.sort_first, &mut bad);
        keep(&mut self.sort_after, &mut bad);
        for (_, sort) in &mut self.replicas {
            keep(sort, &mut bad);
        }
        let replicas = std::mem::take(&mut self.replicas);
        for (name, sort) in replicas {
            if sort.is_empty() || !template_safe(&name) {
                self.skip("replica", name, "its sort names no attribute roll can sort on, or its name can't be a template's");
            } else {
                self.replicas.push((name, sort));
            }
        }
        if let Some((f, _)) = &self.collapse
            && (!template_safe(f) || is_meta(f))
        {
            bad.push(f.clone());
            self.collapse = None;
        }
        let shown = std::mem::take(&mut self.retrieve);
        for f in shown {
            if f == "*" || (template_safe(&f) && !is_meta(&f)) {
                self.retrieve.push(f);
            } else {
                bad.push(f);
            }
        }
        for f in bad {
            self.skip("setting", format!("attribute: {f}"), "not an attribute name the search template can carry (a metadata field, or characters a field name can't hold): left out of the template");
        }
        if !self.rules.is_empty() || !self.filtered_rules.is_empty() {
            let ids: Vec<&str> = self.rules.iter().chain(&self.filtered_rules).filter_map(|r| r["rule_id"].as_str()).take(20).collect();
            let more = (self.rules.len() + self.filtered_rules.len()).saturating_sub(ids.len());
            self.notes.push(format!(
                "query rules ({}{}) match the search text exactly as sent, case and all, and `contains` is a substring test: send the text lowercased as `rules_query` (the template falls back to `query`)",
                ids.join(", "),
                if more > 0 { format!(" and {more} more") } else { String::new() }
            ));
        }
        self.source_excludes.retain(|f| !is_meta(f));
        self.source_includes.retain(|f| !is_meta(f));
        self.geo_points.retain(|f| !is_meta(f));
    }

    fn synonyms_set(dest: &str) -> String {
        format!("{dest}-synonyms")
    }

    fn rulesets(&self, dest: &str) -> Vec<String> {
        let n = self.rules.len().div_ceil(RULES_PER_SET);
        (0..n).map(|i| if i == 0 { format!("{dest}-rules") } else { format!("{dest}-rules-{}", i + 1) }).collect()
    }

    fn filtered_rulesets(&self, dest: &str) -> Vec<String> {
        let n = self.filtered_rules.len().div_ceil(RULES_PER_SET);
        (0..n).map(|i| if i == 0 { format!("{dest}-rules-filtered") } else { format!("{dest}-rules-filtered-{}", i + 1) }).collect()
    }

    fn rule_template_id(dest: &str, rule: &str) -> String {
        format!("{dest}-search-rule-{rule}")
    }

    /// Every search template the import makes, with its source.
    fn templates(&self, dest: &str) -> Vec<(String, String)> {
        let mut out = vec![(Plan::template_id(dest), self.template_source(dest, &self.sort_first, None))];
        for (r, sort) in &self.replicas {
            out.push((Plan::replica_template(dest, r), self.template_source(dest, sort, None)));
        }
        for r in &self.rule_templates {
            out.push((Plan::rule_template_id(dest, &r.rule), self.template_source(dest, &self.sort_first, Some(r))));
        }
        out
    }

    /// The rule templates as the import record and the answer list them.
    fn rule_template_list(&self, dest: &str) -> Vec<Value> {
        self.rule_templates.iter().map(|r| json!({ "rule": r.rule, "anchoring": r.anchoring, "pattern": r.pattern, "template": Plan::rule_template_id(dest, &r.rule) })).collect()
    }

    fn template_id(dest: &str) -> String {
        format!("{dest}-search")
    }

    fn replica_template(dest: &str, replica: &str) -> String {
        format!("{dest}-search-{replica}")
    }

    /// The index's mappings: the explicit fields as a tree, `dynamic` for the rest, where it
    /// came from (`_meta.roll_import`, which a restarted copy checks), `_source` filtering.
    pub fn mappings(&mut self, imported: &Imported) -> Value {
        let mut props = Map::new();
        // Deepest first: an attribute that holds objects (`a.b`) wins over a leaf at its parent
        // (`a`), which is the one listed.
        let mut fields: Vec<(String, Field)> = self.fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        fields.sort_by_key(|(k, _)| std::cmp::Reverse(k.matches('.').count()));
        let mut conflicts = Vec::new();
        for (path, f) in fields {
            let leaf = match f {
                Field::Text { keyword: true } => json!({ "type": "text", "fields": { "keyword": { "type": "keyword" } } }),
                Field::Text { keyword: false } => json!({ "type": "text" }),
                Field::Keyword => json!({ "type": "keyword" }),
                Field::Typed(v) => v,
            };
            if !insert_path(&mut props, &path, leaf) {
                conflicts.push(path);
            }
        }
        for p in conflicts {
            self.skip("field", p, "another mapped field is an object at this path: left to dynamic mapping");
        }
        let mut m = json!({ "dynamic": true, "_meta": { "roll_import": imported }, "properties": props });
        let mut source = Map::new();
        if !self.source_excludes.is_empty() {
            source.insert("excludes".into(), json!(self.source_excludes));
        }
        if !self.source_includes.is_empty() {
            source.insert("includes".into(), json!(self.source_includes));
        }
        if !source.is_empty() {
            m["_source"] = Value::Object(source);
        }
        m
    }

    /// The search analyzer's filters (`synonyms` the synonym filter, by name or inline) and the
    /// index analyzer's. Synonyms come right after case and accent folding, so a rule is parsed
    /// by nothing that drops or changes its words (a stop word in a synonym, a stemmed one); stop
    /// words and plurals apply to what the synonyms make.
    fn chains(&self, synonyms: Option<Value>) -> (Vec<Value>, Vec<Value>) {
        let fold = vec![json!("lowercase"), json!("asciifolding")];
        let mut search = fold.clone();
        let mut index = fold;
        if let Some(f) = synonyms {
            search.push(f);
        }
        if !self.stop.is_empty() {
            search.push(json!({ "type": "stop", "stopwords": self.stop }));
        }
        if let Some(lang) = &self.stemmer {
            search.push(json!({ "type": "stemmer", "language": lang }));
            index.push(json!({ "type": "stemmer", "language": lang }));
        }
        (search, index)
    }

    /// The index's settings: the default fields and an analysis chain close to the source's
    /// (case and accents folded, search-time synonyms and stop words, plurals).
    pub fn settings(&self, dest: &str) -> Value {
        let mut filter = Map::new();
        let syn = (!self.synonyms.is_empty()).then(|| json!({ "type": "synonym_graph", "synonyms_set": Plan::synonyms_set(dest), "updateable": true }));
        let (search, index) = self.chains(syn);
        // Inline filter definitions get names (an analyzer names its filters).
        let mut name = |f: Value, n: &str| -> Value {
            if f.is_string() {
                return f;
            }
            filter.insert(n.to_string(), f);
            json!(n)
        };
        let named = |f: &Value| -> &'static str {
            match f["type"].as_str() {
                Some("synonym_graph") => "roll_synonyms",
                Some("stop") => "roll_stop",
                _ => "roll_plurals",
            }
        };
        let search: Vec<Value> = search.into_iter().map(|f| { let n = named(&f); name(f, n) }).collect();
        let chain: Vec<Value> = index.into_iter().map(|f| { let n = named(&f); name(f, n) }).collect();
        let mut analysis = json!({
            "analyzer": {
                "default": { "type": "custom", "tokenizer": "standard", "filter": chain },
                "default_search": { "type": "custom", "tokenizer": "standard", "filter": search },
            }
        });
        if !filter.is_empty() {
            analysis["filter"] = Value::Object(filter);
        }
        let mut s = json!({ "analysis": analysis });
        if !self.default_fields.is_empty() {
            s["index.query.default_field"] = json!(self.default_fields);
        }
        s
    }

    /// What the answer says about the plan (also before anything is made: `dry_run`).
    fn summary(&self, dest: &str) -> Value {
        let mut v = json!({
            "id_field": self.id_field,
            "docs": self.total,
            "mappings": self.fields.len(),
            "default_fields": self.default_fields,
            "synonyms": self.synonyms.len(),
            "rules": self.rules.len() + self.filtered_rules.len(),
            "search_template": Plan::template_id(dest),
            "not_mapped": self.not_mapped,
            "not_mapped_more": self.not_mapped_more,
            "notes": self.notes,
        });
        if !self.synonyms.is_empty() {
            v["synonyms_set"] = json!(Plan::synonyms_set(dest));
        }
        if !self.rules.is_empty() {
            v["rulesets"] = json!(self.rulesets(dest));
        }
        if !self.filtered_rules.is_empty() {
            v["filtered_rulesets"] = json!(self.filtered_rulesets(dest));
        }
        if !self.rule_templates.is_empty() {
            v["rule_templates"] = json!(self.rule_template_list(dest));
        }
        if !self.replicas.is_empty() {
            v["sort_templates"] = Value::Object(self.replicas.iter().map(|(r, _)| (r.clone(), json!(Plan::replica_template(dest, r)))).collect());
        }
        v
    }
}

/// An Algolia rule's search template: the rule's condition, which a client matches to pick it,
/// and what it does (a filter, a literal query).
#[derive(Debug, Clone)]
pub(crate) struct RuleTemplate {
    pub rule: String,
    /// `is`, `startsWith`, `endsWith`, `contains` or `always` (no pattern).
    pub anchoring: String,
    pub pattern: String,
    pub filter: Option<Value>,
    pub query: Option<String>,
}

/// Puts `leaf` at the dotted `path` of a properties tree; `false` when the path crosses a leaf
/// or a leaf is already there.
fn insert_path(props: &mut Map<String, Value>, path: &str, leaf: Value) -> bool {
    let mut parts = path.split('.').peekable();
    let mut cur = props;
    while let Some(p) = parts.next() {
        if p.is_empty() {
            return false;
        }
        if parts.peek().is_none() {
            if cur.contains_key(p) {
                return false;
            }
            cur.insert(p.to_string(), leaf);
            return true;
        }
        let node = cur.entry(p.to_string()).or_insert_with(|| json!({ "properties": {} }));
        match node.get_mut("properties").and_then(Value::as_object_mut) {
            Some(m) => cur = m,
            None => return false,
        }
    }
    false
}

/// A few records, to see what the attributes the settings name hold (Algolia and Meilisearch
/// have no schema).
#[derive(Default)]
pub(crate) struct Sample(pub Vec<Value>);

impl Sample {
    fn values<'a>(&'a self, path: &str) -> Vec<&'a Value> {
        let mut out = Vec::new();
        for d in &self.0 {
            let mut cur = vec![d];
            for p in path.split('.') {
                let mut next = Vec::new();
                for v in cur {
                    match v {
                        Value::Object(m) => next.extend(m.get(p)),
                        Value::Array(a) => next.extend(a.iter().filter_map(|x| x.get(p))),
                        _ => {}
                    }
                }
                cur = next;
            }
            for v in cur {
                match v {
                    Value::Array(a) => out.extend(a.iter()),
                    v => out.push(v),
                }
            }
        }
        out
    }

    /// Some record holds an object there.
    pub fn holds_objects(&self, path: &str) -> bool {
        self.values(path).iter().any(|v| v.is_object())
    }

    /// Some record holds a string there, and none an object.
    pub fn holds_text(&self, path: &str) -> bool {
        let v = self.values(path);
        v.iter().any(|v| v.is_string()) && !v.iter().any(|v| v.is_object())
    }

    /// Text unless the records show numbers, booleans or objects there (an attribute no record
    /// holds is taken for text).
    pub fn may_be_text(&self, path: &str) -> bool {
        let v = self.values(path);
        v.is_empty() || self.holds_text(path)
    }
}

/// Elasticsearch's minimal (plural-only) stemmer for a language code, if it has one.
pub(crate) fn plural_stemmer(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "en" => "minimal_english",
        "fr" => "minimal_french",
        "de" => "minimal_german",
        "pt" => "minimal_portuguese",
        "es" => "light_spanish",
        "it" => "light_italian",
        "nl" => "dutch",
        "sv" => "light_swedish",
        "fi" => "light_finnish",
        "ru" => "light_russian",
        "no" | "nb" => "light_norwegian",
        _ => return None,
    })
}

/// Elasticsearch's stop word list for a language code.
pub(crate) fn stop_list(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "en" => "_english_",
        "fr" => "_french_",
        "de" => "_german_",
        "es" => "_spanish_",
        "it" => "_italian_",
        "pt" => "_portuguese_",
        "nl" => "_dutch_",
        "sv" => "_swedish_",
        "da" => "_danish_",
        "fi" => "_finnish_",
        "no" | "nb" => "_norwegian_",
        "ru" => "_russian_",
        "tr" => "_turkish_",
        "ar" => "_arabic_",
        _ => return None,
    })
}

/// A query rule's criteria on the search's query text.
pub(crate) fn criteria(kind: &str, values: Vec<String>) -> Value {
    if kind == "always" {
        return json!({ "type": "always" });
    }
    json!({ "type": kind, "metadata": "query_string", "values": values })
}

/// Adds the pinned and excluded rules of one source rule (ids beyond Elasticsearch's 100 a
/// rule are listed as not mapped).
pub(crate) fn push_rules(plan: &mut Plan, id: &str, crit: Vec<Value>, mut pins: Vec<String>, mut hides: Vec<String>, filtered: bool) {
    let id = if id.is_empty() { format!("rule-{}", plan.rules.len() + plan.filtered_rules.len() + 1) } else { id.to_string() };
    let id = id.as_str();
    for (ty, ids, suffix) in [("pinned", &mut pins, ""), ("exclude", &mut hides, "-exclude")] {
        if ids.is_empty() {
            continue;
        }
        // One search names at most MAX_RULESETS rulesets of RULES_PER_SET rules.
        if plan.rules.len() + plan.filtered_rules.len() >= MAX_RULES {
            plan.skip("rule", format!("{id}{suffix}"), format!("past the {MAX_RULES} query rules an index's search can apply (Elasticsearch takes {MAX_RULESETS} rulesets of {RULES_PER_SET})"));
            continue;
        }
        if ids.len() > MAX_RULE_IDS {
            let n = ids.len() - MAX_RULE_IDS;
            ids.truncate(MAX_RULE_IDS);
            plan.skip("rule", id, format!("{n} more documents than a {ty} rule holds (100): the first 100 are kept"));
        }
        // Unique within the rulesets: a repeated id gets a stable `-2`, `-3` …
        let base = format!("{id}{suffix}");
        let mut rule_id = base.clone();
        let mut n = 1;
        while !plan.rule_ids.insert(rule_id.clone()) {
            n += 1;
            rule_id = format!("{base}-{n}");
        }
        let rule = json!({ "rule_id": rule_id, "type": ty, "criteria": crit, "actions": { "ids": ids } });
        // Only pins are filtered by `filterPromotes`: a hide hides whatever the filters say.
        if filtered && ty == "pinned" {
            plan.filtered_rules.push(rule);
        } else {
            plan.rules.push(rule);
        }
    }
}

/// Elasticsearch's metadata fields: no document field may take one of these names.
pub(crate) const META: &[&str] = &[
    "_id", "_index", "_source", "_routing", "_version", "_seq_no", "_primary_term", "_ignored", "_ignored_source", "_field_names", "_doc_count", "_tier", "_data_stream_timestamp", "_feature", "_nested_path", "_tsid", "_size", "_type", "_uid", "_parent", "_all",
];

/// Whether `path` names, or starts with, a metadata field.
pub(crate) fn is_meta(path: &str) -> bool {
    META.contains(&path.split('.').next().unwrap_or_default())
}

/// A name the search template can carry inside a JSON string as is: letters, digits, `_`,
/// `-`, `.` and `@`, no empty segments.
pub(crate) fn template_safe(name: &str) -> bool {
    sane_path(name) && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '@'))
}

/// A field name in a document: at most [`MAX_ID_BYTES`], no empty segments.
pub(crate) fn sane_path(p: &str) -> bool {
    !p.is_empty() && p.len() <= MAX_ID_BYTES && p.split('.').all(|s| !s.is_empty())
}

impl Control {
    /// The operator's Algolia URL ([`ALGOLIA_URL`]), if set.
    fn algolia_url(&self) -> Option<&str> {
        self.cfg.node_settings.iter().rev().find(|(k, _)| k == ALGOLIA_URL).map(|(_, v)| v.as_str())
    }

    /// A client for the source: the node's rules (not the cluster's whitelist), `budget`'s
    /// breaker, errors that never quote the source.
    async fn saas_client(&self, spec: &Spec, control: Option<u64>) -> Result<Client> {
        let mut r = search_es::api::by_query_request::Remote::parse(&spec.base).map_err(|e| ControlError::Invalid(e.reason().to_string()))?;
        r.headers.clone_from(&spec.headers);
        r.connect_timeout = search_es::units::TimeValue::parse("10s", "connect_timeout").ok();
        r.socket_timeout = search_es::units::TimeValue::parse("60s", "socket_timeout").ok();
        let rules = self.remote_rules()?.without_whitelist();
        let host = spec.label.clone();
        let refused = |e: search_es::EsError| {
            let reason = e.reason();
            if e.status == 503 {
                return ControlError::Busy(reason.to_string());
            }
            if reason.contains("private address") || reason.contains("may not reach") || reason.contains("blocked in") || reason.contains("not whitelisted") {
                ControlError::Invalid(format!("couldn't reach {host}"))
            } else {
                ControlError::Invalid(reason.to_string())
            }
        };
        rules.check_now(&r).await.map_err(refused)?;
        let reindex = self.node.reindex();
        Ok(match control {
            Some(limit) => Client::new(&rules, &r, Some(reindex.control_budget.clone())).map_err(refused)?.for_control(limit),
            None => Client::new(&rules, &r, Some(reindex.budget.clone())).map_err(refused)?.quiet(),
        })
    }

    /// `inspect` for Algolia, Typesense and Meilisearch: the source's indices, as an
    /// Elasticsearch listing's rows (`replica_of` for an Algolia replica).
    pub(crate) async fn saas_inspect(&self, c: &SearchCluster, s: &Source) -> Result<Inspected> {
        let spec = spec_of(s, self.algolia_url())?;
        let lister = self.saas_client(&spec, Some(remote::CONTROL_LISTING_LIMIT)).await?;
        let small = self.saas_client(&spec, Some(remote::CONTROL_ANSWER_LIMIT)).await?;
        let mut held = search_es::breaker::Reservation::new(self.node.reindex().control_budget.clone());
        let mut f = Fetch { client: &lister, host: &spec.label, held: search_es::breaker::Reservation::new(self.node.reindex().control_budget.clone()) };
        let (listed, version) = match spec.kind {
            Kind::Algolia => (algolia::list(&mut f).await, None),
            Kind::Typesense => (typesense::list(&mut f).await, typesense::version(&small).await),
            _ => (meili::list(&mut f, &small).await, meili::version(&small).await),
        };
        let listed = listed.map_err(ControlError::Invalid)?;
        let existing = self.roll_names(c).await?;
        let total = listed.len();
        let mut rows = Vec::new();
        for l in listed.into_iter().take(crate::migrate::MAX_LISTED) {
            held.add(2048 + 2 * l.index.len() as u64, "migrate_listing").map_err(|e| ControlError::Busy(e.reason().to_string()))?;
            let mut row = json!({
                "index": l.index, "health": null, "status": null, "docs": l.docs, "size_bytes": l.size,
                "primary_size_bytes": null, "primaries": null, "replicas": null,
                "aliases": [], "aliases_more": 0, "aliases_on_roll": [], "exists": existing.contains(&l.index),
            });
            if let Some(p) = l.replica_of {
                row["replica_of"] = json!(p);
            }
            rows.push(row);
        }
        rows.sort_by(|a, b| a["index"].as_str().cmp(&b["index"].as_str()));
        drop(f);
        let answer = json!({
            "remote": { "host": spec.label, "version": version.unwrap_or_default(), "distribution": kind_name(spec.kind), "cluster_name": null },
            "indices": rows,
            "more_indices": total.saturating_sub(crate::migrate::MAX_LISTED),
            "data_streams": [],
            "more_data_streams": 0,
        });
        let body = serde_json::to_vec(&answer).map_err(|e| ControlError::Internal(e.into()))?;
        drop(answer);
        held.add(body.len() as u64, "migrate_listing").map_err(|e| ControlError::Busy(e.reason().to_string()))?;
        Ok(Inspected { body: body.into(), held })
    }

    /// `migrate` for Algolia, Typesense and Meilisearch: each picked index planned, made and
    /// copied by a task (or only planned, `dry_run`), one row per index in order; one index's
    /// failure doesn't stop the others.
    pub(crate) async fn saas_migrate(&self, c: SearchCluster, req: &MigrateRequest) -> Result<Value> {
        let stage = req.step.unwrap_or_default();
        let spec = spec_for(&req.source, self.algolia_url(), stage != Stage::Cutover)?;
        if req.size.is_some_and(|n| n > PAGE_DOCS && spec.kind == Kind::Algolia) {
            return Err(ControlError::Invalid(format!("size is at most {PAGE_DOCS} for Algolia (its browse pages)")));
        }
        let dry = req.dry_run.unwrap_or(false);
        if stage != Stage::CatchUp && (req.since_attribute.is_some() || req.since_unit.is_some()) {
            return Err(ControlError::Invalid("since_attribute and since_unit are for step catch_up".into()));
        }
        if stage != Stage::Copy {
            if dry {
                return Err(ControlError::Invalid("dry_run is for step copy".into()));
            }
            return self.saas_later(c, &spec, req, stage).await;
        }
        // The cluster's migration slot, shared with each index's setup task: it frees once this
        // call and every setup it started have ended, so a dropped request can't let a second
        // migration start beside a setup still running.
        let slot = std::sync::Arc::new(self.control_slot(&c)?);
        let deadline = tokio::time::Instant::now() + crate::migrate::DEADLINE;
        let client = self.saas_client(&spec, Some(crate::migrate::DEFINITION_LIMIT)).await?;
        let mut names = self.roll_names(&c).await?;
        let mut out = Vec::new();
        for p in &req.indices {
            let dest = p.dest.clone().filter(|d| !d.trim().is_empty()).unwrap_or_else(|| p.index.clone());
            let mut row = json!({ "index": p.index, "dest": dest });
            if tokio::time::Instant::now() >= deadline {
                row["queued"] = json!(true);
                row["reason"] = json!(format!("the call ran past {}s before this index", crate::migrate::DEADLINE.as_secs()));
                out.push(row);
                continue;
            }
            let mut progress = Progress::default();
            let one = Box::pin(self.saas_one(&c, &spec, &client, p, &dest, dry, &names, &mut progress));
            let done = match tokio::time::timeout_at(deadline, one).await {
                Ok(Ok(Step::Done(v))) => v,
                Ok(Ok(Step::Ready(ready))) => {
                    // Made and started past the deadline, in a task the request can't drop.
                    let (me, c2, spec2, size, rps, held) = (self.arc(), c.clone(), spec.clone(), req.size, req.requests_per_second, slot.clone());
                    let joined = match me {
                        Some(me) => tokio::spawn(async move {
                            let done = me.finish_one(&c2, &spec2, *ready, size, rps).await;
                            drop(held);
                            done
                        })
                        .await
                        .ok(),
                        None => None,
                    };
                    match joined {
                        Some((v, created, task)) => {
                            if created {
                                names.insert(dest.clone());
                                progress.created = true;
                            }
                            progress.task = task;
                            v
                        }
                        None => json!({ "error": "the control plane is shutting down" }),
                    }
                }
                Ok(Err(e)) => json!({ "error": e }),
                Err(_) => json!({ "error": format!("the call ran past {}s while this index's definition was being read", crate::migrate::DEADLINE.as_secs()) }),
            };
            if let Value::Object(m) = done {
                row.as_object_mut().expect("an object").extend(m);
            }
            if progress.created {
                row["created"] = json!(true);
            }
            if let Some(t) = progress.task {
                row["task"] = json!(t);
            }
            out.push(row);
        }
        Ok(json!({ "indices": out }))
    }

    /// `catch_up` and `cutover`: one row per index, in order, within the call's deadline (a
    /// catch-up starts its task and returns).
    async fn saas_later(&self, c: SearchCluster, spec: &Spec, req: &MigrateRequest, stage: Stage) -> Result<Value> {
        let _slot = self.control_slot(&c)?;
        let deadline = tokio::time::Instant::now() + crate::migrate::DEADLINE;
        let mut out = Vec::new();
        for p in &req.indices {
            let dest = p.dest.clone().filter(|d| !d.trim().is_empty()).unwrap_or_else(|| p.index.clone());
            let mut row = json!({ "index": p.index, "dest": dest, "step": if stage == Stage::CatchUp { "catch_up" } else { "cutover" } });
            let one = async {
                match stage {
                    Stage::CatchUp => self.saas_catch_up(&c, spec, req, p, &dest).await,
                    _ => self.saas_cutover(&c, spec, p, &dest).await,
                }
            };
            match tokio::time::timeout_at(deadline, Box::pin(one)).await {
                Ok(Ok(Value::Object(m))) => row.as_object_mut().expect("an object").extend(m),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => row["error"] = json!(e),
                Err(_) => row["error"] = json!(format!("the call ran past {}s before this index", crate::migrate::DEADLINE.as_secs())),
            }
            out.push(row);
        }
        Ok(json!({ "indices": out }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn saas_one(
        &self,
        c: &SearchCluster,
        spec: &Spec,
        client: &Client,
        p: &Pick,
        dest: &str,
        dry: bool,
        names: &HashSet<String>,
        progress: &mut Progress,
    ) -> std::result::Result<Step, String> {
        if p.index.is_empty() || p.index.len() > MAX_ID_BYTES || p.index.contains(['/', '\\']) || p.index.chars().any(char::is_control) {
            return Err(format!("[{}] isn't an index name {} has", p.index.chars().take(100).collect::<String>(), product(spec.kind)));
        }
        // Where the copy came from: the product, which application or server, which index.
        let from = format!("{}:{}:{}", kind_name(spec.kind), spec.key, p.index);
        let mut restart = false;
        let mut resume = None;
        if names.contains(dest) {
            // A copy roll made from this same index (its earlier copy failed or was cancelled) is
            // copied into again, from where it stopped; anything else is never touched.
            let Some((imp, _)) = self.read_import(c, dest).await.filter(|(i, _)| i.from == from) else {
                return Err(format!("index [{dest}] already exists on roll: give the copy another name"));
            };
            if let Some(task) = self.running_copy(c, dest).await {
                progress.created = true;
                progress.task = Some(task);
                return Ok(Step::Done(json!({ "note": "already copying into it" })));
            }
            restart = true;
            // A finished copy, or `full`, copies everything again (keeping nothing of the old
            // record's place); a stopped one resumes where it stopped.
            if imp.copy.state != "done" && !p.full.unwrap_or(false) {
                let at = imp.copy.position.clone();
                resume = Some((imp, at));
            }
        }
        // The copy's place in the node's task slots, held from here until it ends.
        let permit = if dry {
            None
        } else {
            let owner = search_es::slots::owner_of(c.owner.as_deref(), &c.name);
            match self.node.reindex().acquire(&c.name, &owner) {
                Ok(p) => Some(p),
                Err(_) => {
                    return Ok(Step::Done(json!({ "queued": true, "reason": format!("the node's copy slots are taken (a cluster copies [{}] indices at a time, in turn with the node's other clusters)", self.node.reindex().max_per_cluster()) })));
                }
            }
        };
        let mut f = Fetch { client, host: &spec.label, held: search_es::breaker::Reservation::new(self.node.reindex().control_budget.clone()) };
        let mut plan = match spec.kind {
            Kind::Algolia => algolia::plan(&mut f, &p.index).await?,
            Kind::Typesense => typesense::plan(&mut f, &p.index).await?,
            _ => meili::plan(&mut f, &p.index).await?,
        };
        if !sane_path(&plan.id_field) || is_meta(&plan.id_field) {
            return Err(format!("[{}] has no primary key roll can use as _id", p.index));
        }
        plan.guard();
        let imported = Imported {
            from: from.clone(),
            id_field: plan.id_field.clone(),
            geo_points: plan.geo_points.clone(),
            copy: CopyState { state: "copying".into(), started_ms: now_ms(), done_ms: None, position: None },
            caught_up_ms: None,
            cutover: None,
            rule_templates: plan.rule_template_list(dest),
        };
        let mappings = plan.mappings(&imported);
        let summary = plan.summary(dest);
        // The analyzers are built as the index would build them (a synonym the chain can't
        // parse fails here, in the review, not at creation).
        if let Err(e) = self.check_analysis(c, &plan).await {
            if dry {
                return Ok(Step::Done(json!({ "plan": summary, "error": e })));
            }
            return Err(e);
        }
        drop(f);
        if dry {
            let mut v = json!({ "plan": summary, "mappings": mappings, "settings": plan.settings(dest), "restart": restart });
            if let Some((_, at)) = &resume {
                v["resume"] = json!(at.as_ref().map_or_else(|| "the start".to_string(), Position::describe));
            }
            return Ok(Step::Done(v));
        }
        // A restart that doesn't resume starts a new record (the copy's start is now).
        let resume = match (restart, resume) {
            (_, Some(r)) => Some(r),
            (true, None) => Some((imported, None)),
            (false, None) => None,
        };
        Ok(Step::Ready(Box::new(Ready { plan, mappings, summary, restart, resume, permit, index: p.index.clone(), dest: dest.to_string() })))
    }

    /// The rest of one index's migration, past the read: what it needs made on cluster `c`
    /// (undone if a step fails), then its copy started. Run in a task of its own, so neither
    /// the call's deadline nor a closed connection leaves it half made.
    async fn finish_one(&self, c: &SearchCluster, spec: &Spec, r: Ready, size: Option<u32>, rps: Option<f64>) -> (Value, bool, Option<String>) {
        let Ready { plan, mappings, summary, restart, resume, permit, index, dest } = r;
        let mut made = Vec::new();
        if !restart {
            let mut undo = Vec::new();
            if let Err(e) = self.make(c, &plan, &dest, &mappings, &mut made, &mut undo).await {
                // Nothing half made stays: a later attempt starts from scratch.
                for path in undo.iter().rev() {
                    self.call_json(c, "DELETE", path, None).await;
                }
                return (json!({ "plan": summary, "error": e }), false, None);
            }
        }
        // The record the copy keeps: a resumed copy's own, else the one the index was made with.
        let (meta, at) = match resume {
            Some((imp, at)) => (imp, at),
            None => (serde_json::from_value(mappings.pointer("/_meta/roll_import").cloned().unwrap_or_default()).unwrap_or_default(), None),
        };
        let resumed = at.as_ref().map(Position::describe);
        let reader = Reader::new(spec.kind, &index, at.clone(), None, plan.geo_points.clone());
        let started = match self.saas_client(spec, None).await {
            Ok(client) => {
                let copy = Copy {
                    reader,
                    client,
                    id_field: plan.id_field.clone(),
                    dest: dest.clone(),
                    size: size.unwrap_or(PAGE_DOCS).clamp(1, if spec.kind == Kind::Algolia { PAGE_DOCS } else { 10_000 }),
                    total: plan.total,
                    read: 0,
                    stated: plan.total,
                    mode: copy::Mode::Copy,
                    meta,
                    resume_at: at,
                };
                let description = format!("reindex from [{}][{index}] to [{dest}]", spec.label);
                self.start_copy(c, copy, description, rps, permit)
            }
            Err(e) => Err(e),
        };
        let task = match started {
            Ok(t) => t,
            Err(e) => return (json!({ "plan": summary, "made": made, "error": format!("[{dest}] is ready, but its copy didn't start: {e}; pick it again to start it") }), !restart, None),
        };
        let mut v = json!({ "plan": summary, "made": made, "dropped": [] });
        if let Some(r) = resumed {
            v["resumed"] = json!(r);
            v["note"] = json!(format!("resuming the copy that stopped, from {r}"));
        } else if restart {
            v["restarted"] = json!(true);
            v["note"] = json!("copying again into the index an earlier copy of it made: documents are overwritten by id, and none is deleted");
        }
        (v, !restart, Some(task))
    }

    /// Builds the search analyzer's chain with `_analyze` (the synonyms inline, as the set will
    /// hold them): the error Elasticsearch would give at index creation, if any.
    async fn check_analysis(&self, c: &SearchCluster, plan: &Plan) -> std::result::Result<(), String> {
        if plan.synonyms.is_empty() {
            return Ok(());
        }
        let (filters, _) = plan.chains(Some(json!({ "type": "synonym_graph", "synonyms": plan.synonyms.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>() })));
        let body = json!({ "tokenizer": "standard", "filter": filters, "text": "roll" });
        let (status, v) = self.call_json(c, "POST", "/_analyze", Some(&body)).await;
        if status == 200 {
            return Ok(());
        }
        let reason = v.pointer("/error/caused_by/reason").or(v.pointer("/error/reason")).and_then(Value::as_str).unwrap_or("roll refused it");
        Err(format!("the synonyms don't build in the search analyzer: {reason}"))
    }

    /// Makes what `plan` needs on cluster `c` for `dest`, in order: the synonyms set (the
    /// analyzer reads it), the index, the rulesets, the search templates. Names roll already
    /// has are never overwritten. What was made is listed in `made`, and how to remove it in
    /// `undo` (the caller does, if a later step fails).
    async fn make(&self, c: &SearchCluster, plan: &Plan, dest: &str, mappings: &Value, made: &mut Vec<String>, undo: &mut Vec<String>) -> std::result::Result<(), String> {
        let enc = |s: &str| urlencoding::encode(s).into_owned();
        let failed = |what: &str, v: &Value| format!("{what} failed: {}", v.pointer("/error/reason").and_then(Value::as_str).unwrap_or("roll refused it"));
        let set = Plan::synonyms_set(dest);
        let mut sets: Vec<(String, &[Value])> = Vec::new();
        for (names, rules) in [(plan.rulesets(dest), &plan.rules), (plan.filtered_rulesets(dest), &plan.filtered_rules)] {
            for (i, n) in names.into_iter().enumerate() {
                sets.push((n, &rules[i * RULES_PER_SET..((i + 1) * RULES_PER_SET).min(rules.len())]));
            }
        }
        let sources = plan.templates(dest);
        let templates: Vec<&String> = sources.iter().map(|(id, _)| id).collect();
        // Everything this index needs is free before anything is made.
        if !plan.synonyms.is_empty() && self.call_json(c, "GET", &format!("/_synonyms/{}?size=1", enc(&set)), None).await.0 == 200 {
            return Err(format!("synonyms set [{set}] already exists on roll: give the copy another name"));
        }
        for (s, _) in &sets {
            if self.call_json(c, "GET", &format!("/_query_rules/{}", enc(s)), None).await.0 == 200 {
                return Err(format!("query ruleset [{s}] already exists on roll: give the copy another name"));
            }
        }
        for t in &templates {
            if self.call_json(c, "GET", &format!("/_scripts/{}", enc(t)), None).await.0 == 200 {
                return Err(format!("search template [{t}] already exists on roll: give the copy another name"));
            }
        }
        if !plan.synonyms.is_empty() {
            let rules: Vec<Value> = plan.synonyms.iter().map(|(id, s)| json!({ "id": id, "synonyms": s })).collect();
            let (status, v) = self.call_json(c, "PUT", &format!("/_synonyms/{}", enc(&set)), Some(&json!({ "synonyms_set": rules }))).await;
            if status != 200 && status != 201 {
                return Err(failed(&format!("creating synonyms set [{set}]"), &v));
            }
            made.push(format!("synonyms set [{set}]"));
            undo.push(format!("/_synonyms/{}", enc(&set)));
        }
        let body = json!({ "settings": plan.settings(dest), "mappings": mappings });
        let (status, v) = self.call_json(c, "PUT", &format!("/{}", enc(dest)), Some(&body)).await;
        if status != 200 {
            return Err(failed(&format!("creating index [{dest}]"), &v));
        }
        made.push(format!("index [{dest}]"));
        undo.push(format!("/{}", enc(dest)));
        for (s, rules) in &sets {
            let (status, v) = self.call_json(c, "PUT", &format!("/_query_rules/{}", enc(s)), Some(&json!({ "rules": rules }))).await;
            if status != 200 && status != 201 {
                return Err(failed(&format!("creating query ruleset [{s}]"), &v));
            }
            made.push(format!("query ruleset [{s}]"));
            undo.push(format!("/_query_rules/{}", enc(s)));
        }
        for (id, source) in &sources {
            let body = json!({ "script": { "lang": "mustache", "source": source } });
            let (status, v) = self.call_json(c, "PUT", &format!("/_scripts/{}", enc(id)), Some(&body)).await;
            if status != 200 {
                return Err(failed(&format!("storing search template [{id}]"), &v));
            }
            made.push(format!("search template [{id}]"));
            undo.push(format!("/_scripts/{}", enc(id)));
        }
        Ok(())
    }

    /// Registers the copy as a reindex task of cluster `c` and runs it in the background
    /// (holding `permit`, the cluster's task slot, until it ends). The task's id.
    fn start_copy(&self, c: &SearchCluster, copy: Copy, description: String, rps: Option<f64>, permit: Option<remote::RemoteSlot>) -> Result<String> {
        let me = self.arc().ok_or_else(|| ControlError::Internal(anyhow::anyhow!("the control plane is shutting down")))?;
        let loaded = self.node.get_loaded(&c.name).ok_or_else(|| ControlError::Busy(format!("cluster {} isn't loaded on this node: try again", c.name)))?;
        let cluster = loaded.cluster.clone();
        let max = self.node.reindex().max_duration;
        let guard = cluster.tasks.register(ACTION, &description, None, Vec::new());
        let id = guard.detach();
        let flag = cluster.tasks.get(id).map(|t| t.cancelled).unwrap_or_default();
        let live = Live::for_task(id, rps.unwrap_or(-1.0), flag);
        cluster.tasks.set_live(id, live.clone());
        let task = format!("{}:{id}", cluster.node.id);
        let c = c.clone();
        tokio::spawn(async move {
            let guard = cluster.tasks.adopt(id);
            let started = std::time::Instant::now();
            let outcome = match tokio::time::timeout(max, copy.run(&me, &c, &live)).await {
                Ok(r) => r,
                Err(_) => {
                    live.update(|s| s.canceled = Some(format!("the copy ran past the node's limit of {}s", max.as_secs())));
                    Ok(())
                }
            };
            let outcome = outcome.map(|()| response(&live, started));
            cluster.tasks.record(&guard, outcome);
            drop(guard);
            drop(permit);
        });
        Ok(task)
    }
}

/// Algolia's and Meilisearch's `asc(x)` / `x:asc` sort, as a field and an order.
pub(crate) fn order(o: &str) -> Option<&'static str> {
    match o {
        "asc" => Some("asc"),
        "desc" => Some("desc"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_become_a_properties_tree() {
        let mut p = Plan::default();
        p.fields.insert("a.b".into(), Field::Keyword);
        p.fields.insert("a.c".into(), Field::Text { keyword: true });
        p.fields.insert("a".into(), Field::Keyword);
        let m = p.mappings(&Imported { from: "algolia:x".into(), ..Default::default() });
        assert_eq!(m["properties"]["a"]["properties"]["b"]["type"], "keyword");
        assert_eq!(m["properties"]["a"]["properties"]["c"]["fields"]["keyword"]["type"], "keyword");
        // `a` itself can't be a keyword and an object.
        assert_eq!(p.not_mapped.len(), 1, "{:?}", p.not_mapped);
        assert_eq!(m["_meta"]["roll_import"]["from"], "algolia:x");
    }

    #[test]
    fn a_spec_never_shows_its_key() {
        let s = Source { kind: Kind::Algolia, app_id: Some("APP1".into()), api_key: Some("s3cr3t-key-123".into()), ..Default::default() };
        let spec = spec_of(&s, None).unwrap();
        assert_eq!(spec.base, "https://app1-dsn.algolia.net:443");
        assert!(!format!("{spec:?}").contains("s3cr3t") && !spec.label.contains("s3cr3t"));
        let bad = Source { kind: Kind::Algolia, app_id: Some("a.b/c".into()), api_key: Some("k".into()), ..Default::default() };
        assert!(spec_of(&bad, None).is_err());
        let with_host = Source { kind: Kind::Algolia, app_id: Some("A".into()), host: "https://evil.example".into(), api_key: Some("k".into()), ..Default::default() };
        assert!(spec_of(&with_host, None).is_err());
    }
}
