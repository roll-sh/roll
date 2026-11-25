//! Meilisearch: `GET /indexes`, `GET /stats`, `GET /indexes/{uid}/settings`,
//! `GET /indexes/{uid}/documents?offset&limit`, with `Authorization: Bearer`. See
//! `docs/search/guides/migrate-from-algolia.md` for what maps to what.

use super::{order, sane_path, Fetch, Field, Listed, Plan, Sample, MAX_SYNONYMS, SAMPLE};
use search_es::api::remote::Client;
use serde_json::{json, Value};

/// Meilisearch's default ranking rules.
const DEFAULT_RULES: &[&str] = &["words", "typo", "proximity", "attribute", "sort", "exactness"];

/// `GET /indexes` (paged) and `GET /stats`: every index and its document count.
pub(crate) async fn list(f: &mut Fetch<'_>, small: &Client) -> Result<Vec<Listed>, String> {
    let mut out = Vec::new();
    let mut offset = 0;
    loop {
        let v = f.get(&format!("/indexes?offset={offset}&limit=1000")).await?;
        let rows = v["results"].as_array().ok_or("Meilisearch's index list isn't the shape roll reads")?;
        for r in rows {
            if let Some(uid) = r["uid"].as_str() {
                out.push(Listed { index: uid.to_string(), docs: None, size: None, replica_of: None });
            }
        }
        offset += rows.len();
        if rows.len() < 1000 || offset >= crate::migrate::MAX_LISTED || offset as u64 >= v["total"].as_u64().unwrap_or(0) {
            break;
        }
    }
    // Counts are a nicety: a key that can't read `/stats` still lists.
    if let Ok(stats) = small.get("/stats").await {
        let idx = stats.value.get("indexes");
        for l in &mut out {
            let s = idx.and_then(|m| m.get(&l.index));
            l.docs = s.and_then(|s| s.get("numberOfDocuments")).and_then(search_es::json::Value::as_number).and_then(|n| n.as_i64());
            l.size = s.and_then(|s| s.get("rawDocumentDbSize")).and_then(search_es::json::Value::as_number).and_then(|n| n.as_i64());
        }
    }
    Ok(out)
}

/// The server's version (`GET /version`).
pub(crate) async fn version(c: &Client) -> Option<String> {
    let v = c.get("/version").await.ok()?;
    v.value.get("pkgVersion").and_then(search_es::json::Value::as_str).map(|s| s.chars().take(64).collect())
}

/// Attribute names of a list setting: strings, or `{attributePatterns}` objects (1.13+).
fn names(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for x in v.as_array().into_iter().flatten() {
        match x {
            Value::String(s) => out.push(s.clone()),
            Value::Object(m) => out.extend(m.get("attributePatterns").and_then(Value::as_array).into_iter().flatten().filter_map(|p| p.as_str().map(str::to_string))),
            _ => {}
        }
    }
    out
}

/// Reads index `uid`'s primary key, settings and a few documents into a plan.
pub(crate) async fn plan(f: &mut Fetch<'_>, uid: &str) -> Result<Plan, String> {
    let e = urlencoding::encode(uid).into_owned();
    let info = f.get(&format!("/indexes/{e}")).await?;
    let pk = info["primaryKey"].as_str().ok_or_else(|| format!("[{uid}] has no primary key yet (Meilisearch sets one with the first documents)"))?.to_string();
    let s = f.get(&format!("/indexes/{e}/settings")).await?;
    let first = f.get(&format!("/indexes/{e}/documents?offset=0&limit={SAMPLE}")).await?;
    let sample = Sample(first["results"].as_array().cloned().unwrap_or_default());
    let mut p = Plan { id_field: pk.clone(), total: first["total"].as_u64(), ..Default::default() };
    if sample.holds_text(&pk) || sample.0.is_empty() {
        p.fields.insert(pk.clone(), Field::Keyword);
    }

    let searchable = names(&s["searchableAttributes"]);
    if searchable != ["*"] {
        let n = searchable.len() as u32;
        for (rank, a) in searchable.iter().enumerate() {
            if sane_path(a) && !a.contains('*') {
                p.searchable(a, n - rank as u32, &sample);
            } else {
                p.skip("setting", format!("searchableAttributes: {a}"), "not an attribute name roll can map");
            }
        }
        p.notes.push("searchableAttributes are the index's default fields (index.query.default_field), the first weighted most".into());
    }
    for key in ["filterableAttributes", "sortableAttributes"] {
        for a in names(&s[key]) {
            if sane_path(a.as_str()) && !a.contains('*') {
                p.keyword(&a, &sample);
            } else {
                p.skip("setting", format!("{key}: {a}"), "a pattern: matching attributes are mapped dynamically (strings get a keyword sub-field)");
            }
        }
    }
    let rules: Vec<String> = s["rankingRules"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(str::to_string)).collect();
    let mut text_rules = Vec::new();
    for r in &rules {
        match r.rsplit_once(':').and_then(|(a, o)| Some((a, order(o)?))) {
            Some((a, o)) if sane_path(a) => {
                p.keyword(a, &sample);
                p.sort_after.push((p.exact(a), o));
            }
            _ if r == "sort" => {}
            _ => text_rules.push(r.as_str()),
        }
    }
    if !rules.is_empty() && !text_rules.is_empty() {
        let list = if text_rules == DEFAULT_RULES.iter().copied().filter(|r| *r != "sort").collect::<Vec<_>>() { "the default ranking rules".to_string() } else { text_rules.join(", ") };
        p.skip("setting", format!("rankingRules: {}", text_rules.join(", ")), format!("{list} have no Elasticsearch equivalent: roll ranks by BM25 relevance, then custom rules as a sort"));
    }

    let typo = &s["typoTolerance"];
    if typo["enabled"] != json!(false) {
        let one = typo.pointer("/minWordSizeForTypos/oneTypo").and_then(Value::as_u64).unwrap_or(5);
        let two = typo.pointer("/minWordSizeForTypos/twoTypos").and_then(Value::as_u64).unwrap_or(9);
        p.fuzziness = Some(format!("AUTO:{one},{two}"));
        for (k, why) in [("disableOnWords", "fuzziness applies to every word"), ("disableOnAttributes", "fuzziness applies to every field the template searches")] {
            if typo[k].as_array().is_some_and(|a| !a.is_empty()) {
                p.skip("setting", format!("typoTolerance.{k}: {}", typo[k]), why);
            }
        }
        if typo["disableOnNumbers"] == json!(true) {
            p.skip("setting", "typoTolerance.disableOnNumbers", "fuzziness applies to numbers in a query too");
        }
    }
    p.prefix = s["prefixSearch"].as_str() != Some("disabled");

    let stop: Vec<String> = s["stopWords"].as_array().into_iter().flatten().filter_map(|w| w.as_str().map(str::to_lowercase)).collect();
    if !stop.is_empty() {
        p.stop = stop;
    }
    let mut syns = 0;
    for (word, to) in s["synonyms"].as_object().into_iter().flatten() {
        let esc = |t: &str| t.trim().replace('\\', "\\\\").replace(',', "\\,").replace("=>", "\\=>");
        let to: Vec<String> = to.as_array().into_iter().flatten().filter_map(Value::as_str).map(esc).filter(|t| !t.is_empty()).collect();
        if to.is_empty() {
            continue;
        }
        syns += 1;
        if syns > MAX_SYNONYMS {
            p.skip("synonym", word, format!("an index brings at most {MAX_SYNONYMS} synonyms"));
            continue;
        }
        let w = esc(word);
        p.synonyms.push((format!("meili-{syns}"), format!("{w} => {w}, {}", to.join(", "))));
    }
    if let Some(d) = s["distinctAttribute"].as_str().filter(|d| sane_path(d)) {
        p.keyword(d, &sample);
        p.collapse = Some((p.exact(d), 1));
        p.notes.push(format!("distinctAttribute [{d}] is a field collapse in the search template"));
    }
    let shown = names(&s["displayedAttributes"]);
    if !shown.is_empty() && shown != ["*"] {
        p.retrieve = shown;
    }
    for (k, default) in [
        ("separatorTokens", json!([])),
        ("nonSeparatorTokens", json!([])),
        ("dictionary", json!([])),
        ("proximityPrecision", json!("byWord")),
        ("embedders", json!({})),
        ("localizedAttributes", Value::Null),
        ("searchCutoffMs", Value::Null),
    ] {
        let v = &s[k];
        if !v.is_null() && v != &default {
            let text = v.to_string();
            p.skip("setting", format!("{k}: {}", if text.len() > 80 { format!("[{} bytes]", text.len()) } else { text }), "no Elasticsearch equivalent roll maps it to");
        }
    }
    Ok(p)
}
