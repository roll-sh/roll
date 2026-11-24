//! Algolia (Search API v1): `GET /1/indexes`, `GET /1/indexes/{i}/settings`, `POST …/browse`,
//! `POST …/synonyms/search`, `POST …/rules/search`, with `X-Algolia-Application-Id` and
//! `X-Algolia-API-Key`. See `docs/search/guides/migrate-from-algolia.md` for what maps to what.

use super::{criteria, order, plural_stemmer, push_rules, sane_path, stop_list, Fetch, Field, Listed, Plan, Sample, MAX_REPLICAS, MAX_RULES, MAX_SYNONYMS, SAMPLE};
use serde_json::{json, Value};

/// Algolia's default `ranking`.
const DEFAULT_RANKING: &[&str] = &["typo", "geo", "words", "filters", "proximity", "attribute", "exact", "custom"];
/// Settings that change how results look, not which match or how they rank (not listed).
const DISPLAY_ONLY: &[&str] = &[
    "attributesToHighlight",
    "attributesToSnippet",
    "highlightPreTag",
    "highlightPostTag",
    "snippetEllipsisText",
    "restrictHighlightAndSnippetArrays",
    "maxValuesPerFacet",
    "sortFacetValuesBy",
    "responseFields",
    "renderingContent",
    "paginationLimitedTo",
    "version",
    "primary",
    "userData",
    "maxFacetHits",
    "replaceSynonymsInHighlight",
    "attributeCriteriaComputedByMinProximity",
    "numericAttributesForFiltering",
    "allowCompressionOfIntegerArray",
    "enableRules",
    "enablePersonalization",
    "enableReRanking",
    "personalizationImpact",
    "minProximity",
];
/// Settings read into the plan (the rest, when set to something, are listed as not mapped).
const READ: &[&str] = &[
    "searchableAttributes",
    "attributesToIndex",
    "attributesForFaceting",
    "customRanking",
    "ranking",
    "typoTolerance",
    "minWordSizefor1Typo",
    "minWordSizefor2Typos",
    "ignorePlurals",
    "removeStopWords",
    "queryLanguages",
    "indexLanguages",
    "unretrievableAttributes",
    "attributesToRetrieve",
    "distinct",
    "attributeForDistinct",
    "replicas",
    "queryType",
    "hitsPerPage",
    "disableTypoToleranceOnAttributes",
    "disableTypoToleranceOnWords",
    "allowTyposOnNumericTokens",
];

/// An index name in a path.
pub(crate) fn enc(index: &str) -> String {
    urlencoding::encode(index).into_owned()
}

/// `GET /1/indexes`: every index, with its record count and size; a replica names its primary.
pub(crate) async fn list(f: &mut Fetch<'_>) -> Result<Vec<Listed>, String> {
    let v = f.get("/1/indexes").await?;
    let items = v["items"].as_array().ok_or("Algolia's index list isn't the shape roll reads")?;
    Ok(items
        .iter()
        .filter_map(|i| {
            Some(Listed {
                index: i["name"].as_str()?.to_string(),
                docs: i["entries"].as_i64(),
                size: i["dataSize"].as_i64(),
                replica_of: i["primary"].as_str().map(str::to_string),
            })
        })
        .collect())
}

/// `searchable(x)`, `filterOnly(x)`, `unordered(x)`… as the modifier and the attribute.
fn modifier(s: &str) -> (Option<&str>, &str) {
    match s.find('(') {
        Some(at) if s.ends_with(')') => (Some(&s[..at]), &s[at + 1..s.len() - 1]),
        _ => (None, s),
    }
}

/// `asc(price)` / `desc(price)`.
fn ranked(s: &str) -> Option<(String, &'static str)> {
    let (m, attr) = modifier(s);
    Some((attr.to_string(), order(m?)?))
}

/// Strings of a list setting.
fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()
}

/// Languages a setting that is `true` or a list of codes names (`true`: the index's query
/// languages, English when it has none).
fn languages(v: &Value, settings: &Value) -> Vec<String> {
    match v {
        Value::Bool(true) => {
            let q = strings(&settings["queryLanguages"]);
            if q.is_empty() { vec!["en".to_string()] } else { q }
        }
        Value::Array(_) => strings(v),
        _ => Vec::new(),
    }
}

/// Every page of a `…/search` endpoint (synonyms, rules), at most `max` hits; the source's total.
async fn all(f: &mut Fetch<'_>, path: &str, max: usize) -> Result<(Vec<Value>, u64), String> {
    let mut out = Vec::new();
    let mut total = 0;
    for page in 0.. {
        let v = f.post(path, &json!({ "query": "", "page": page, "hitsPerPage": 1000 })).await?;
        total = v["nbHits"].as_u64().unwrap_or(total);
        let hits = v["hits"].as_array().cloned().unwrap_or_default();
        let n = hits.len();
        out.extend(hits.into_iter().take(max - out.len()));
        if n < 1000 || out.len() >= max {
            break;
        }
    }
    let total = total.max(out.len() as u64);
    Ok((out, total))
}

/// Reads index `index`'s settings, a few records, its synonyms, rules and replicas' sorts into a
/// plan.
pub(crate) async fn plan(f: &mut Fetch<'_>, index: &str) -> Result<Plan, String> {
    let e = enc(index);
    let settings = f.get(&format!("/1/indexes/{e}/settings?getVersion=2")).await?;
    let first = f.post(&format!("/1/indexes/{e}/browse"), &json!({ "hitsPerPage": SAMPLE })).await?;
    let sample = Sample(first["hits"].as_array().cloned().unwrap_or_default());
    let mut p = Plan { id_field: "objectID".into(), total: first["nbHits"].as_u64(), ..Default::default() };
    p.fields.insert("objectID".into(), Field::Keyword);
    settings_into(&mut p, &settings, &sample);

    let (syns, total) = all(f, &format!("/1/indexes/{e}/synonyms/search"), MAX_SYNONYMS).await?;
    if total > syns.len() as u64 {
        p.skip("synonym", format!("{} more", total - syns.len() as u64), format!("an index brings at most {MAX_SYNONYMS} synonyms"));
    }
    for s in &syns {
        synonym_into(&mut p, s);
    }
    let (rules, total) = all(f, &format!("/1/indexes/{e}/rules/search"), MAX_RULES).await?;
    if total > rules.len() as u64 {
        p.skip("rule", format!("{} more", total - rules.len() as u64), "an index brings at most 1,000 rules (10 rulesets of 100)");
    }
    for r in &rules {
        rule_into(&mut p, r);
    }

    for (n, replica) in strings(&settings["replicas"]).iter().enumerate() {
        let (virt, name) = match modifier(replica) {
            (Some("virtual"), n) => (true, n.to_string()),
            _ => (false, replica.clone()),
        };
        if n >= MAX_REPLICAS {
            p.skip("replica", name, format!("an index brings the sorts of at most {MAX_REPLICAS} replicas"));
            continue;
        }
        let Some(rs) = f.get_opt(&format!("/1/indexes/{}/settings", enc(&name))).await? else {
            p.skip("replica", name, "its settings couldn't be read");
            continue;
        };
        // A standard replica sorts with `ranking` led by `asc(…)`/`desc(…)`; a virtual one
        // re-ranks with `customRanking`.
        let lead: Vec<(String, &'static str)> = if virt { strings(&rs["customRanking"]).iter().filter_map(|s| ranked(s)).collect() } else { strings(&rs["ranking"]).iter().map_while(|s| ranked(s)).collect() };
        if lead.is_empty() {
            p.skip("replica", name, "it sorts no differently from the primary (no asc() or desc() to turn into a sort)");
            continue;
        }
        let sort: Vec<(String, &'static str)> = lead.into_iter().map(|(a, o)| (p.exact(&a), o)).collect();
        for (a, _) in &sort {
            let base = a.trim_end_matches(".keyword").to_string();
            p.keyword(&base, &sample);
        }
        let words: Vec<String> = sort.iter().map(|(a, o)| format!("{a} {o}")).collect();
        p.notes.push(format!(
            "{} replica [{name}] is a sort on this index ({}), not an index: search with its search template",
            if virt { "virtual" } else { "standard" },
            words.join(", ")
        ));
        p.replicas.push((name, sort));
    }
    Ok(p)
}

/// Algolia's settings into the plan.
fn settings_into(p: &mut Plan, s: &Value, sample: &Sample) {
    // Searchable attributes in priority order: the first counts most (`a,b` share a rank).
    let searchable = if s["searchableAttributes"].is_array() { strings(&s["searchableAttributes"]) } else { strings(&s["attributesToIndex"]) };
    let n = searchable.len() as u32;
    for (rank, entry) in searchable.iter().enumerate() {
        for one in entry.split(',').map(str::trim).filter(|x| !x.is_empty()) {
            let (m, attr) = modifier(one);
            if !sane_path(attr) {
                p.skip("setting", format!("searchableAttributes: {one}"), "not an attribute name roll can map");
                continue;
            }
            if m.is_some_and(|m| m != "unordered") {
                p.skip("setting", format!("searchableAttributes: {one}"), format!("the {} modifier has no Elasticsearch equivalent: searched as {attr}", m.unwrap_or_default()));
            }
            p.searchable(attr, n - rank as u32, sample);
        }
    }
    if !searchable.is_empty() {
        p.notes.push("searchableAttributes are the index's default fields (index.query.default_field), the first weighted most; Algolia's attribute and position criteria aren't BM25's".into());
    }
    for facet in strings(&s["attributesForFaceting"]) {
        let (m, attr) = modifier(&facet);
        if !sane_path(attr) || attr == "*" {
            p.skip("setting", format!("attributesForFaceting: {facet}"), "not an attribute name roll can map");
            continue;
        }
        if m == Some("searchable") {
            p.notes.push(format!("[{attr}] is facetable: aggregate on {} (facet search with a prefix query on it)", p.exact(attr)));
        }
        if m == Some("afterDistinct") {
            p.skip("setting", format!("attributesForFaceting: {facet}"), "facets are counted over every match, not after distinct");
        }
        p.keyword(attr, sample);
    }

    // `ranking`: asc()/desc() before the textual criteria sort first; the criteria themselves
    // are Algolia's tie-breaking, which BM25 has no equivalent of.
    let ranking = strings(&s["ranking"]);
    let mut criteria_seen = false;
    for r in &ranking {
        match ranked(r) {
            Some((attr, o)) if !criteria_seen => {
                p.keyword(&attr, sample);
                p.sort_first.push((p.exact(&attr), o));
            }
            Some(_) => p.skip("setting", format!("ranking: {r}"), "a sort after the textual criteria has no place in a score-then-sort order"),
            None => criteria_seen = true,
        }
    }
    let criteria: Vec<&str> = ranking.iter().map(String::as_str).filter(|r| ranked(r).is_none()).collect();
    let custom = if criteria.is_empty() { DEFAULT_RANKING.to_vec() } else { criteria };
    let tie: Vec<&str> = custom.iter().copied().filter(|c| *c != "custom").collect();
    if !tie.is_empty() {
        p.skip("setting", format!("ranking: {}", tie.join(", ")), "Algolia's tie-breaking criteria have no Elasticsearch equivalent: roll ranks by BM25 relevance, then customRanking as a sort");
    }
    for c in strings(&s["customRanking"]) {
        match ranked(&c) {
            Some((attr, o)) if sane_path(&attr) => {
                p.keyword(&attr, sample);
                p.sort_after.push((p.exact(&attr), o));
            }
            _ => p.skip("setting", format!("customRanking: {c}"), "only asc(attribute) and desc(attribute) map, as a sort after relevance"),
        }
    }
    if !p.sort_after.is_empty() {
        p.notes.push("customRanking is a sort after relevance (_score) in the search template: ties of BM25 scores are rarer than Algolia's buckets, so it decides less often".into());
    }

    // Typos.
    let one = s["minWordSizefor1Typo"].as_u64().unwrap_or(4);
    let two = s["minWordSizefor2Typos"].as_u64().unwrap_or(8);
    let off = s["typoTolerance"] == json!(false) || s["typoTolerance"] == json!("false");
    match s["typoTolerance"].as_str() {
        _ if off => {}
        Some(m @ ("min" | "strict")) => {
            p.fuzziness = Some(format!("AUTO:{one},{two}"));
            p.skip("setting", format!("typoTolerance: {m}"), format!("Elasticsearch has no {m} mode: matches with typos rank by BM25 like the rest (fuzziness AUTO:{one},{two})"));
        }
        _ => p.fuzziness = Some(format!("AUTO:{one},{two}")),
    }
    for (key, why) in [("disableTypoToleranceOnAttributes", "fuzziness applies to every field the template searches"), ("disableTypoToleranceOnWords", "fuzziness applies to every word")] {
        let v = strings(&s[key]);
        if !v.is_empty() {
            p.skip("setting", format!("{key}: {}", v.join(", ")), why);
        }
    }
    if s["allowTyposOnNumericTokens"] == json!(false) {
        p.skip("setting", "allowTyposOnNumericTokens: false", "fuzziness applies to numbers in a query too");
    }
    match s["queryType"].as_str().unwrap_or("prefixLast") {
        "prefixLast" => p.prefix = true,
        "prefixNone" => {}
        other => {
            p.prefix = true;
            p.skip("setting", format!("queryType: {other}"), "only the last word matches as a prefix (bool_prefix)");
        }
    }

    // Plurals and stop words.
    let plurals = languages(&s["ignorePlurals"], s);
    for (i, l) in plurals.iter().enumerate() {
        match plural_stemmer(l) {
            Some(st) if i == 0 => p.stemmer = Some(st.to_string()),
            Some(_) => p.skip("setting", format!("ignorePlurals: {l}"), "one language's plurals per index: the first listed is used"),
            None => p.skip("setting", format!("ignorePlurals: {l}"), "Elasticsearch has no plural stemmer for this language"),
        }
    }
    for l in languages(&s["removeStopWords"], s) {
        match stop_list(&l) {
            Some(list) => p.stop.push(list.to_string()),
            None => p.skip("setting", format!("removeStopWords: {l}"), "Elasticsearch has no stop word list for this language"),
        }
    }

    // What's returned.
    let hidden = strings(&s["unretrievableAttributes"]);
    if !hidden.is_empty() {
        p.notes.push(format!("unretrievableAttributes ({}) are left out of _source: searchable, never returned, and not copyable again from roll", hidden.join(", ")));
        p.source_excludes = hidden;
    }
    let shown = strings(&s["attributesToRetrieve"]);
    if !shown.is_empty() && shown != ["*"] {
        p.retrieve = shown;
    }
    if let Some(n) = s["hitsPerPage"].as_u64().filter(|n| *n != 20 && *n > 0) {
        p.size = Some(n.min(1000) as u32);
    }

    // Distinct.
    let distinct = match &s["distinct"] {
        Value::Bool(true) => 1,
        Value::Number(n) => n.as_u64().unwrap_or(0) as u32,
        _ => 0,
    };
    if let Some(attr) = s["attributeForDistinct"].as_str().filter(|a| distinct > 0 && sane_path(a)) {
        p.keyword(attr, sample);
        p.collapse = Some((p.exact(attr), distinct));
        p.notes.push(format!("distinct on [{attr}] is a field collapse in the search template{}", if distinct > 1 { format!(" with the first {distinct} of each group in inner_hits") } else { String::new() }));
    }

    // Everything else that's set and could change what matches or how it ranks.
    if let Some(m) = s.as_object() {
        for (k, v) in m {
            if READ.contains(&k.as_str()) || DISPLAY_ONLY.contains(&k.as_str()) || v.is_null() || v == &json!([]) || v == &json!(false) || v == &json!("") {
                continue;
            }
            let text = v.to_string();
            let shown = if text.chars().count() > 80 { format!("{}…", text.chars().take(80).collect::<String>()) } else { text };
            p.skip("setting", format!("{k}: {shown}"), "no Elasticsearch equivalent roll maps it to");
        }
    }
}

/// One Algolia synonym into Solr's format (one-way synonyms keep the input matching itself).
fn synonym_into(p: &mut Plan, s: &Value) {
    let id = s["objectID"].as_str().unwrap_or_default().to_string();
    let esc = |t: &str| t.trim().replace('\\', "\\\\").replace(',', "\\,").replace("=>", "\\=>");
    let list = |v: &Value| strings(v).iter().map(|t| esc(t)).filter(|t| !t.is_empty()).collect::<Vec<_>>();
    match s["type"].as_str().unwrap_or_default() {
        "synonym" => {
            let terms = list(&s["synonyms"]);
            if terms.len() > 1 {
                p.synonyms.push((id, terms.join(", ")));
            }
        }
        "onewaysynonym" => {
            let input = esc(s["input"].as_str().unwrap_or_default());
            let to = list(&s["synonyms"]);
            if !input.is_empty() && !to.is_empty() {
                p.synonyms.push((id, format!("{input} => {input}, {}", to.join(", "))));
            }
        }
        t @ ("altcorrection1" | "altcorrection2") => {
            let word = esc(s["word"].as_str().unwrap_or_default());
            let to = list(&s["corrections"]);
            if !word.is_empty() && !to.is_empty() {
                p.synonyms.push((id.clone(), format!("{word} => {word}, {}", to.join(", "))));
                p.notes.push(format!("{t} [{id}] is a one-way synonym: its corrections aren't ranked as typos"));
            }
        }
        "placeholder" => p.skip("synonym", id, "placeholders match a token in records, which no Elasticsearch synonym does"),
        t => p.skip("synonym", id, format!("unknown synonym type [{t}]")),
    }
}

/// One Algolia rule into Elasticsearch query rules: its conditions as criteria on the query
/// text (and the search's `context`), `promote` as a pinned rule, `hide` as an exclude rule.
/// What else it does (filters, query edits, user data, validity) is listed.
fn rule_into(p: &mut Plan, r: &Value) {
    let id = r["objectID"].as_str().unwrap_or_default().to_string();
    if r["enabled"] == json!(false) {
        p.skip("rule", id, "disabled on Algolia");
        return;
    }
    let mut conds: Vec<&Value> = r["conditions"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    if conds.is_empty() && r["condition"].is_object() {
        conds.push(&r["condition"]);
    }
    let mut crit_sets: Vec<Vec<Value>> = Vec::new();
    for c in &conds {
        let mut crit = Vec::new();
        if let Some(pattern) = c["pattern"].as_str() {
            if pattern.contains('{') {
                p.skip("rule", id, "a pattern with a {facet:…} placeholder has no query rule equivalent");
                return;
            }
            let kind = match c["anchoring"].as_str().unwrap_or("is") {
                "is" => "exact",
                "startsWith" => "prefix",
                "endsWith" => "suffix",
                _ => "contains",
            };
            if c["alternatives"] == json!(true) {
                p.notes.push(format!("rule [{id}]'s condition matches the query exactly, not its alternatives (synonyms, plurals)"));
            }
            crit.push(if pattern.is_empty() && kind == "exact" { criteria("always", Vec::new()) } else { criteria(kind, vec![pattern.to_lowercase()]) });
        }
        if let Some(ctx) = c["context"].as_str() {
            crit.push(json!({ "type": "exact", "metadata": "context", "values": [ctx] }));
        }
        if c["filters"].is_string() {
            p.skip("rule", id, "a condition on the search's filters has no query rule equivalent");
            return;
        }
        if crit.is_empty() {
            crit.push(criteria("always", Vec::new()));
        }
        crit_sets.push(crit);
    }
    if crit_sets.is_empty() {
        crit_sets.push(vec![criteria("always", Vec::new())]);
    }
    let cons = &r["consequence"];
    let mut promoted: Vec<(u64, String)> = Vec::new();
    for pr in cons["promote"].as_array().into_iter().flatten() {
        let pos = pr["position"].as_u64().unwrap_or(0);
        if let Some(one) = pr["objectID"].as_str() {
            promoted.push((pos, one.to_string()));
        }
        for (i, one) in strings(&pr["objectIDs"]).into_iter().enumerate() {
            promoted.push((pos + i as u64, one));
        }
    }
    promoted.sort_by_key(|(pos, _)| *pos);
    if promoted.iter().enumerate().any(|(i, (pos, _))| *pos != i as u64) {
        p.notes.push(format!("rule [{id}] pins its documents at the top in its order, not at the positions Algolia gave them"));
    }
    let pins: Vec<String> = promoted.into_iter().map(|(_, id)| id).collect();
    let hides: Vec<String> = cons["hide"].as_array().into_iter().flatten().filter_map(|h| h["objectID"].as_str().map(str::to_string)).collect();
    let mut rest = Vec::new();
    if cons["params"].as_object().is_some_and(|m| !m.is_empty()) {
        rest.push("its params (filters, query edits, search parameters)");
    }
    if !cons["userData"].is_null() {
        rest.push("its userData");
    }
    if cons["filterPromotes"] == json!(true) {
        rest.push("filterPromotes");
    }
    if r["validity"].as_array().is_some_and(|v| !v.is_empty()) {
        rest.push("its validity periods");
    }
    if pins.is_empty() && hides.is_empty() {
        let why = if rest.is_empty() { "it neither promotes nor hides anything".to_string() } else { format!("query rules pin or exclude documents; this rule only sets {}", rest.join(", ")) };
        p.skip("rule", id, why);
        return;
    }
    if !rest.is_empty() {
        p.skip("rule", id.clone(), format!("its promotions and hides are mapped, not {}", rest.join(", ")));
    }
    for (i, crit) in crit_sets.into_iter().enumerate() {
        let rid = if i == 0 { id.clone() } else { format!("{id}-{}", i + 1) };
        push_rules(p, &rid, crit, pins.clone(), hides.clone());
    }
}
