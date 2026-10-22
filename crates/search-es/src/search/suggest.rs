//! Suggesters (`SuggestBuilder` / `SuggestPhase`): `term` (Lucene's `DirectSpellChecker` per
//! analyzed token, options merged across shards) and `completion` (FST prefix and fuzzy
//! lookups over the field's analyzed keys, one suggestion per document, `_source` fetched).
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
pub fn run(cluster: &crate::Cluster, targets: &[Target], suggest: &Map, typed_keys: bool) -> EsResult<Value> {
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
            let r = completion_suggest(targets, prefix, c).map_err(|e| {
                if e.kind == "illegal_argument_exception" && e.reason().starts_with("Missing mandatory contexts") {
                    let failures: Vec<(usize, EsError)> = (0..targets.len()).map(|i| (i, e.clone())).collect();
                    super::all_shards_failed(&cluster.node.id, targets, &failures, "all shards failed")
                } else {
                    e
                }
            })?;
            out.insert(key("completion"), r);
        } else if let Some(Value::Obj(p)) = d.get("phrase") {
            out.insert(key("phrase"), phrase_suggest(targets, text.as_deref().unwrap_or(""), p)?);
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
        let table = tg.engine.table();
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
        let searcher = tg.searcher()?;
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

fn completion_suggest(targets: &[Target], prefix: &str, c: &Map) -> EsResult<Value> {
    let field = c.get("field").and_then(Value::as_str).ok_or_else(|| EsError::illegal_argument("The required field option is missing"))?;
    let size = f(c, "size").unwrap_or(5.0) as usize;
    let skip_duplicates = c.get("skip_duplicates").and_then(Value::as_bool).unwrap_or(false);
    let fuzzy = c.get("fuzzy");
    // (score, text, target, doc, _source, _id, matched contexts)
    type Option_ = (f32, String, usize, u32, Option<Value>, String, Vec<(String, String)>);
    let mut all: Vec<Option_> = Vec::new();
    for (ti, tg) in targets.iter().enumerate() {
        let table = tg.engine.table();
        let Some(fi) = table.get(field) else { continue };
        if fi.kind != Kind::Completion {
            return Err(EsError::illegal_argument(format!("Field [{field}] is not a completion suggest field")));
        }
        let key = crate::engine::completion::analyzed_key(fi, prefix, fi.search_analyzer.as_ref().or(fi.analyzer.as_ref()));
        let searcher = tg.searcher()?;
        let defs = crate::engine::completion::context_defs(fi);
        let query_contexts = if defs.is_empty() { Vec::new() } else { crate::engine::completion::query_contexts(&defs, c.get("contexts").and_then(Value::as_object))? };
        // Runs the input automaton, behind each query context when the field has contexts
        // (a doc keeps its best context).
        let run = |aut: &dyn search_codec::automaton::Automaton| -> EsResult<Vec<search_index::suggest::Suggestion>> {
            if query_contexts.is_empty() {
                return searcher
                    .complete(&search_index::suggest::CompletionQuery { field: fi.number, automaton: aut, boost: None, size, skip_duplicates, one_per_doc: true })
                    .map_err(crate::engine::convert::index_error);
            }
            let mut best: Vec<search_index::suggest::Suggestion> = Vec::new();
            // Every context a doc matched is reported (the best candidate's prefixes first).
            let mut all_prefixes: Vec<(u32, Vec<u8>)> = Vec::new();
            for qc in &query_contexts {
                let ca = crate::engine::completion::ContextAutomaton { context: &qc.typed, exact: qc.exact, inner: aut };
                let boost = qc.boost;
                let bf = move |_: &[u8]| boost;
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
        let results = match fuzzy {
            Some(fz) => {
                let fm = fz.as_object().cloned().unwrap_or_default();
                let edits = match fm.get("fuzziness").map(|v| v.to_es_string()).as_deref() {
                    Some("0") => 0,
                    Some("1") => 1,
                    Some("2") => 2,
                    _ => if key.len() < 3 { 0 } else if key.len() < 6 { 1 } else { 2 },
                };
                let prefix_len = f(&fm, "prefix_length").unwrap_or(1.0) as usize;
                let transpositions = fm.get("transpositions").and_then(Value::as_bool).unwrap_or(true);
                let text = String::from_utf8_lossy(&key).into_owned();
                let aut = search_codec::automaton::dfa::Dfa::levenshtein(&text, edits, prefix_len, transpositions);
                let pref = PrefixOf(aut);
                run(&pref)?
            }
            None => {
                let aut = search_codec::automaton::Prefix::new(key.clone());
                run(&aut)?
            }
        };
        for s in results {
            let stored = searcher.stored(s.doc).map_err(crate::engine::convert::index_error)?;
            let id = super::hits::stored_id(&stored).unwrap_or_default();
            let src = crate::fetch::render_source(&stored, &crate::fetch::FetchSource::default())?;
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
            all.push((s.score, String::from_utf8_lossy(&s.payload).into_owned(), ti, s.doc, src, id, ctx));
        }
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
    let options: Vec<Value> = all
        .into_iter()
        .map(|(score, text, ti, _doc, src, id, ctx)| {
            let mut m = Map::new();
            m.insert("text".into(), Value::Str(text));
            m.insert("_index".into(), Value::Str(targets[ti].idx.name.clone()));
            m.insert("_id".into(), Value::Str(id));
            m.insert("_score".into(), Value::Num(Number::Float(crate::fetch::java_float_as_double(score))));
            if let Some(s) = src {
                m.insert("_source".into(), s);
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

fn phrase_suggest(targets: &[Target], text: &str, p: &Map) -> EsResult<Value> {
    // A term suggester over each token, joined: the best correction of every misspelt word.
    let field = p.get("field").and_then(Value::as_str).ok_or_else(|| EsError::illegal_argument("The required field option is missing"))?;
    let mut tm = Map::new();
    tm.insert("field".into(), Value::Str(field.to_string()));
    tm.insert("size".into(), Value::from(1i64));
    tm.insert("suggest_mode".into(), Value::Str("missing".into()));
    let terms = term_suggest(targets, text, &tm)?;
    let mut corrected = text.to_string();
    let mut changed = false;
    if let Value::Arr(entries) = &terms {
        for e in entries.iter().rev() {
            let (Some(off), Some(len)) = (e.get("offset").and_then(Value::as_i64), e.get("length").and_then(Value::as_i64)) else { continue };
            if let Some(Value::Arr(opts)) = e.get("options")
                && let Some(best) = opts.first().and_then(|o| o.get("text")).and_then(Value::as_str)
            {
                let units: Vec<u16> = corrected.encode_utf16().collect();
                let (a, b) = (off as usize, (off + len) as usize);
                if b <= units.len() {
                    let mut out: Vec<u16> = units[..a].to_vec();
                    out.extend(best.encode_utf16());
                    out.extend_from_slice(&units[b..]);
                    corrected = String::from_utf16_lossy(&out);
                    changed = true;
                }
            }
        }
    }
    let options = if changed { vec![obj! { "text" => corrected, "score" => Value::Num(Number::Float(0.5)) }] } else { Vec::new() };
    Ok(Value::Arr(vec![obj! { "text" => text, "offset" => 0, "length" => text.encode_utf16().count() as i64, "options" => Value::Arr(options) }]))
}
