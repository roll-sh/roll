//! Suggesters (`SuggestBuilder` / `SuggestPhase`): `term` (Lucene's `DirectSpellChecker` per
//! analyzed token, options merged across shards) and `completion` (FST prefix and fuzzy
//! lookups over the field's analyzed keys, one suggestion per document, `_source` fetched)
//! and `phrase` (search-index's noisy-channel spell checker per shard, with collation).
//! Results render as `{name: [{text, offset, length, options: [...]}]}`.

use super::Target;
use crate::engine::fields::Kind;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::obj;
use crate::query::{Token, XParser};
use search_index::spell::{SpellOptions, StringDistance, SuggestMode, SuggestSort};

fn f(m: &Map, k: &str) -> Option<f64> {
    m.get(k).and_then(|v| match v {
        Value::Num(n) => Some(n.as_f64()),
        Value::Str(s) => s.parse().ok(),
        _ => None,
    })
}

/// `SuggestBuilder.fromXContent` for the `suggest` section at `p` (its start): the checks
/// Elasticsearch makes while parsing the request, with its exceptions and locations. The section
/// itself is kept as JSON and read again by [`run`].
pub fn check(p: &XParser<'_>) -> EsResult<()> {
    let src = p.source();
    let mut q = XParser::started(p.value(), Some(src), p.path());
    let mut name: Option<&str> = None;
    while let Some(t) = q.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => name = q.current_name(),
            Token::StartObject => suggestion(&mut q)?,
            t if t.is_value() => {
                if name != Some("text") {
                    return Err(EsError::illegal_argument(format!("[suggest] does not support [{}]", name.unwrap_or("null"))));
                }
            }
            t => return Err(q.parsing_error(format!("unexpected token [{}] after [{}]", t.name(), name.unwrap_or("null")))),
        }
    }
    Ok(())
}

/// `SuggestionBuilder.fromXContent`: `text`/`prefix`/`regex` plus one suggester object.
fn suggestion(q: &mut XParser<'_>) -> EsResult<()> {
    let mut name: Option<&str> = None;
    let mut found = false;
    while let Some(t) = q.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => name = q.current_name(),
            t if t.is_value() => {
                if !matches!(name, Some("text" | "prefix" | "regex")) {
                    return Err(q.parsing_error(format!("suggestion does not support [{}]", name.unwrap_or("null"))));
                }
            }
            Token::StartObject => {
                found = true;
                match name {
                    Some("term") => term_options(q)?,
                    Some("phrase") => phrase_options(q)?,
                    // Completion options and unknown suggester types are checked by [`run`].
                    _ => q.skip_children(),
                }
            }
            _ => q.skip_children(),
        }
    }
    if !found {
        return Err(EsError::parse("missing suggestion object"));
    }
    Ok(())
}

/// `SuggestionBuilder.size`.
fn check_size(n: i32) -> EsResult<()> {
    if n <= 0 {
        return Err(EsError::illegal_argument("size must be positive"));
    }
    Ok(())
}

/// A frequency that is a fraction of the docs or, above 1, a whole count.
fn check_freq(v: f32, what: &str) -> EsResult<()> {
    if v < 0.0 {
        return Err(EsError::illegal_argument(format!("{what} must be positive")));
    }
    if v > 1.0 && v != v.floor() {
        return Err(EsError::illegal_argument(format!("if {what} is greater than 1, it must not be a fraction")));
    }
    Ok(())
}

/// `TermSuggestionBuilder.fromXContent` and its setters' checks.
fn term_options(q: &mut XParser<'_>) -> EsResult<()> {
    let mut name: Option<&str> = None;
    let mut field = false;
    while let Some(t) = q.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => name = q.current_name(),
            t if t.is_value() => match name.unwrap_or("") {
                "analyzer" => {
                    q.text()?;
                }
                "field" => {
                    q.text()?;
                    field = true;
                }
                "size" => check_size(q.int_value()?)?,
                "shard_size" => {
                    q.int_value()?;
                }
                "suggest_mode" => {
                    let m = q.text()?.to_uppercase();
                    if !matches!(m.as_str(), "MISSING" | "POPULAR" | "ALWAYS") {
                        return Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.search.suggest.term.TermSuggestionBuilder.SuggestMode.{m}")));
                    }
                }
                "accuracy" => {
                    let a = q.float_value()?;
                    if !(0.0..=1.0).contains(&a) {
                        return Err(EsError::illegal_argument("accuracy must be between 0 and 1"));
                    }
                }
                "sort" => {
                    let m = q.text()?.to_uppercase();
                    if !matches!(m.as_str(), "SCORE" | "FREQUENCY") {
                        return Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.search.suggest.SortBy.{m}")));
                    }
                }
                "string_distance" => {
                    let d = q.text()?.to_lowercase();
                    if !matches!(d.as_str(), "internal" | "damerau_levenshtein" | "levenshtein" | "ngram" | "jaro_winkler") {
                        return Err(EsError::illegal_argument(format!("Illegal distance option {d}")));
                    }
                }
                "max_edits" => {
                    if !(1..=2).contains(&q.int_value()?) {
                        return Err(EsError::illegal_argument("maxEdits must be between 1 and 2"));
                    }
                }
                "max_inspections" => {
                    if q.int_value()? < 0 {
                        return Err(EsError::illegal_argument("maxInspections must be positive"));
                    }
                }
                "max_term_freq" => check_freq(q.float_value()?, "maxTermFreq")?,
                "prefix_length" => {
                    if q.int_value()? < 0 {
                        return Err(EsError::illegal_argument("prefixLength must be positive"));
                    }
                }
                "min_word_length" => {
                    if q.int_value()? < 1 {
                        return Err(EsError::illegal_argument("minWordLength must be greater or equal to 1"));
                    }
                }
                "min_doc_freq" => check_freq(q.float_value()?, "minDocFreq")?,
                other => return Err(q.parsing_error(format!("suggester[term] doesn't support field [{other}]"))),
            },
            _ => return Err(q.parsing_error(format!("suggester[term] parsing failed on [{}]", name.unwrap_or("null")))),
        }
    }
    if !field {
        return Err(EsError::parse("the required field option [field] is missing"));
    }
    Ok(())
}

/// `PhraseSuggestionBuilder.fromXContent`'s scalar options and their setters' checks; the
/// generator, smoothing and collate sections are read by the suggester.
fn phrase_options(q: &mut XParser<'_>) -> EsResult<()> {
    let mut name: Option<&str> = None;
    let mut field = false;
    while let Some(t) = q.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => name = q.current_name(),
            t if t.is_value() => match name.unwrap_or("") {
                "analyzer" | "separator" => {
                    q.text()?;
                }
                "field" => {
                    q.text()?;
                    field = true;
                }
                "size" => check_size(q.int_value()?)?,
                "shard_size" => {
                    q.int_value()?;
                }
                "real_word_error_likelihood" => {
                    if q.float_value()? <= 0.0 {
                        return Err(EsError::illegal_argument("real_word_error_likelihood must be > 0.0"));
                    }
                }
                "confidence" => {
                    if q.float_value()? < 0.0 {
                        return Err(EsError::illegal_argument("confidence must be >= 0.0"));
                    }
                }
                "max_errors" => {
                    if q.float_value()? <= 0.0 {
                        return Err(EsError::illegal_argument("max_error must be > 0.0"));
                    }
                }
                "gram_size" => {
                    if q.int_value()? < 1 {
                        return Err(EsError::illegal_argument("gramSize must be >= 1"));
                    }
                }
                "force_unigrams" => {
                    q.boolean_value()?;
                }
                "token_limit" => {
                    if q.int_value()? <= 0 {
                        return Err(EsError::illegal_argument("token_limit must be >= 1"));
                    }
                }
                other => return Err(q.parsing_error(format!("suggester[phrase] doesn't support field [{other}]"))),
            },
            Token::StartArray if name == Some("direct_generator") => q.skip_children(),
            Token::StartObject if name == Some("highlight") => {
                let (mut pre, mut post) = (false, false);
                let mut key: Option<&str> = None;
                while let Some(t) = q.next_token() {
                    match t {
                        Token::EndObject => break,
                        Token::FieldName => key = q.current_name(),
                        t if t.is_value() => match key.unwrap_or("") {
                            "pre_tag" => pre = true,
                            "post_tag" => post = true,
                            other => return Err(q.parsing_error(format!("suggester[phrase][highlight] doesn't support field [{other}]"))),
                        },
                        _ => q.skip_children(),
                    }
                }
                if pre != post {
                    return Err(EsError::illegal_argument("Pre and post tag must both be null or both not be null."));
                }
            }
            Token::StartObject if matches!(name, Some("smoothing" | "collate")) => q.skip_children(),
            Token::StartObject | Token::StartArray => {
                return Err(q.parsing_error(format!("suggester[phrase]  doesn't support array field [{}]", name.unwrap_or("null"))));
            }
            _ => return Err(q.parsing_error(format!("suggester[phrase] doesn't support field [{}]", name.unwrap_or("null")))),
        }
    }
    if !field {
        return Err(EsError::parse("the required field option [field] is missing"));
    }
    Ok(())
}

/// Runs the `suggest` section over the shards.
/// `typed_keys` prefixes each name with its suggester type (`term#name`).
/// `opts`: the search's fetch options, which completion suggestions are fetched with (their
/// options are hits: `_source` filtering, `fields`, `_version`, `_ignored`…).
/// A shard a suggestion failed on: `targets` position, the error, and whether the fetch phase
/// (rather than the query phase) failed.
pub type ShardFailure = (usize, EsError, bool);

/// Runs `suggest` over `targets`. Shards a completion suggestion fails on (a field that is
/// not a completion field there, an option whose fetch fails) are pushed to `failed` instead
/// of failing the request: Elasticsearch builds and fetches suggestions per shard, so they
/// are shard failures, and the coordinator decides about partial results.
pub fn run(cluster: &crate::Cluster, targets: &[Target], suggest: &Map, typed_keys: bool, opts: &super::hits::FetchOpts, failed: &mut Vec<ShardFailure>) -> EsResult<Value> {
    let global_text = suggest.get("text").and_then(Value::as_str).map(str::to_string);
    let mut out = Map::new();
    for (name, def) in suggest {
        if name == "text" {
            continue;
        }
        let Value::Obj(d) = def else { return Err(EsError::illegal_argument(format!("suggestion does not support [{name}]"))) };
        let text = d.get("text").or_else(|| d.get("prefix")).or_else(|| d.get("regex")).map(Value::to_es_string).or(global_text.clone());
        if text.is_none() {
            // `SuggestBuilder.build` runs on every shard.
            let e = EsError::illegal_argument("The required text option is missing");
            let failures: Vec<(usize, EsError)> = (0..targets.len()).map(|i| (i, e.clone())).collect();
            return Err(super::all_shards_failed(&cluster.node.id, targets, &failures, "all shards failed"));
        }
        let key = |ty: &str| if typed_keys { format!("{ty}#{name}") } else { name.clone() };
        if let Some(Value::Obj(t)) = d.get("term") {
            out.insert(key("term"), term_suggest(targets, text.as_deref().unwrap_or(""), t)?);
        } else if let Some(Value::Obj(c)) = d.get("completion") {
            let prefix = d.get("prefix").and_then(Value::as_str).or(d.get("text").and_then(Value::as_str)).unwrap_or("");
            // Context errors are raised while each shard builds its query.
            let r = completion_suggest(targets, prefix, c, opts, failed).map_err(|e| {
                if e.kind == "illegal_argument_exception" && e.reason().starts_with("Missing mandatory contexts") {
                    let failures: Vec<(usize, EsError)> = (0..targets.len()).map(|i| (i, e.clone())).collect();
                    super::all_shards_failed(&cluster.node.id, targets, &failures, "all shards failed")
                } else {
                    e
                }
            })?;
            out.insert(key("completion"), r);
        } else if let Some(Value::Obj(p)) = d.get("phrase") {
            smoothing_model(p)?;
            // Building and running the suggestion happen on every shard.
            let r = phrase_suggest(cluster, targets, text.as_deref().unwrap_or(""), p).map_err(|e| {
                if matches!(&*e.kind, "illegal_argument_exception" | "illegal_state_exception") {
                    let failures: Vec<(usize, EsError)> = (0..targets.len()).map(|i| (i, e.clone())).collect();
                    super::all_shards_failed(&cluster.node.id, targets, &failures, "all shards failed")
                } else {
                    e
                }
            })?;
            out.insert(key("phrase"), r);
        } else {
            return Err(EsError::new(400, "named_object_not_found_exception", format!("[{name}] unknown suggester type")));
        }
    }
    Ok(Value::Obj(out))
}

fn spell_opts(t: &Map) -> EsResult<SpellOptions> {
    let mut o = SpellOptions::default();
    if let Some(v) = f(t, "accuracy") {
        o.accuracy = v as f32;
    }
    if let Some(v) = f(t, "max_edits") {
        o.max_edits = v as u32;
    }
    if let Some(v) = f(t, "max_inspections") {
        o.max_inspections = v as u32;
    }
    if let Some(v) = f(t, "max_term_freq") {
        o.max_term_freq = v as f32;
    }
    if let Some(v) = f(t, "prefix_length").or_else(|| f(t, "prefix_len")) {
        o.prefix_length = v as usize;
    }
    if let Some(v) = f(t, "min_word_length").or_else(|| f(t, "min_word_len")) {
        o.min_word_length = v as usize;
    }
    if let Some(v) = f(t, "min_doc_freq") {
        o.min_doc_freq = v as f32;
    }
    if let Some(Value::Str(s)) = t.get("suggest_mode") {
        o.suggest_mode = match s.as_str() {
            "missing" => SuggestMode::Missing,
            "popular" => SuggestMode::Popular,
            "always" => SuggestMode::Always,
            other => return Err(EsError::illegal_argument(format!("Illegal suggest mode {other}"))),
        };
    }
    if let Some(Value::Str(s)) = t.get("sort") {
        o.sort = match s.as_str() {
            "score" => SuggestSort::Score,
            "frequency" => SuggestSort::Frequency,
            other => return Err(EsError::illegal_argument(format!("Illegal suggest sort {other}"))),
        };
    }
    if let Some(Value::Str(s)) = t.get("string_distance") {
        o.string_distance = match s.as_str() {
            "internal" => StringDistance::Internal,
            "damerau_levenshtein" => StringDistance::DamerauLevenshtein,
            "levenshtein" => StringDistance::Levenshtein,
            "jaro_winkler" => StringDistance::JaroWinkler,
            "ngram" => StringDistance::NGram,
            other => return Err(EsError::illegal_argument(format!("Illegal distance option {other}"))),
        };
    }
    if let Some(Value::Bool(b)) = t.get("lowercase_terms") {
        o.lowercase_terms = *b;
    }
    Ok(o)
}

#[allow(clippy::type_complexity)]
fn term_suggest(targets: &[Target], text: &str, t: &Map) -> EsResult<Value> {
    let field = t.get("field").and_then(Value::as_str).ok_or_else(|| EsError::illegal_argument("The required field option is missing"))?;
    let size = f(t, "size").unwrap_or(5.0) as usize;
    let shard_size = f(t, "shard_size").map(|v| v as usize).unwrap_or(size.max(5));
    let opts = spell_opts(t)?;
    let mut entries: Vec<(String, u32, u32, Vec<(String, f32, u32)>)> = Vec::new();
    for tg in targets {
        let table = tg.table();
        let Some(fi) = table.get(field) else { continue };
        let analyzer = match t.get("analyzer").and_then(Value::as_str) {
            Some(a) => table.analysis.analyzer(a),
            None => fi.query_analyzer().cloned(),
        };
        let mut toks: Vec<(String, u32, u32)> = Vec::new();
        match &analyzer {
            Some(a) => a.analyze(text, &mut |tk| toks.push((tk.term.to_string(), tk.start_offset, tk.end_offset))),
            None => toks.push((text.to_string(), 0, text.encode_utf16().count() as u32)),
        }
        // Suggesters read the shard's own statistics, never a DFS search's global ones.
        let searcher = tg.searcher()?.with_global_stats(None);
        for (i, (term, start, end)) in toks.iter().enumerate() {
            let words = searcher.suggest_similar(fi.number, term, shard_size, &opts).map_err(crate::engine::convert::index_error)?;
            if entries.len() <= i {
                entries.push((term.clone(), *start, end - start, Vec::new()));
            }
            let e = &mut entries[i];
            for w in words {
                match e.3.iter_mut().find(|(t, _, _)| *t == w.term) {
                    Some(x) => {
                        x.2 += w.freq;
                        x.1 = x.1.max(w.score);
                    }
                    None => e.3.push((w.term, w.score, w.freq)),
                }
            }
        }
    }
    let mut out = Vec::new();
    for (text, offset, length, mut opts_v) in entries {
        match opts.sort {
            SuggestSort::Score => opts_v.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0))),
            SuggestSort::Frequency => opts_v.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.total_cmp(&a.1)).then(a.0.cmp(&b.0))),
        }
        opts_v.truncate(size);
        let options: Vec<Value> = opts_v
            .into_iter()
            .map(|(t, s, fr)| obj! { "text" => t, "score" => Value::Num(Number::Float(crate::fetch::java_float_as_double(s))), "freq" => fr as i64 })
            .collect();
        out.push(obj! { "text" => text, "offset" => offset as i64, "length" => length as i64, "options" => Value::Arr(options) });
    }
    Ok(Value::Arr(out))
}

fn completion_suggest(targets: &[Target], prefix: &str, c: &Map, opts: &super::hits::FetchOpts, failed: &mut Vec<ShardFailure>) -> EsResult<Value> {
    let field = c.get("field").and_then(Value::as_str).ok_or_else(|| EsError::illegal_argument("The required field option is missing"))?;
    let size = f(c, "size").unwrap_or(5.0) as usize;
    let skip_duplicates = c.get("skip_duplicates").and_then(Value::as_bool).unwrap_or(false);
    let fuzzy = c.get("fuzzy");
    // (score, text, target, doc, matched contexts)
    type Option_ = (f32, String, usize, u32, Vec<(String, String)>);
    let mut all: Vec<Option_> = Vec::new();
    // Each target's searcher and field table, kept for the fetch of the merged options.
    let mut readers: Vec<Option<(search_index::Searcher, std::sync::Arc<crate::engine::fields::FieldTable>)>> = vec![None; targets.len()];
    for (ti, tg) in targets.iter().enumerate() {
        let table = tg.table();
        // `CompletionSuggestionBuilder.build` on each shard: an unmapped field fails too.
        let Some(fi) = table.get(field).filter(|fi| fi.kind == Kind::Completion) else {
            if !failed.iter().any(|f| f.0 == ti) {
                failed.push((ti, EsError::illegal_argument(format!("Field [{field}] is not a completion suggest field")), false));
            }
            continue;
        };
        if failed.iter().any(|f| f.0 == ti) {
            continue;
        }
        let key = crate::engine::completion::analyzed_key(fi, prefix, fi.search_analyzer.as_ref().or(fi.analyzer.as_ref()));
        // Suggesters read the shard's own statistics, never a DFS search's global ones.
        let searcher = tg.searcher()?.with_global_stats(None);
        let defs = crate::engine::completion::context_defs(fi);
        let query_contexts = if defs.is_empty() { Vec::new() } else { crate::engine::completion::query_contexts(&defs, c.get("contexts").and_then(Value::as_object))? };
        // Runs the input automaton, behind each query context when the field has contexts
        // (a doc keeps its best context).
        // `FuzzyCompletionQuery` boosts every match (exact ones too) by how far the accepted
        // key prefix agrees with the query's analyzed key.
        // (`BoostFn`s are `'static`: they own the key.)
        let analyzed = std::sync::Arc::new(vec![key.clone()]);
        let fuzzy_boost = {
            let analyzed = analyzed.clone();
            move |p: &[u8]| search_index::suggest::fuzzy_prefix_boost(p, &analyzed)
        };
        let run = |aut: &dyn search_codec::automaton::Automaton| -> EsResult<Vec<search_index::suggest::Suggestion>> {
            if query_contexts.is_empty() {
                let boost: Option<&search_index::suggest::BoostFn> = if fuzzy.is_some() { Some(&fuzzy_boost) } else { None };
                return searcher
                    .complete(&search_index::suggest::CompletionQuery { field: fi.number, automaton: aut, boost, size, skip_duplicates, one_per_doc: true })
                    .map_err(crate::engine::convert::index_error);
            }
            let mut best: Vec<search_index::suggest::Suggestion> = Vec::new();
            // Every context a doc matched is reported (the best candidate's prefixes first).
            let mut all_prefixes: Vec<(u32, Vec<u8>)> = Vec::new();
            for qc in &query_contexts {
                let ca = crate::engine::completion::ContextAutomaton { context: &qc.typed, exact: qc.exact, inner: aut };
                let boost = qc.boost;
                // `ContextCompletionWeight.boost`: the context's boost plus the inner query's,
                // which sees the key after the context.
                let is_fuzzy = fuzzy.is_some();
                let analyzed = analyzed.clone();
                let bf = move |p: &[u8]| {
                    let rest = p.iter().position(|&b| b == crate::engine::completion::CONTEXT_SEP).map_or(p, |i| &p[i + 1..]);
                    boost + if is_fuzzy { search_index::suggest::fuzzy_prefix_boost(rest, &analyzed) } else { 0.0 }
                };
                let found = searcher
                    .complete(&search_index::suggest::CompletionQuery { field: fi.number, automaton: &ca, boost: Some(&bf), size, skip_duplicates, one_per_doc: true })
                    .map_err(crate::engine::convert::index_error)?;
                for s in found {
                    for p in &s.prefixes {
                        if !all_prefixes.iter().any(|(d, x)| *d == s.doc && x == p) {
                            all_prefixes.push((s.doc, p.clone()));
                        }
                    }
                    match best.iter_mut().find(|b| b.doc == s.doc) {
                        Some(b) if s.score > b.score => *b = s,
                        Some(_) => {}
                        None => best.push(s),
                    }
                }
            }
            for b in best.iter_mut() {
                b.prefixes = all_prefixes.iter().filter(|(d, _)| *d == b.doc).map(|(_, p)| p.clone()).collect();
            }
            Ok(best)
        };
        // `FuzzyOptions` defaults: 1 edit, `prefix_length` 1, `min_length` 3; a key no longer
        // than the prefix or shorter than `min_length` is matched exactly (but still boosted).
        let fuzzy_params = fuzzy.map(|fz| {
            let fm = fz.as_object().cloned().unwrap_or_default();
            let edits = match fm.get("fuzziness") {
                None => 1,
                // `Fuzziness.asDistance()` without a term: AUTO takes length 5.
                Some(v) => match v.to_es_string().to_ascii_uppercase().as_str() {
                    s if s.starts_with("AUTO") => {
                        let (lo, hi) = s.strip_prefix("AUTO:").and_then(|r| r.split_once(',')).and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?))).unwrap_or((3usize, 6usize));
                        if 5 < lo { 0 } else if 5 < hi { 1 } else { 2 }
                    }
                    s => s.parse::<f64>().map_or(1, |n| (n as usize).min(2)),
                },
            };
            let prefix_len = f(&fm, "prefix_length").unwrap_or(1.0).max(0.0) as usize;
            let min_len = f(&fm, "min_length").unwrap_or(3.0).max(0.0) as usize;
            let transpositions = fm.get("transpositions").and_then(Value::as_bool).unwrap_or(true);
            (edits, prefix_len, min_len, transpositions)
        });
        let results = match fuzzy_params {
            Some((edits, prefix_len, min_len, transpositions)) if key.len() > prefix_len && key.len() >= min_len => {
                let text = String::from_utf8_lossy(&key).into_owned();
                let aut = search_codec::automaton::dfa::Dfa::levenshtein(&text, edits as u32, prefix_len, transpositions);
                let pref = PrefixOf(aut);
                run(&pref)?
            }
            _ => {
                let aut = search_codec::automaton::Prefix::new(key.clone());
                run(&aut)?
            }
        };
        for s in results {
            let ctx: Vec<(String, String)> = if defs.is_empty() {
                Vec::new()
            } else {
                let mut v: Vec<(String, String)> = Vec::new();
                for p in s.prefixes.iter().chain(std::iter::once(&s.key)) {
                    if let Some((n, c)) = crate::engine::completion::key_context(&defs, p)
                        && !v.iter().any(|(a, b)| a == n && *b == c)
                    {
                        v.push((n.to_string(), c));
                    }
                }
                v
            };
            all.push((s.score, String::from_utf8_lossy(&s.payload).into_owned(), ti, s.doc, ctx));
        }
        readers[ti] = Some((searcher, table.clone()));
    }
    all.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
    if skip_duplicates {
        let mut seen = Vec::new();
        all.retain(|x| {
            if seen.contains(&x.1) {
                false
            } else {
                seen.push(x.1.clone());
                true
            }
        });
    }
    all.truncate(size);
    // The fetch phase loads only the merged options. A shard whose fetch fails is a failed
    // shard; its options stay, as text and score only.
    let mut hits: Vec<Option<Map>> = Vec::with_capacity(all.len());
    for (score, _, ti, doc, _) in &all {
        let Some((searcher, table)) = &readers[*ti] else {
            hits.push(None);
            continue;
        };
        let fetched = (|| -> EsResult<Map> {
            let stored = searcher.stored(*doc).map_err(crate::engine::convert::index_error)?;
            // `stored_fields: _none_` drops `_id` and every metadata field, as in `hits`.
            let mut hit = Map::new();
            if opts.stored_fields != Some(None) || opts.keep_id {
                hit.insert("_id".into(), Value::Str(super::hits::stored_id(&stored).unwrap_or_default()));
            }
            super::hits::render_doc_stored(table, Some(&targets[*ti].idx), searcher, *doc, &stored, opts, &mut hit, Some(*score), 0)?;
            Ok(hit)
        })();
        match fetched {
            Ok(h) => hits.push(Some(h)),
            Err(e) => {
                failed.push((*ti, e, true));
                readers[*ti] = None;
                hits.push(None);
            }
        }
    }
    let options: Vec<Value> = all
        .into_iter()
        .zip(hits)
        .map(|((score, text, ti, _doc, ctx), hit)| {
            let mut m = Map::new();
            m.insert("text".into(), Value::Str(text));
            match hit.filter(|_| readers[ti].is_some()) {
                Some(hit) => {
                    m.insert("_index".into(), Value::Str(targets[ti].idx.name.clone()));
                    m.extend(hit);
                }
                None => {
                    m.insert("score".into(), Value::Num(Number::Float(crate::fetch::java_float_as_double(score))));
                }
            }
            if !ctx.is_empty() {
                let mut cm = Map::new();
                for (name, value) in ctx {
                    match cm.get_mut(&name) {
                        Some(Value::Arr(a)) => a.push(Value::Str(value)),
                        _ => {
                            cm.insert(name, Value::Arr(vec![Value::Str(value)]));
                        }
                    }
                }
                // `getNamedContexts` collects each name's values in a `HashSet`.
                for v in cm.values_mut() {
                    if let Value::Arr(a) = v {
                        let keys: Vec<String> = a.iter().map(Value::to_es_string).collect();
                        let order = crate::json::java_hash_order(&keys, 16);
                        *a = order.into_iter().map(|i| Value::Str(keys[i].clone())).collect();
                    }
                }
                m.insert("contexts".into(), Value::Obj(cm));
            }
            Value::Obj(m)
        })
        .collect();
    Ok(Value::Arr(vec![obj! { "text" => prefix, "offset" => 0, "length" => prefix.encode_utf16().count() as i64, "options" => Value::Arr(options) }]))
}

/// An automaton accepting every extension of what `0` accepts (fuzzy prefixes).
struct PrefixOf(search_codec::automaton::dfa::Dfa);

impl search_codec::automaton::Automaton for PrefixOf {
    fn start(&self) -> u32 {
        self.0.start()
    }
    fn is_match(&self, state: u32) -> bool {
        self.0.is_match(state)
    }
    fn can_match(&self, state: u32) -> bool {
        self.0.is_match(state) || self.0.can_match(state)
    }
    fn will_always_match(&self, state: u32) -> bool {
        self.0.is_match(state)
    }
    fn accept(&self, state: u32, byte: u8) -> u32 {
        // Once matched, every continuation stays matched (prefix semantics).
        if self.0.is_match(state) {
            return state;
        }
        self.0.accept(state, byte)
    }
}

/// A `phrase` suggester's settings (`PhraseSuggestionBuilder`, defaults from
/// `PhraseSuggestionContext`).
struct PhraseSpec {
    field: String,
    analyzer: Option<String>,
    size: usize,
    shard_size: usize,
    rwl: f32,
    confidence: f32,
    max_errors: f32,
    separator: String,
    gram_size: Option<usize>,
    force_unigrams: bool,
    token_limit: usize,
    tags: Option<(String, String)>,
    smoothing: search_index::phrase_suggest::Smoothing,
    generators: Vec<Map>,
    /// `collate`: the query template, its params, `prune`.
    collate: Option<(Value, Map, bool)>,
}

/// `PhraseSuggestionBuilder.parseSmoothingModel`: each named model replaces the previous one.
/// Parsed with the request, before any shard runs.
fn smoothing_model(p: &Map) -> EsResult<search_index::phrase_suggest::Smoothing> {
    use search_index::phrase_suggest::Smoothing;
    // The implicit default model (`StupidBackoffScorer.DEFAULT_BACKOFF_DISCOUNT` as a float).
    let mut smoothing = Smoothing::StupidBackoff { discount: 0.4f32 as f64 };
    if let Some(Value::Obj(models)) = p.get("smoothing") {
        for (k, v) in models {
            let Value::Obj(m) = v else {
                return Err(EsError::new(400, "parsing_exception", format!("[smoothing] unknown token [{}] after [{k}]", if matches!(v, Value::Num(_)) { "VALUE_NUMBER" } else { v.kind_name() })));
            };
            smoothing = match k.as_str() {
                "laplace" => Smoothing::Laplace { alpha: f(m, "alpha").unwrap_or(0.5) },
                "linear" => {
                    let (trigram, bigram, unigram) = (f(m, "trigram_lambda").unwrap_or(0.0), f(m, "bigram_lambda").unwrap_or(0.0), f(m, "unigram_lambda").unwrap_or(0.0));
                    // `LinearInterpolation`'s constructor.
                    if (trigram + bigram + unigram - 1.0).abs() > 0.001 {
                        return Err(EsError::illegal_argument("linear smoothing lambdas must sum to 1"));
                    }
                    Smoothing::LinearInterpolation { trigram, bigram, unigram }
                }
                "stupid_backoff" => Smoothing::StupidBackoff { discount: f(m, "discount").unwrap_or(0.4) },
                _ => return Err(EsError::illegal_argument(format!("suggester[phrase] doesn't support object field [{k}]"))),
            };
        }
    }
    Ok(smoothing)
}

fn phrase_spec(p: &Map) -> EsResult<PhraseSpec> {
    let field = p.get("field").and_then(Value::as_str).ok_or_else(|| EsError::illegal_argument("The required field option is missing"))?.to_string();
    let size = f(p, "size").unwrap_or(5.0) as usize;
    let tags = p.get("highlight").and_then(Value::as_object).and_then(|h| {
        let pre = h.get("pre_tag").and_then(Value::as_str)?;
        let post = h.get("post_tag").and_then(Value::as_str)?;
        Some((pre.to_string(), post.to_string()))
    });
    let smoothing = smoothing_model(p)?;
    let generators = match p.get("direct_generator") {
        Some(Value::Arr(a)) => a.iter().filter_map(Value::as_object).cloned().collect(),
        Some(Value::Obj(m)) => vec![m.clone()],
        _ => Vec::new(),
    };
    let collate = match p.get("collate").and_then(Value::as_object) {
        Some(c) => {
            let q = match c.get("query") {
                Some(Value::Obj(m)) => m.get("source").or_else(|| m.get("inline")).cloned().unwrap_or(Value::Obj(m.clone())),
                Some(v) => v.clone(),
                None => return Err(EsError::illegal_argument("suggester[phrase][collate] query is missing")),
            };
            let params = c.get("params").and_then(Value::as_object).cloned().unwrap_or_default();
            Some((q, params, c.get("prune").and_then(Value::as_bool).unwrap_or(false)))
        }
        None => None,
    };
    Ok(PhraseSpec {
        field,
        analyzer: p.get("analyzer").and_then(Value::as_str).map(str::to_string),
        size,
        shard_size: f(p, "shard_size").map(|v| v as usize).unwrap_or(size.max(5)),
        rwl: f(p, "real_word_error_likelihood").unwrap_or(0.95) as f32,
        confidence: f(p, "confidence").unwrap_or(1.0) as f32,
        max_errors: f(p, "max_errors").unwrap_or(0.5) as f32,
        separator: p.get("separator").and_then(Value::as_str).unwrap_or(" ").to_string(),
        gram_size: f(p, "gram_size").map(|v| v as usize),
        force_unigrams: p.get("force_unigrams").and_then(Value::as_bool).unwrap_or(true),
        token_limit: f(p, "token_limit").unwrap_or(10.0) as usize,
        tags,
        smoothing,
        generators,
        collate,
    })
}

/// A generator's `pre_filter` / `post_filter`: the term through an analyzer.
fn term_filter(a: search_analysis::Analyzer) -> Box<search_index::phrase_suggest::TermFilter> {
    Box::new(move |term: &[u8]| {
        let text = String::from_utf8_lossy(term);
        let mut out = Vec::new();
        a.analyze(&text, &mut |t| out.push((t.term.as_bytes().to_vec(), t.position_increment)));
        out
    })
}

/// The `phrase` suggester (`PhraseSuggester`): every shard's noisy-channel corrections of the
/// analyzed text (search-index's [`Searcher::phrase_suggest`]), optionally collated against a
/// query template, merged across shards by text (highest score) above every shard's cutoff,
/// best first and ties by text, trimmed to `size`.
///
/// [`Searcher::phrase_suggest`]: search_index::Searcher::phrase_suggest
fn phrase_suggest(cluster: &crate::Cluster, targets: &[Target], text: &str, p: &Map) -> EsResult<Value> {
    use search_index::phrase_suggest::{DirectGenerator, PhraseSuggestOptions, PhraseToken};
    let spec = phrase_spec(p)?;
    let st = cluster.state();
    // (text, highlighted, score, collate_match)
    let mut options: Vec<(String, Option<String>, f32, Option<bool>)> = Vec::new();
    let mut cutoff = f64::from_bits(1);
    let collate_template = match &spec.collate {
        Some((q, _, _)) => {
            let (src, opts): (String, Vec<(String, String)>) = match q {
                Value::Str(s) => (s.clone(), Vec::new()),
                other => (other.to_json(), vec![("content_type".into(), "application/json".into())]),
            };
            Some(cluster.scripts.engine.compile_template(&src, search_painless::Context::Template, &opts).map_err(crate::scripting::script_error)?)
        }
        None => None,
    };
    for tg in targets {
        let table = tg.table();
        let Some(fi) = table.get(&spec.field) else { continue };
        let analyzer = match &spec.analyzer {
            Some(a) => Some(table.analysis.analyzer(a).ok_or_else(|| EsError::illegal_argument(format!("Analyzer [{a}] doesn't exists")))?),
            None => fi.query_analyzer().cloned(),
        };
        // `PhraseSuggestionBuilder.build`: the gram size from the analyzer's shingles, and
        // the unigram requirement of the default generator.
        let shingle = analyzer.as_ref().and_then(|a| a.shingle());
        let mut gram_size = spec.gram_size.unwrap_or(1);
        if spec.gram_size.is_none() || spec.generators.is_empty() {
            if spec.gram_size.is_none()
                && let Some(sh) = shingle
            {
                gram_size = sh.max_shingle_size;
            }
            if spec.generators.is_empty()
                && let Some(sh) = shingle
                && sh.min_shingle_size > 1
                && !sh.output_unigrams
                && spec.force_unigrams
            {
                return Err(EsError::illegal_argument(format!(
                    "The default candidate generator for phrase suggest can't operate on field: [{}] since it doesn't emit unigrams. If this is intentional try to set the candidate generator field explicitly",
                    spec.field
                )));
            }
        }
        let gen_specs: Vec<Map> = if spec.generators.is_empty() {
            let mut m = Map::new();
            m.insert("field".into(), Value::Str(spec.field.clone()));
            vec![m]
        } else {
            spec.generators.clone()
        };
        let mut generators = Vec::with_capacity(gen_specs.len());
        for g in &gen_specs {
            let gf = g.get("field").and_then(Value::as_str).ok_or_else(|| EsError::illegal_argument("[direct_generator] field is missing"))?;
            let Some(gi) = table.get(gf) else { continue };
            let mut spell = spell_opts(g)?;
            if g.get("suggest_mode").is_none() {
                spell.suggest_mode = SuggestMode::Missing;
            }
            let filter = |k: &str| -> EsResult<Option<Box<search_index::phrase_suggest::TermFilter>>> {
                match g.get(k).and_then(Value::as_str) {
                    Some(a) => {
                        let an = table.analysis.analyzer(a).ok_or_else(|| EsError::illegal_argument(format!("Analyzer [{a}] doesn't exists")))?;
                        Ok(Some(term_filter(an)))
                    }
                    None => Ok(None),
                }
            };
            generators.push(DirectGenerator { field: gi.number, spell, size: f(g, "size").unwrap_or(5.0) as usize, pre_filter: filter("pre_filter")?, post_filter: filter("post_filter")? });
        }
        let mut tokens: Vec<PhraseToken> = Vec::new();
        match &analyzer {
            Some(a) => a.analyze(text, &mut |t| {
                tokens.push(PhraseToken { term: t.term.as_bytes().to_vec(), position_increment: t.position_increment, shingle: t.kind == "shingle", synonym: t.kind == "SYNONYM" })
            }),
            None => tokens.push(PhraseToken { term: text.as_bytes().to_vec(), position_increment: 1, shingle: false, synonym: false }),
        }
        let opts = PhraseSuggestOptions {
            field: fi.number,
            real_word_error_likelihood: spec.rwl,
            confidence: spec.confidence,
            max_errors: spec.max_errors,
            separator: spec.separator.as_bytes().to_vec(),
            shard_size: spec.shard_size,
            gram_size,
            require_unigram: spec.force_unigrams,
            token_limit: spec.token_limit,
            smoothing: spec.smoothing,
            generators,
        };
        // Suggesters read the shard's own statistics, never a DFS search's global ones.
        let searcher = tg.searcher()?.with_global_stats(None);
        let result = searcher.phrase_suggest(&tokens, &opts).map_err(crate::engine::convert::index_error)?;
        cutoff = cutoff.max(result.cutoff_score);
        for c in &result.corrections {
            let phrase = String::from_utf8_lossy(&c.join(b" ", None)).into_owned();
            let mut collate_match = None;
            if let (Some(t), Some((_, params, prune))) = (&collate_template, &spec.collate) {
                let mut vars = params.clone();
                vars.insert("suggestion".into(), Value::Str(phrase.clone()));
                let rendered = t.render_json(&crate::scripting::to_serde(&Value::Obj(vars))).map_err(crate::scripting::script_error)?;
                let qv = crate::json::parse(rendered.as_bytes()).map_err(|e| EsError::x_content_parse_bare(e.to_string()))?.unwrap_or(Value::Obj(Map::new()));
                let (query, _) = crate::query::parse_query(&qv, None, &[])?;
                let qctx = super::query::QueryCtx::new(cluster, &st, &tg.idx, &table, &searcher, crate::time::now_millis());
                let q = qctx.to_query(&query)?;
                let matched = searcher.count(&q).map_err(crate::engine::convert::index_error)? > 0;
                if !matched && !prune {
                    continue;
                }
                if *prune {
                    collate_match = Some(matched);
                }
            }
            let highlighted = spec.tags.as_ref().map(|(pre, post)| String::from_utf8_lossy(&c.join(b" ", Some((pre.as_bytes(), post.as_bytes())))).into_owned());
            let score = c.score as f32;
            match options.iter_mut().find(|o| o.0 == phrase) {
                Some(o) => {
                    if score > o.2 {
                        o.2 = score;
                    }
                    if let (Some(a), Some(b)) = (o.3.as_mut(), collate_match) {
                        *a |= b;
                    }
                }
                None => options.push((phrase, highlighted, score, collate_match)),
            }
        }
    }
    // `PhraseSuggestion.Entry.addOption`: only options above the (largest) cutoff.
    options.retain(|o| o.2 as f64 > cutoff);
    options.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    options.truncate(spec.size);
    let rendered: Vec<Value> = options
        .into_iter()
        .map(|(t, h, s, c)| {
            let mut m = Map::new();
            m.insert("text".into(), Value::Str(t));
            if let Some(h) = h {
                m.insert("highlighted".into(), Value::Str(h));
            }
            m.insert("score".into(), Value::Num(Number::Float(crate::fetch::java_float_as_double(s))));
            if let Some(c) = c {
                m.insert("collate_match".into(), Value::Bool(c));
            }
            Value::Obj(m)
        })
        .collect();
    Ok(Value::Arr(vec![obj! { "text" => text, "offset" => 0, "length" => text.encode_utf16().count() as i64, "options" => Value::Arr(rendered) }]))
}
