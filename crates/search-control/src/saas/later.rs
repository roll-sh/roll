//! What comes after an import's first copy: resuming one that stopped, catching up with what
//! changed at the source since, and cutting over (an alias moved to the copy).
//!
//! Each copy keeps its record in its index's `_meta.roll_import` ([`Imported`]): where it came
//! from, its id field, how far the copy got (Algolia's browse cursor, Meilisearch's offset, the
//! Typesense export's lines; never a key), when it started, when the last catch-up started, and
//! the cutover. A copy saves its place at most every [`SAVE_EVERY`] while it runs and whenever it
//! stops; picking the index again resumes there.
//!
//! A catch-up reads the source again. With a `since_attribute` (a numeric attribute the
//! source's records set when they change) it reads only records changed since the copy (or the
//! last catch-up) started, less [`MARGIN_MS`] for clock skew: Algolia's `browse` with
//! `numericFilters`, Typesense's export with `filter_by`, Meilisearch's documents with `filter`.
//! Without one it reads everything. Either way each page is compared with what roll holds
//! (`_mget`) and only documents that differ are written. Records deleted at the source aren't
//! deleted on roll: none of the three says what it deleted.

use super::copy::{Copy, Mode, Reader};
use super::{kind_name, product, template_safe, Spec, PAGE_DOCS};
use crate::control::Control;
use crate::migrate::{Kind, MigrateRequest, Pick};
use crate::store::SearchCluster;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How often a running copy saves its place.
pub(crate) const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
/// How far back a catch-up reads before the last pass started (clocks differ).
pub(crate) const MARGIN_MS: i64 = 5 * 60 * 1000;

/// A copy's record in its index's `_meta.roll_import`.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub(crate) struct Imported {
    /// `<product>:<application or URL>:<index>`.
    pub from: String,
    #[serde(default)]
    pub id_field: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub geo_points: Vec<String>,
    #[serde(default)]
    pub copy: CopyState,
    /// When the last catch-up that finished started (ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caught_up_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutover: Option<Value>,
    /// Algolia rules that filter or rewrite the query: their pattern and their template.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_templates: Vec<Value>,
}

/// Where the copy stands.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub(crate) struct CopyState {
    /// `copying`, `stopped` or `done`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub started_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_ms: Option<i64>,
    /// Where a stopped copy resumes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
}

/// How far a copy got: the next page to read.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Position {
    /// Algolia's browse cursor (a place in the index, not a credential).
    Cursor(String),
    /// Meilisearch's offset.
    Offset(u64),
    /// Lines of the Typesense export already written.
    Lines(u64),
}

impl Position {
    pub(crate) fn describe(&self) -> String {
        match self {
            Position::Cursor(_) => "Algolia's browse cursor after the last page written".into(),
            Position::Offset(n) => format!("document {n}"),
            Position::Lines(n) => format!("line {n} of the export (the lines before it are read again, not written)"),
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// The source's filter for records whose `attr` is at least `t` (in the attribute's unit).
pub(crate) fn since_filter(kind: Kind, attr: &str, t: i64) -> String {
    match kind {
        Kind::Algolia => format!("{attr}>={t}"),
        Kind::Typesense => format!("{attr}:>={t}"),
        _ => format!("{attr} >= {t}"),
    }
}

impl Control {
    /// The import record of index `dest`, and its `_source` excludes (left out of comparisons).
    pub(crate) async fn read_import(&self, c: &SearchCluster, dest: &str) -> Option<(Imported, Vec<String>)> {
        let (status, v) = self.call_json(c, "GET", &format!("/{}/_mapping", urlencoding::encode(dest)), None).await;
        if status != 200 {
            return None;
        }
        let m = v.as_object()?.values().next()?.get("mappings")?;
        let imp: Imported = serde_json::from_value(m.pointer("/_meta/roll_import")?.clone()).ok()?;
        let excludes = m.pointer("/_source/excludes").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
        Some((imp, excludes))
    }

    /// Writes index `dest`'s import record (its `_meta`).
    pub(crate) async fn save_import(&self, c: &SearchCluster, dest: &str, imp: &Imported) -> bool {
        let body = json!({ "_meta": { "roll_import": imp } });
        self.call_json(c, "PUT", &format!("/{}/_mapping", urlencoding::encode(dest)), Some(&body)).await.0 == 200
    }

    /// `catch_up` for one index: its copy (finished, made from this source) read again from the
    /// source, only what changed written. The task's id.
    pub(super) async fn saas_catch_up(&self, c: &SearchCluster, spec: &Spec, req: &MigrateRequest, p: &Pick, dest: &str) -> Result<Value, String> {
        let from = format!("{}:{}:{}", kind_name(spec.kind), spec.key, p.index);
        let Some((imp, excludes)) = self.read_import(c, dest).await.filter(|(i, _)| i.from == from) else {
            return Err(format!("[{dest}] isn't a copy of {}'s [{}] on roll: copy it first", product(spec.kind), p.index));
        };
        if self.running_copy(c, dest).await.is_some() {
            return Err(format!("[{dest}] is being copied into: catch up once that ends"));
        }
        if imp.copy.state != "done" {
            return Err(format!("[{dest}]'s copy hasn't finished: pick it again to resume it, then catch up"));
        }
        let since_ms = imp.caught_up_ms.unwrap_or(imp.copy.started_ms) - MARGIN_MS;
        let filter = match &req.since_attribute {
            Some(a) if !template_safe(a) => return Err(format!("since_attribute [{a}] isn't an attribute name")),
            Some(a) => {
                let t = match req.since_unit.as_deref().unwrap_or("s") {
                    "s" => since_ms / 1000,
                    "ms" => since_ms,
                    u => return Err(format!("since_unit is s or ms, not {u}")),
                };
                Some(since_filter(spec.kind, a, t))
            }
            None => None,
        };
        let owner = search_es::slots::owner_of(c.owner.as_deref(), &c.name);
        let Ok(permit) = self.node.reindex().acquire(&c.name, &owner) else {
            return Ok(json!({ "queued": true, "reason": "the node's copy slots are taken: try again shortly" }));
        };
        let client = self.saas_client(spec, None).await.map_err(|e| e.to_string())?;
        let copy = Copy {
            reader: Reader::new(spec.kind, &p.index, None, filter.clone(), imp.geo_points.clone()),
            client,
            id_field: imp.id_field.clone(),
            dest: dest.to_string(),
            size: req.size.unwrap_or(PAGE_DOCS).clamp(1, if spec.kind == Kind::Algolia { PAGE_DOCS } else { 10_000 }),
            total: None,
            read: 0,
            stated: None,
            mode: Mode::CatchUp { started_ms: now_ms(), excludes },
            meta: imp,
            resume_at: None,
        };
        let description = format!("reindex from [{} (catch up)][{}] to [{dest}]", spec.label, p.index);
        let task = self.start_copy(c, copy, description, req.requests_per_second, Some(permit)).map_err(|e| e.to_string())?;
        Ok(json!({
            "task": task,
            "since_ms": since_ms,
            "method": if filter.is_some() { "since_attribute" } else { "compare" },
            "note": if filter.is_some() { "reads the records changed since then; records deleted at the source stay on roll" } else { "reads every record and writes the ones that differ; records deleted at the source stay on roll" },
        }))
    }

    /// `cutover` for one index: `alias` (the source's index name unless set) moved to the copy
    /// in one step, from whatever held it.
    pub(super) async fn saas_cutover(&self, c: &SearchCluster, spec: &Spec, p: &Pick, dest: &str) -> Result<Value, String> {
        let from = format!("{}:{}:{}", kind_name(spec.kind), spec.key, p.index);
        let Some((mut imp, _)) = self.read_import(c, dest).await.filter(|(i, _)| i.from == from) else {
            return Err(format!("[{dest}] isn't a copy of {}'s [{}] on roll", product(spec.kind), p.index));
        };
        if self.running_copy(c, dest).await.is_some() {
            return Err(format!("[{dest}] is being copied into: cut over once that ends"));
        }
        if imp.copy.state != "done" {
            return Err(format!("[{dest}]'s copy hasn't finished: pick it again to resume it first"));
        }
        let alias = p.alias.clone().filter(|a| !a.trim().is_empty()).unwrap_or_else(|| p.index.clone());
        if alias == dest {
            return Err(format!("the alias can't be [{dest}] itself: name another"));
        }
        let enc = urlencoding::encode(&alias).into_owned();
        let (status, held) = self.call_json(c, "GET", &format!("/_alias/{enc}"), None).await;
        let holders: Vec<String> = if status == 200 { held.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default() } else { Vec::new() };
        if holders.is_empty() && self.call_json(c, "HEAD", &format!("/{enc}"), None).await.0 == 200 {
            return Err(format!("[{alias}] is an index on roll, not an alias: name another alias"));
        }
        let mut actions: Vec<Value> = holders.iter().filter(|h| *h != dest).map(|h| json!({ "remove": { "index": h, "alias": alias } })).collect();
        actions.push(json!({ "add": { "index": dest, "alias": alias, "is_write_index": true } }));
        let (status, v) = self.call_json(c, "POST", "/_aliases", Some(&json!({ "actions": actions }))).await;
        if status != 200 {
            return Err(format!("moving alias [{alias}] failed: {}", v.pointer("/error/reason").and_then(Value::as_str).unwrap_or("roll refused it")));
        }
        let previous: Vec<&String> = holders.iter().filter(|h| *h != dest).collect();
        imp.cutover = Some(json!({ "alias": alias, "ms": now_ms(), "previous": previous }));
        let caught = imp.caught_up_ms;
        let saved = self.save_import(c, dest, &imp).await;
        let mut row = json!({ "alias": alias, "previous": previous, "caught_up_ms": caught, "copy_started_ms": imp.copy.started_ms });
        row["note"] = json!(format!(
            "[{alias}] now points at [{dest}]. Writes made at the source after {} aren't on roll: stop writing there, catch up once more, then use roll",
            if caught.is_some() { "the last catch-up started" } else { "the copy started" }
        ));
        if !saved {
            row["warning"] = json!("the alias moved, but the cutover couldn't be recorded on the index");
        }
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_source_reads_what_changed_its_own_way() {
        assert_eq!(since_filter(Kind::Algolia, "updated_at", 1700000000), "updated_at>=1700000000");
        assert_eq!(since_filter(Kind::Typesense, "updated_at", 1700000000), "updated_at:>=1700000000");
        assert_eq!(since_filter(Kind::Meilisearch, "updated_at", 1700000000), "updated_at >= 1700000000");
        let imp: Imported = serde_json::from_value(json!({ "from": "algolia:app:x", "copy": { "state": "stopped", "started_ms": 5, "position": { "cursor": "c1" } } })).unwrap();
        assert_eq!(imp.copy.position, Some(Position::Cursor("c1".into())));
        // A copy made before records were kept reads as one with nothing to resume.
        let old: Imported = serde_json::from_value(json!({ "from": "algolia:app:x" })).unwrap();
        assert_eq!((old.copy.state.as_str(), old.copy.position), ("", None));
    }
}
