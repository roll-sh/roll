//! Typesense: `GET /collections` (schemas), `GET /collections/{c}/documents/export` (JSONL),
//! `GET /collections/{c}/synonyms` and `…/overrides`, with `X-TYPESENSE-API-KEY`. See
//! `docs/search/guides/migrate-from-algolia.md` for what maps to what.

use super::{criteria, push_rules, sane_path, Fetch, Field, Listed, Plan, MAX_RULES, MAX_SYNONYMS};
use search_es::api::remote::Client;
use serde_json::{json, Value};

/// `GET /collections`: every collection and its document count.
pub(crate) async fn list(f: &mut Fetch<'_>) -> Result<Vec<Listed>, String> {
    let v = f.get("/collections").await?;
    let items = v.as_array().ok_or("Typesense's collection list isn't the shape roll reads")?;
    Ok(items.iter().filter_map(|c| Some(Listed { index: c["name"].as_str()?.to_string(), docs: c["num_documents"].as_i64(), size: None, replica_of: None })).collect())
}

/// The server's version (`GET /debug`), when the key may read it.
pub(crate) async fn version(c: &Client) -> Option<String> {
    let v = c.get("/debug").await.ok()?;
    v.value.get("version").and_then(search_es::json::Value::as_str).map(|s| s.chars().take(64).collect())
}

/// Turns a `geopoint`'s `[lat, lng]` at the dotted `field` into Elasticsearch's `{lat, lon}`
/// (its array form is `[lon, lat]`), arrays of them too, through objects and arrays of objects
/// on the way (`store.loc`, `stores[].loc`).
pub(crate) fn geo_point(doc: &mut search_es::json::Value, field: &str) {
    use search_es::json::{Map, Value as V};
    fn one(v: &mut V) {
        if let V::Arr(a) = v
            && a.len() == 2
            && a.iter().all(|x| matches!(x, V::Num(_)))
        {
            let mut m = Map::new();
            m.insert("lat".into(), a[0].clone());
            m.insert("lon".into(), a[1].clone());
            *v = V::Obj(m);
        }
    }
    fn at(v: &mut V, path: &[&str]) {
        match (v, path) {
            (V::Arr(a), _) if !path.is_empty() => a.iter_mut().for_each(|x| at(x, path)),
            (V::Obj(m), [first, rest @ ..]) => {
                let Some(v) = m.get_mut(*first) else { return };
                if rest.is_empty() {
                    match v {
                        V::Arr(a) if a.iter().all(|x| matches!(x, V::Arr(_))) => a.iter_mut().for_each(one),
                        v => one(v),
                    }
                } else {
                    at(v, rest);
                }
            }
            _ => {}
        }
    }
    let path: Vec<&str> = field.split('.').collect();
    at(doc, &path);
}

/// A Typesense field type as an Elasticsearch mapping (`None`: left to dynamic mapping).
fn mapping_of(ty: &str, f: &Value, p: &mut Plan, name: &str) -> Option<Field> {
    let base = ty.trim_end_matches("[]");
    let indexed = f["index"] != json!(false);
    let exact = f["facet"] == json!(true) || f["sort"] == json!(true);
    let typed = |t: &str| Some(Field::Typed(if indexed { json!({ "type": t }) } else { json!({ "type": t, "index": false }) }));
    match base {
        "string" if !indexed => Some(Field::Typed(json!({ "type": "keyword", "index": false, "doc_values": false }))),
        "string" => {
            if f["infix"] == json!(true) {
                p.skip("field", name, "infix search: query it with a wildcard, or add an ngram sub-field");
            }
            if f["locale"].as_str().is_some_and(|l| !l.is_empty() && l != "en") {
                p.skip("field", name, format!("locale [{}]: tokenized with the standard analyzer", f["locale"].as_str().unwrap_or_default()));
            }
            if f["stem"] == json!(true) {
                p.skip("field", name, "stem: words aren't stemmed (the index's analyzer folds case and accents only)");
            }
            Some(Field::Text { keyword: exact })
        }
        "int32" => typed("integer"),
        "int64" => typed("long"),
        "float" if f["num_dim"].as_u64().is_some() => {
            let dims = f["num_dim"].as_u64().unwrap_or(0);
            let sim = match f["vec_dist"].as_str().unwrap_or("cosine") {
                "ip" => "max_inner_product",
                _ => "cosine",
            };
            if !f["embed"].is_null() {
                p.notes.push(format!("[{name}]'s vectors are copied; Typesense's embedding model isn't (embed new documents with an inference endpoint)"));
            }
            Some(Field::Typed(json!({ "type": "dense_vector", "dims": dims, "similarity": sim, "index": true })))
        }
        "float" => typed("float"),
        "bool" => typed("boolean"),
        "geopoint" => {
            p.geo_points.push(name.to_string());
            typed("geo_point")
        }
        "object" => None,
        "auto" | "string*" => None,
        other => {
            p.skip("field", name, format!("type [{other}] has no mapping roll makes: left to dynamic mapping"));
            None
        }
    }
}

/// Reads collection `name`'s schema, synonyms and overrides into a plan.
pub(crate) async fn plan(f: &mut Fetch<'_>, name: &str) -> Result<Plan, String> {
    let e = urlencoding::encode(name).into_owned();
    let schema = f.get(&format!("/collections/{e}")).await?;
    let mut p = Plan { id_field: "id".into(), total: schema["num_documents"].as_u64(), ..Default::default() };
    p.fields.insert("id".into(), Field::Keyword);
    // Typesense searches the fields each request names (query_by): the template searches every
    // indexed string field, with Typesense's default typo and prefix behaviour.
    p.fuzziness = Some("AUTO:4,7".into());
    p.prefix = true;
    let mut not_stored = Vec::new();
    for fd in schema["fields"].as_array().into_iter().flatten() {
        let (Some(n), Some(ty)) = (fd["name"].as_str(), fd["type"].as_str()) else { continue };
        if n == "id" {
            continue;
        }
        if !sane_path(n) || n.contains(['*', '^', '$', '(', '[']) {
            p.skip("field", n, "a pattern field: matching fields are mapped dynamically");
            continue;
        }
        if fd["store"] == json!(false) {
            not_stored.push(n.to_string());
        }
        let Some(m) = mapping_of(ty, fd, &mut p, n) else { continue };
        let text = matches!(m, Field::Text { .. });
        p.fields.insert(n.to_string(), m);
        if text {
            p.default_fields.push(n.to_string());
        }
    }
    if !not_stored.is_empty() {
        p.notes.push(format!("fields Typesense doesn't store ({}) are left out of _source", not_stored.join(", ")));
        p.source_excludes = not_stored;
    }
    if let Some(d) = schema["default_sorting_field"].as_str().filter(|d| !d.is_empty()) {
        p.sort_after.push((p.exact(d), "desc"));
        p.notes.push(format!("default_sorting_field [{d}] is a sort after relevance in the search template"));
    }
    for k in ["token_separators", "symbols_to_index"] {
        if schema[k].as_array().is_some_and(|a| !a.is_empty()) {
            p.skip("setting", format!("{k}: {}", schema[k]), "the standard tokenizer splits words its own way");
        }
    }
    if !p.default_fields.is_empty() {
        p.notes.push("Typesense names the searched fields per request (query_by): the index's default fields are every indexed string field".into());
    }

    // Synonyms: the collection's own (Typesense 28 and older) and the synonym sets its schema
    // names (29 and later); curations likewise: its overrides and its curation sets (30).
    let mut syns: Vec<Value> = f.get_opt(&format!("/collections/{e}/synonyms")).await?.and_then(|v| v["synonyms"].as_array().cloned()).unwrap_or_default();
    for set in capped(&mut p, "synonym", names(&schema["synonym_sets"])) {
        match f.get_opt(&format!("/synonym_sets/{}", urlencoding::encode(&set))).await? {
            Some(v) => syns.extend(items(&v, &["items", "synonyms"])),
            None => p.skip("synonym", set, "the synonym set the schema names doesn't exist on the server"),
        }
    }
    if syns.len() > MAX_SYNONYMS {
        p.skip("synonym", format!("{} more", syns.len() - MAX_SYNONYMS), format!("an index brings at most {MAX_SYNONYMS} synonyms"));
    }
    for s in syns.iter().take(MAX_SYNONYMS) {
        synonym_into(&mut p, s);
    }
    let mut curations: Vec<Value> = f.get_opt(&format!("/collections/{e}/overrides")).await?.and_then(|v| v["overrides"].as_array().cloned()).unwrap_or_default();
    for set in capped(&mut p, "rule", names(&schema["curation_sets"])) {
        match f.get_opt(&format!("/curation_sets/{}", urlencoding::encode(&set))).await? {
            Some(v) => curations.extend(items(&v, &["items", "curations", "overrides"])),
            None => p.skip("rule", set, "the curation set the schema names doesn't exist on the server"),
        }
    }
    if curations.len() > MAX_RULES {
        p.skip("rule", format!("{} more", curations.len() - MAX_RULES), "an index brings at most 1,000 rules (10 rulesets of 100)");
    }
    for o in curations.iter().take(MAX_RULES) {
        override_into(&mut p, o);
    }
    Ok(p)
}

/// The names a schema field lists (`synonym_sets`, `curation_sets`).
fn names(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()
}

/// The most synonym or curation sets one schema brings.
const MAX_SETS: usize = 20;

/// The first [`MAX_SETS`] of a schema's sets; the rest are listed.
fn capped(p: &mut Plan, kind: &str, sets: Vec<String>) -> Vec<String> {
    if sets.len() > MAX_SETS {
        for extra in &sets[MAX_SETS..] {
            p.skip(kind, extra.clone(), format!("a collection brings at most {MAX_SETS} sets of each kind"));
        }
    }
    sets.into_iter().take(MAX_SETS).collect()
}

/// A set's entries, under whichever of `keys` the server's version uses (or the set itself, a list).
fn items(v: &Value, keys: &[&str]) -> Vec<Value> {
    if let Some(a) = v.as_array() {
        return a.clone();
    }
    keys.iter().find_map(|k| v[*k].as_array().cloned()).unwrap_or_default()
}

/// A Typesense synonym: multi-way, or one-way from its `root`.
fn synonym_into(p: &mut Plan, s: &Value) {
    let id = s["id"].as_str().unwrap_or_default().to_string();
    let esc = |t: &str| t.trim().replace('\\', "\\\\").replace(',', "\\,").replace("=>", "\\=>");
    let terms: Vec<String> = s["synonyms"].as_array().into_iter().flatten().filter_map(Value::as_str).map(esc).filter(|t| !t.is_empty()).collect();
    if s["locale"].as_str().is_some_and(|l| !l.is_empty()) || s["symbols_to_index"].as_array().is_some_and(|a| !a.is_empty()) {
        p.notes.push(format!("synonym [{id}]'s locale and symbols aren't kept"));
    }
    match s["root"].as_str().map(esc).filter(|r| !r.is_empty()) {
        Some(root) if !terms.is_empty() => p.synonyms.push((id, format!("{root} => {root}, {}", terms.join(", ")))),
        None if terms.len() > 1 => p.synonyms.push((id, terms.join(", "))),
        _ => p.skip("synonym", id, "it names fewer than two terms"),
    }
}

/// A Typesense override (curation): its rule as criteria on the query text, `includes` pinned
/// in position order, `excludes` excluded; filters, sorts and query rewrites are listed.
fn override_into(p: &mut Plan, o: &Value) {
    let id = o["id"].as_str().unwrap_or_default().to_string();
    let rule = &o["rule"];
    let mut crit = Vec::new();
    if let Some(q) = rule["query"].as_str() {
        if q.contains('{') {
            p.skip("rule", id, "a query with a {placeholder} (dynamic filtering) has no query rule equivalent");
            return;
        }
        let kind = if rule["match"].as_str() == Some("contains") { "contains" } else { "exact" };
        crit.push(criteria(kind, vec![q.to_lowercase()]));
    }
    if rule["filter_by"].is_string() || rule["tags"].as_array().is_some_and(|t| !t.is_empty()) {
        p.skip("rule", id, "a rule on filters or tags has no query rule equivalent");
        return;
    }
    if crit.is_empty() {
        crit.push(criteria("always", Vec::new()));
    }
    let mut includes: Vec<(u64, String)> = o["includes"].as_array().into_iter().flatten().filter_map(|i| Some((i["position"].as_u64().unwrap_or(1), i["id"].as_str()?.to_string()))).collect();
    includes.sort_by_key(|(pos, _)| *pos);
    if includes.iter().enumerate().any(|(i, (pos, _))| *pos != i as u64 + 1) {
        p.notes.push(format!("override [{id}] pins its documents at the top in its order, not at the positions Typesense gave them"));
    }
    let pins: Vec<String> = includes.into_iter().map(|(_, i)| i).collect();
    let hides: Vec<String> = o["excludes"].as_array().into_iter().flatten().filter_map(|e| e["id"].as_str().map(str::to_string)).collect();
    let mut rest = Vec::new();
    for (k, what) in [("filter_by", "a filter"), ("sort_by", "a sort"), ("replace_query", "a query rewrite"), ("metadata", "metadata"), ("effective_from_ts", "a validity period"), ("effective_to_ts", "a validity period")] {
        if !o[k].is_null() && !rest.contains(&what) {
            rest.push(what);
        }
    }
    if pins.is_empty() && hides.is_empty() {
        let why = if rest.is_empty() { "it neither includes nor excludes documents".to_string() } else { format!("query rules pin or exclude documents; this override only sets {}", rest.join(", ")) };
        p.skip("rule", id, why);
        return;
    }
    if !rest.is_empty() {
        p.skip("rule", id.clone(), format!("its includes and excludes are mapped, not {}", rest.join(", ")));
    }
    push_rules(p, &id, crit, pins, hides, false);
}
