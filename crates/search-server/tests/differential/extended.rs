//! The extended grammar: what the base generator in `workload.rs` leaves out.
//!
//! More field types (a normalized keyword, `date_nanos`, `unsigned_long`, `byte`, `flattened`,
//! an exact `dense_vector`, `search_as_you_type`, `integer_range`), the rest of the Query DSL
//! (`terms_set`, multi-term queries on text with rewrites, scripts, `distance_feature`,
//! `more_like_this`, the span family, interval rules, `pinned`, nested inner hits, `knn`),
//! the remaining aggregations (significant / rare / multi terms, auto and variable-width
//! histograms, geo grids, sibling and parent pipelines, samplers, `global` and
//! `reverse_nested`, `top_metrics`, ...), search options (`search_after`, script and runtime
//! fields, `indices_boost`, `terminate_after`, sort options, highlighter options, rescore
//! modes) and other APIs (URI search, `_validate`, term vectors, `_terms_enum`, search
//! templates, `_analyze`, bulk updates and conflicts).
//!
//! Everything here draws from the generator's second stream ([`Gen::with_ext`] swaps it in),
//! and the base grammar only asks the second stream *whether* to extend, so turning a branch
//! on here never reshuffles the base grammar's draws for the rest of a case.

use super::*;

impl Gen<'_> {
    /// Runs `f` with the extension stream as the generator's stream.
    pub(super) fn with_ext<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        std::mem::swap(&mut self.rng, &mut self.ext);
        let out = f(self);
        std::mem::swap(&mut self.rng, &mut self.ext);
        out
    }

    /// Adds some of the extended field types to the schema.
    pub(super) fn extend_schema(&mut self) {
        let kinds =
            [Kind::KeywordLower, Kind::DateNanos, Kind::UnsignedLong, Kind::Byte, Kind::Flattened, Kind::DenseVector, Kind::SearchAsYouType, Kind::IntegerRange];
        for kind in kinds {
            if self.ext.chance(0.25) {
                add_fields(&mut self.ext, kind, 1, "", false, &mut self.schema.fields);
            }
        }
    }

    fn keyword_field(&mut self) -> Option<Field> {
        self.field(|f| matches!(f.kind, Kind::Keyword | Kind::KeywordLower))
    }

    fn num_field(&mut self) -> Option<Field> {
        self.field(|f| f.kind.numeric())
    }

    /// A painless expression for a numeric field's value, 0 when the doc has none.
    fn doc_value_expr(f: &Field) -> String {
        format!("(doc['{0}'].size() == 0 ? 0 : doc['{0}'].value)", f.path)
    }

    // ---- queries -------------------------------------------------------------------------

    pub(super) fn ext_leaf(&mut self) -> Option<Value> {
        Some(match self.rng.below(30) {
            0 => {
                let k = self.keyword_field()?;
                let terms: Vec<&str> = (0..3).map(|_| *self.rng.pick(KEYWORDS)).collect();
                let mut ts = json!({"terms": terms});
                match self.field(|f| matches!(f.kind, Kind::Integer | Kind::Short | Kind::Byte)) {
                    Some(n) if self.rng.chance(0.5) => ts["minimum_should_match_field"] = json!(n.path),
                    _ => ts["minimum_should_match_script"] = json!({"source": "Math.min(params.num_terms, 2)"}),
                }
                json!({"terms_set": {k.path: ts}})
            }
            1 => {
                // Multi-term queries on analyzed text, sometimes with a scoring rewrite.
                let t = self.field(|f| f.kind == Kind::Text)?;
                let w = self.word().to_lowercase();
                let mut q = match self.rng.below(4) {
                    0 => json!({"prefix": {t.path.clone(): {"value": &w[..2]}}}),
                    1 => json!({"wildcard": {t.path.clone(): {"value": format!("{}*{}", &w[..1], &w[w.len() - 1..])}}}),
                    2 => json!({"regexp": {t.path.clone(): {"value": format!("{}.*", &w[..2]), "flags": *self.rng.pick(&["ALL", "NONE", "COMPLEMENT|INTERVAL"])}}}),
                    _ => json!({"fuzzy": {t.path.clone(): {"value": w, "fuzziness": *self.rng.pick(&[json!(1), json!(2), json!("AUTO")]), "prefix_length": self.rng.below(2), "transpositions": self.rng.chance(0.5)}}}),
                };
                if self.rng.chance(0.4) {
                    let key = q.as_object().unwrap().keys().next().unwrap().clone();
                    q[&key][&t.path]["rewrite"] = json!(*self.rng.pick(&["constant_score", "scoring_boolean", "top_terms_2", "top_terms_boost_3", "top_terms_blended_freqs_2", "constant_score_blended"]));
                }
                q
            }
            2 => {
                let t = self.field(|f| f.kind.textual())?;
                let mut m = json!({"query": self.words(1, 3)});
                match self.rng.below(5) {
                    0 => {
                        m = json!({"query": "the of and", "zero_terms_query": *self.rng.pick(&["all", "none"]), "analyzer": "stop"});
                    }
                    1 => {
                        m["fuzziness"] = json!(1);
                        m["prefix_length"] = json!(1);
                        m["fuzzy_transpositions"] = json!(false);
                        m["max_expansions"] = json!(2);
                    }
                    2 => {
                        m["analyzer"] = json!(*self.rng.pick(&["whitespace", "keyword", "english"]));
                    }
                    3 => {
                        m["minimum_should_match"] = json!(*self.rng.pick(&["75%", "-1", "2<-25%"]));
                    }
                    _ => {
                        m["lenient"] = json!(true);
                        m["auto_generate_synonyms_phrase_query"] = json!(false);
                    }
                }
                json!({"match": {t.path: m}})
            }
            3 => {
                let k = self.keyword_field()?;
                match self.rng.below(3) {
                    0 => json!({"term": {k.path: {"value": *self.rng.pick(KEYWORDS), "case_insensitive": true}}}),
                    1 => json!({"range": {k.path: {"gte": *self.rng.pick(KEYWORDS), "lt": "omega"}}}),
                    _ => json!({"wildcard": {k.path: {"value": "*A*", "case_insensitive": self.rng.chance(0.5)}}}),
                }
            }
            4 => {
                let n = self.num_field()?;
                let v = self.rng.range(0, 60);
                json!({"script": {"script": {"source": format!("{} > params.v", Self::doc_value_expr(&n)), "params": {"v": v}}}})
            }
            5 => {
                let n = self.num_field()?;
                let source = match self.rng.below(3) {
                    0 => format!("_score * 2 + Math.abs({})", Self::doc_value_expr(&n)),
                    1 => format!("Math.log(2 + Math.abs({}))", Self::doc_value_expr(&n)),
                    _ => "saturation(_score + 1, 2)".to_string(),
                };
                json!({"script_score": {"query": self.query(2), "script": {"source": source}}})
            }
            6 => {
                if let Some(d) = self.field(|f| matches!(f.kind, Kind::Date | Kind::DateNanos))
                    && self.rng.chance(0.6)
                {
                    json!({"distance_feature": {"field": d.path, "pivot": *self.rng.pick(&["7d", "30d", "12h"]), "origin": iso(BASE + self.rng.range(0, 300) * DAY, true)}})
                } else {
                    let g = self.field(|f| f.kind == Kind::GeoPoint)?;
                    json!({"distance_feature": {"field": g.path, "pivot": "1000km", "origin": [20.0, 10.0]}})
                }
            }
            7 => {
                let fields: Vec<String> = self.schema.top(|f| f.kind == Kind::Text).iter().map(|f| f.path.clone()).collect();
                let mut mlt = json!({"fields": fields, "like": self.words(3, 6), "min_term_freq": 1, "min_doc_freq": 1});
                if self.rng.chance(0.3) {
                    let idx = self.target();
                    if let Some(id) = self.existing_id(idx) {
                        mlt["like"] = json!([{"_index": self.indices[idx], "_id": id}]);
                    }
                }
                if self.rng.chance(0.3) {
                    mlt["minimum_should_match"] = json!("30%");
                }
                json!({"more_like_this": mlt})
            }
            8 | 9 => self.span_query(0)?,
            10 => {
                let t = self.field(|f| f.kind == Kind::Text)?;
                json!({"intervals": {t.path: self.interval_rule(0)}})
            }
            11 => {
                let idx = self.target();
                let ids: Vec<String> = (0..2).filter_map(|_| self.existing_id(idx)).collect();
                json!({"pinned": {"ids": ids, "organic": self.query(2)}})
            }
            12 => {
                if !self.schema.has_nested {
                    return None;
                }
                let f = self.nested_field(|_| true)?;
                let inner = self.leaf_on(&f)?;
                let mut ih = json!({"size": 1 + self.rng.below(3)});
                if self.rng.chance(0.4) {
                    ih["_source"] = json!(false);
                }
                if self.rng.chance(0.3) {
                    ih["sort"] = json!([{"n.long0": "asc"}]);
                }
                if self.rng.chance(0.3) {
                    ih["highlight"] = json!({"fields": {"n.text0": {}}});
                }
                json!({"nested": {"path": "n", "query": inner, "score_mode": *self.rng.pick(&["avg", "sum", "max", "min", "none"]), "inner_hits": ih}})
            }
            13 => {
                let t = self.field(|f| f.kind == Kind::Text)?;
                let w = self.word().to_lowercase();
                let q = match self.rng.below(6) {
                    0 => format!("{w}~1"),
                    1 => format!("\"{}\"~2", self.words(2, 2)),
                    2 => format!("{}:({} OR {})^2", t.path, w, self.word()),
                    3 => format!("_exists_:{}", t.path),
                    4 => match self.num_field() {
                        Some(n) => format!("{}:[0 TO {}]", n.path, self.rng.range(1, 80)),
                        None => format!("{w} -{}", self.word()),
                    },
                    _ => format!("{}* AND NOT {}", &w[..2], self.word()),
                };
                let mut qs = json!({"query": q});
                if self.rng.chance(0.4) {
                    qs["fields"] = json!([t.path, format!("{}^3", t.path)]);
                }
                if self.rng.chance(0.3) {
                    qs["type"] = json!(*self.rng.pick(&["best_fields", "most_fields", "cross_fields", "phrase"]));
                }
                if self.rng.chance(0.2) {
                    qs["fuzziness"] = json!(1);
                }
                json!({"query_string": qs})
            }
            14 => {
                let w = self.word().to_lowercase();
                let q = match self.rng.below(4) {
                    0 => format!("{w}~1 {}*", &self.word().to_lowercase()[..2]),
                    1 => format!("\"{}\"~1", self.words(2, 2)),
                    2 => format!("({w} | {}) +{}", self.word(), self.word()),
                    _ => format!("-{w}"),
                };
                let mut s = json!({"query": q, "flags": *self.rng.pick(&["ALL", "OR|AND|PREFIX", "PHRASE|SLOP|FUZZY", "NONE"])});
                if self.rng.chance(0.3) {
                    s["default_operator"] = json!("and");
                }
                if self.rng.chance(0.3) {
                    s["minimum_should_match"] = json!("2");
                }
                json!({"simple_query_string": s})
            }
            15 => {
                let fields: Vec<String> = self.schema.top(|f| f.kind == Kind::Text).iter().map(|f| f.path.clone()).collect();
                json!({"combined_fields": {"query": self.words(1, 3), "fields": fields, "operator": "and", "minimum_should_match": "1"}})
            }
            16 => {
                if self.schema.has_object && self.rng.chance(0.5) {
                    json!({"exists": {"field": "o"}})
                } else {
                    json!({"match_none": {}})
                }
            }
            17 => {
                let k = self.field(|f| f.kind == Kind::KeywordLower)?;
                let v = *self.rng.pick(KEYWORDS);
                match self.rng.below(3) {
                    0 => json!({"term": {k.path: v}}),
                    1 => json!({"prefix": {k.path: {"value": "AL"}}}),
                    _ => json!({"terms": {k.path: [v, "ALPHA"]}}),
                }
            }
            18 => {
                let d = self.field(|f| f.kind == Kind::DateNanos)?;
                let (a, b) = (self.date_ms(), self.date_ms());
                json!({"range": {d.path: {"gte": iso(a.min(b), false), "lte": format!("{}.999999999Z", iso(a.max(b), false).trim_end_matches('Z'))}}})
            }
            19 => {
                let u = self.field(|f| f.kind == Kind::UnsignedLong)?;
                match self.rng.below(3) {
                    0 => json!({"range": {u.path: {"gte": 9_223_372_036_854_775_808u64}}}),
                    1 => json!({"term": {u.path: u64::MAX}}),
                    _ => json!({"range": {u.path: {"gt": 10, "lte": 100}}}),
                }
            }
            20 => {
                let f = self.field(|f| f.kind == Kind::Flattened)?;
                match self.rng.below(4) {
                    0 => json!({"term": {format!("{}.a", f.path): *self.rng.pick(KEYWORDS)}}),
                    1 => json!({"term": {f.path: *self.rng.pick(KEYWORDS)}}),
                    2 => json!({"prefix": {format!("{}.b.c", f.path): "a"}}),
                    _ => json!({"exists": {"field": format!("{}.b", f.path)}}),
                }
            }
            21 => {
                let s = self.field(|f| f.kind == Kind::SearchAsYouType)?;
                let p = s.path;
                let text = format!("{} {}", self.word().to_lowercase(), &self.word().to_lowercase()[..2]);
                match self.rng.below(2) {
                    0 => json!({"multi_match": {"query": text, "type": "bool_prefix", "fields": [p.clone(), format!("{p}._2gram"), format!("{p}._3gram")]}}),
                    _ => json!({"match_phrase_prefix": {p: text}}),
                }
            }
            22 => {
                let r = self.field(|f| f.kind == Kind::IntegerRange)?;
                match self.rng.below(2) {
                    0 => json!({"term": {r.path: self.rng.range(-20, 110)}}),
                    _ => {
                        let lo = self.rng.range(-20, 80);
                        json!({"range": {r.path: {"gte": lo, "lte": lo + self.rng.range(0, 40), "relation": *self.rng.pick(&["intersects", "within", "contains"])}}})
                    }
                }
            }
            23 => {
                let v = self.field(|f| f.kind == Kind::DenseVector)?;
                let qv = json!([1.0, self.rng.range(-200, 200) as f64 / 100.0, 0.5]);
                if self.rng.chance(0.5) {
                    let mut k = json!({"field": v.path, "query_vector": qv, "k": 1 + self.rng.below(5), "num_candidates": 10});
                    if self.rng.chance(0.3) {
                        k["filter"] = self.query(2);
                    }
                    if self.rng.chance(0.2) {
                        k["similarity"] = json!(2.5);
                    }
                    json!({"knn": k})
                } else {
                    let f = *self.rng.pick(&["cosineSimilarity(params.v, '%') + 1.0", "1 / (1 + l2norm(params.v, '%'))", "dotProduct(params.v, '%') + 100"]);
                    let src = f.replace('%', &v.path);
                    json!({"script_score": {"query": {"exists": {"field": v.path}}, "script": {"source": src, "params": {"v": qv}}}})
                }
            }
            24 => {
                let g = self.field(|f| f.kind == Kind::GeoPoint)?;
                json!({"geo_distance": {"distance": "2000km", "distance_type": *self.rng.pick(&["arc", "plane"]), g.path: [20.0, 10.0]}})
            }
            25 => {
                let k = self.keyword_field()?;
                json!({"regexp": {k.path: {"value": *self.rng.pick(&["a.*a", "x-[0-9]", "[a-g]+", ".*@.*", ""]), "case_insensitive": self.rng.chance(0.3)}}})
            }
            26 => {
                let t = self.field(|f| f.kind == Kind::Text)?;
                json!({"match_phrase": {t.path: {"query": self.words(2, 3), "slop": self.rng.below(4), "zero_terms_query": "all"}}})
            }
            27 => {
                let t = self.field(|f| f.kind == Kind::Text)?;
                json!({"match_phrase_prefix": {t.path: {"query": format!("{} {}", self.word(), &self.word()[..1]), "max_expansions": 2, "slop": 1}}})
            }
            28 => {
                let n = self.num_field()?;
                json!({"bool": {"filter": [{"range": {n.path.clone(): {"gte": 0}}}], "should": [self.query(2)], "minimum_should_match": 0, "boost": 0.5}})
            }
            _ => {
                let fields: Vec<Value> = self.schema.top(|g| g.kind == Kind::Text).iter().map(|g| json!(format!("{}^{}", g.path, 1 + self.rng.below(3)))).collect();
                json!({"multi_match": {"query": self.words(1, 3), "fields": fields, "type": *self.rng.pick(&["best_fields", "most_fields", "cross_fields", "phrase"]), "fuzziness": *self.rng.pick(&[json!(null), json!("AUTO"), json!(1)]), "tie_breaker": 0.5}})
            }
        })
    }

    /// A span query over one text field.
    fn span_query(&mut self, depth: usize) -> Option<Value> {
        let t = self.field(|f| f.kind == Kind::Text)?;
        let term = |g: &mut Self| json!({"span_term": {t.path.clone(): g.word().to_lowercase()}});
        if depth >= 2 {
            return Some(term(self));
        }
        Some(match self.rng.below(8) {
            0 => json!({"span_or": {"clauses": [self.span_query(depth + 1)?, term(self)]}}),
            1 => json!({"span_first": {"match": self.span_query(depth + 1)?, "end": 1 + self.rng.below(4)}}),
            2 => json!({"span_not": {"include": self.span_query(depth + 1)?, "exclude": term(self), "pre": self.rng.below(2), "post": self.rng.below(2)}}),
            3 => json!({"span_containing": {"big": {"span_near": {"clauses": [term(self), term(self)], "slop": 3, "in_order": false}}, "little": term(self)}}),
            4 => json!({"span_within": {"big": {"span_near": {"clauses": [term(self), term(self)], "slop": 3, "in_order": true}}, "little": term(self)}}),
            5 => {
                let w = self.word().to_lowercase();
                json!({"span_multi": {"match": {"prefix": {t.path.clone(): {"value": &w[..2]}}}}})
            }
            6 => json!({"span_near": {"clauses": [self.span_query(depth + 1)?, self.span_query(depth + 1)?], "slop": self.rng.below(3), "in_order": self.rng.chance(0.5)}}),
            _ => json!({"span_field_masking": {"query": term(self), "field": t.path}}),
        })
    }

    /// An `intervals` rule.
    fn interval_rule(&mut self, depth: usize) -> Value {
        let w = self.word().to_lowercase();
        if depth >= 2 {
            return json!({"match": {"query": w}});
        }
        match self.rng.below(7) {
            0 => json!({"all_of": {"intervals": [self.interval_rule(depth + 1), self.interval_rule(depth + 1)], "ordered": self.rng.chance(0.5), "max_gaps": self.rng.below(4)}}),
            1 => json!({"any_of": {"intervals": [self.interval_rule(depth + 1), self.interval_rule(depth + 1)]}}),
            2 => json!({"prefix": {"prefix": &w[..2]}}),
            3 => json!({"wildcard": {"pattern": format!("{}*", &w[..2])}}),
            4 => json!({"fuzzy": {"term": w, "fuzziness": 1}}),
            5 => {
                let mut m = json!({"query": self.words(1, 3), "max_gaps": self.rng.below(3), "ordered": self.rng.chance(0.5)});
                if self.rng.chance(0.4) {
                    m["filter"] = json!({*self.rng.pick(&["not_containing", "containing", "not_overlapping", "before", "after"]): {"match": {"query": self.word().to_lowercase()}}});
                }
                json!({"match": m})
            }
            _ => json!({"match": {"query": w}}),
        }
    }

    // ---- aggregations --------------------------------------------------------------------

    pub(super) fn ext_agg(&mut self, depth: usize, in_nested: bool) -> Option<(String, Value)> {
        let kw = |g: &mut Self| g.agg_field(in_nested, |f| matches!(f.kind, Kind::Keyword | Kind::KeywordLower));
        let num = |g: &mut Self| g.agg_field(in_nested, |f| f.kind.numeric());
        let (kind, body): (&str, Value) = match self.rng.below(32) {
            0 => {
                let k = kw(self)?;
                let mut st = json!({"field": k.path, "min_doc_count": 1});
                if self.rng.chance(0.3) {
                    st["background_filter"] = self.query(2);
                }
                if self.rng.chance(0.3) {
                    st[*self.rng.pick(&["jlh", "chi_square", "gnd", "percentage", "mutual_information"])] = json!({});
                }
                ("significant_terms", json!({"significant_terms": st}))
            }
            1 => {
                let k = self.agg_field(in_nested, |f| matches!(f.kind, Kind::Keyword | Kind::Long | Kind::Integer))?;
                ("rare_terms", json!({"rare_terms": {"field": k.path, "max_doc_count": 1 + self.rng.below(2)}}))
            }
            2 => {
                let a = kw(self)?;
                let b = self.agg_field(in_nested, |f| f.kind.integral() || f.kind == Kind::Keyword)?;
                let mut mt = json!({"terms": [{"field": a.path}, {"field": b.path, "missing": 0}]});
                if self.rng.chance(0.4) {
                    mt["size"] = json!(3);
                }
                if self.rng.chance(0.3) {
                    mt["order"] = json!({"_key": "asc"});
                }
                ("multi_terms", json!({"multi_terms": mt}))
            }
            3 => {
                let d = self.agg_field(in_nested, |f| matches!(f.kind, Kind::Date | Kind::DateNanos))?;
                ("auto_date_histogram", json!({"auto_date_histogram": {"field": d.path, "buckets": 1 + self.rng.below(8), "format": "yyyy-MM-dd"}}))
            }
            4 => {
                let n = num(self)?;
                ("variable_width_histogram", json!({"variable_width_histogram": {"field": n.path, "buckets": 1 + self.rng.below(4)}}))
            }
            5 => {
                let ip = self.agg_field(in_nested, |f| f.kind == Kind::Ip)?;
                let keyed = self.rng.chance(0.3);
                ("ip_range", json!({"ip_range": {"field": ip.path, "keyed": keyed, "ranges": [{"to": "[IP]"}, {"from": "[IP]", "to": "[IP]"}, {"mask": "[IP]/16"}]}}))
            }
            6 => {
                let g = self.agg_field(in_nested, |f| f.kind == Kind::GeoPoint)?;
                let kind = *self.rng.pick(&["geohash_grid", "geotile_grid"]);
                let precision = if kind == "geohash_grid" { 1 + self.rng.below(2) } else { 1 + self.rng.below(4) };
                (kind, json!({kind: {"field": g.path, "precision": precision, "size": 5}}))
            }
            7 => {
                let g = self.agg_field(in_nested, |f| f.kind == Kind::GeoPoint)?;
                ("geo_distance", json!({"geo_distance": {"field": g.path, "origin": "10, 20", "unit": "km", "ranges": [{"to": 2000}, {"from": 2000, "to": 6000}, {"from": 6000}]}}))
            }
            8 => {
                let g = self.agg_field(in_nested, |f| f.kind == Kind::GeoPoint)?;
                ("geo_centroid", json!({"geo_centroid": {"field": g.path}}))
            }
            9 => {
                let n = num(self)?;
                ("percentile_ranks", json!({"percentile_ranks": {"field": n.path, "values": [0, 25, 50.5]}}))
            }
            10 => {
                let n = num(self)?;
                ("median_absolute_deviation", json!({"median_absolute_deviation": {"field": n.path}}))
            }
            11 => {
                let k = kw(self)?;
                ("string_stats", json!({"string_stats": {"field": k.path, "show_distribution": self.rng.chance(0.5)}}))
            }
            12 => {
                let n = num(self)?;
                ("boxplot", json!({"boxplot": {"field": n.path}}))
            }
            13 => {
                let n = num(self)?;
                // Not on dates: Elasticsearch 9.5.5 drops the connection when a document
                // lacks the date it sorts by (an exception on its network thread).
                let s = self.agg_field(in_nested, |f| matches!(f.kind, Kind::Long | Kind::Integer))?;
                ("top_metrics", json!({"top_metrics": {"metrics": [{"field": n.path}], "sort": {s.path: *self.rng.pick(&["asc", "desc"])}, "size": 1 + self.rng.below(2)}}))
            }
            14 => {
                let n = num(self)?;
                let kind = *self.rng.pick(&["avg", "sum", "max", "value_count"]);
                (kind, json!({kind: {"script": {"source": format!("{} * 2", Self::doc_value_expr(&n))}}}))
            }
            15 => {
                // Sibling pipelines over a histogram's or terms' buckets.
                let n = num(self)?;
                let inner_kind = *self.rng.pick(&["avg", "sum", "max"]);
                let hist = if self.rng.chance(0.5) {
                    json!({"histogram": {"field": n.path, "interval": 20}, "aggs": {"m": {inner_kind: {"field": n.path}}}})
                } else {
                    let k = kw(self)?;
                    json!({"terms": {"field": k.path}, "aggs": {"m": {inner_kind: {"field": n.path}}}})
                };
                let pipe = *self.rng.pick(&["avg_bucket", "max_bucket", "min_bucket", "sum_bucket", "stats_bucket", "extended_stats_bucket", "percentiles_bucket"]);
                let path = if self.rng.chance(0.5) { "h>_count" } else { "h>m" };
                let mut pb = json!({"buckets_path": path});
                if self.rng.chance(0.3) {
                    pb["gap_policy"] = json!(*self.rng.pick(&["skip", "insert_zeros", "keep_values"]));
                }
                let name = self.agg_name("sibling");
                ("filter", json!({"filter": {"match_all": {}}, "aggs": {"h": hist, name: {pipe: pb}}}))
            }
            16 => {
                // Parent pipelines inside a histogram.
                let n = self.agg_field(in_nested, |f| f.kind.numeric())?;
                let mut aggs = Map::new();
                aggs.insert("m".into(), json!({"sum": {"field": n.path}}));
                let p = match self.rng.below(5) {
                    0 => json!({"bucket_script": {"buckets_path": {"c": "_count", "s": "m"}, "script": "params.s / params.c"}}),
                    1 => json!({"moving_fn": {"buckets_path": "m", "window": 2 + self.rng.below(3), "script": *self.rng.pick(&["MovingFunctions.unweightedAvg(values)", "MovingFunctions.max(values)", "MovingFunctions.sum(values)", "MovingFunctions.linearWeightedAvg(values)"])}}),
                    2 => json!({"serial_diff": {"buckets_path": "m", "lag": 1 + self.rng.below(2)}}),
                    3 => json!({"derivative": {"buckets_path": "m"}}),
                    _ => json!({"cumulative_sum": {"buckets_path": "m"}}),
                };
                aggs.insert("p".into(), p);
                let mut h = json!({"field": n.path, "interval": *self.rng.pick(&[10, 25, 100])});
                if self.rng.chance(0.4) {
                    h["extended_bounds"] = json!({"min": -50, "max": 250});
                }
                ("histogram", json!({"histogram": h, "aggs": Value::Object(aggs)}))
            }
            17 => {
                let d = self.agg_field(in_nested, |f| f.kind == Kind::Date)?;
                let k = self.agg_field(in_nested, |f| f.kind == Kind::Keyword || f.kind.integral());
                let mut aggs = json!({"c": {"cumulative_cardinality": {"buckets_path": "card"}}});
                aggs["card"] = json!({"cardinality": {"field": k.map(|k| k.path).unwrap_or_else(|| d.path.clone())}});
                ("date_histogram", json!({"date_histogram": {"field": d.path, "calendar_interval": "month", "min_doc_count": 0}, "aggs": aggs}))
            }
            18 => {
                if in_nested {
                    return None;
                }
                let subs = self.aggs(depth + 1, false);
                ("global", json!({"global": {}, "aggs": subs}))
            }
            19 => {
                if !in_nested {
                    return None;
                }
                let k = self.field(|f| f.kind == Kind::Keyword)?;
                ("reverse_nested", json!({"reverse_nested": {}, "aggs": {"top": {"terms": {"field": k.path}}}}))
            }
            20 => {
                let subs = self.aggs(depth + 1, in_nested);
                if self.rng.chance(0.5) {
                    ("sampler", json!({"sampler": {"shard_size": 1 + self.rng.below(10)}, "aggs": subs}))
                } else {
                    let k = kw(self)?;
                    ("diversified_sampler", json!({"diversified_sampler": {"shard_size": 1 + self.rng.below(10), "field": k.path, "max_docs_per_value": 1 + self.rng.below(2)}, "aggs": subs}))
                }
            }
            21 => {
                let k = self.agg_field(in_nested, |f| matches!(f.kind, Kind::Keyword | Kind::KeywordLower | Kind::Long | Kind::Integer))?;
                let mut t = json!({"field": k.path});
                if matches!(k.kind, Kind::Keyword | Kind::KeywordLower) {
                    match self.rng.below(3) {
                        0 => t["include"] = json!("(a|b|g).*"),
                        1 => t["exclude"] = json!(["alpha", "pi"]),
                        _ => t["include"] = json!({"partition": self.rng.below(2), "num_partitions": 2}),
                    }
                } else {
                    t["include"] = json!({"partition": self.rng.below(3), "num_partitions": 3});
                }
                if let Some(n) = num(self)
                    && self.rng.chance(0.5)
                {
                    t["order"] = json!({"m": *self.rng.pick(&["asc", "desc"])});
                    t["size"] = json!(2 + self.rng.below(3));
                    t["shard_size"] = json!(2 + self.rng.below(3));
                    return Some((self.agg_name("terms"), json!({"terms": t, "aggs": {"m": {"avg": {"field": n.path}}}})));
                }
                if self.rng.chance(0.3) {
                    t["execution_hint"] = json!(*self.rng.pick(&["map", "global_ordinals"]));
                }
                if self.rng.chance(0.3) {
                    t["min_doc_count"] = json!(0);
                }
                ("terms", json!({"terms": t}))
            }
            22 => {
                let n = num(self)?;
                let mut h = json!({"field": n.path, "interval": 25, "keyed": self.rng.chance(0.3)});
                if self.rng.chance(0.5) {
                    h["hard_bounds"] = json!({"min": 0, "max": 100});
                } else {
                    h["extended_bounds"] = json!({"min": -100, "max": 300});
                    h["min_doc_count"] = json!(0);
                }
                ("histogram", json!({"histogram": h}))
            }
            23 => {
                let d = self.agg_field(in_nested, |f| matches!(f.kind, Kind::Date | Kind::DateNanos))?;
                let mut h = json!({"field": d.path, "calendar_interval": *self.rng.pick(&["month", "quarter", "year"]), "min_doc_count": 0, "extended_bounds": {"min": "2023-11-01", "max": "2025-06-01"}});
                if self.rng.chance(0.3) {
                    h["keyed"] = json!(true);
                    h["format"] = json!("yyyy-MM");
                }
                if self.rng.chance(0.3) {
                    h["offset"] = json!("+1d");
                }
                ("date_histogram", json!({"date_histogram": h}))
            }
            24 => {
                let n = num(self)?;
                ("range", json!({"range": {"field": n.path, "keyed": true, "ranges": [{"key": "low", "to": 10}, {"key": "mid", "from": 10, "to": 60.5}, {"from": 60.5}]}}))
            }
            25 => {
                let d = self.agg_field(in_nested, |f| f.kind == Kind::Date)?;
                ("date_range", json!({"date_range": {"field": d.path, "format": "yyyy-MM", "keyed": self.rng.chance(0.5), "ranges": [{"to": "2024-04"}, {"from": "2024-04", "to": "2025-01"}], "time_zone": "+01:00"}}))
            }
            26 => {
                let f = self.agg_field(in_nested, |f| f.kind.doc_values() && f.kind != Kind::GeoPoint)?;
                ("cardinality", json!({"cardinality": {"field": f.path, "precision_threshold": *self.rng.pick(&[1, 10, 3000])}}))
            }
            27 => {
                let (a, b, c) = (self.query(2), self.query(2), self.query(2));
                ("adjacency_matrix", json!({"adjacency_matrix": {"filters": {"a": a, "b": b, "c": c}}}))
            }
            28 => {
                if in_nested {
                    return None;
                }
                let d = self.field(|f| f.kind == Kind::Date)?;
                let mut sources = vec![json!({"d": {"date_histogram": {"field": d.path, "calendar_interval": "month", "format": "yyyy-MM", "order": "desc"}}})];
                let keyed = match self.keyword_field() {
                    Some(k) => {
                        sources.push(json!({"k": {"terms": {"field": k.path, "missing_bucket": true, "missing_order": "last"}}}));
                        true
                    }
                    None => false,
                };
                let mut c = json!({"size": 2 + self.rng.below(3), "sources": sources});
                if self.rng.chance(0.4) {
                    c["after"] = if keyed { json!({"d": "2024-06", "k": "beta"}) } else { json!({"d": "2024-06"}) };
                }
                ("composite", json!({"composite": c}))
            }
            29 => {
                let f = self.field(|f| matches!(f.kind, Kind::KeywordLower | Kind::DateNanos | Kind::UnsignedLong | Kind::Byte))?;
                let kind = if matches!(f.kind, Kind::KeywordLower) { "terms" } else { *self.rng.pick(&["terms", "stats", "max", "percentiles"]) };
                (kind, json!({kind: {"field": f.path}}))
            }
            30 => {
                let f = self.field(|f| f.kind == Kind::Flattened)?;
                ("terms", json!({"terms": {"field": format!("{}.a", f.path)}}))
            }
            _ => {
                let r = self.field(|f| f.kind == Kind::IntegerRange)?;
                if self.rng.chance(0.5) {
                    ("histogram", json!({"histogram": {"field": r.path, "interval": 20}}))
                } else {
                    ("range", json!({"range": {"field": r.path, "ranges": [{"to": 20}, {"from": 20, "to": 60}, {"from": 60}]}}))
                }
            }
        };
        Some((self.agg_name(kind), body))
    }

    // ---- search options ------------------------------------------------------------------

    pub(super) fn ext_search(&mut self, b: &mut Map<String, Value>, sorted: bool) {
        match self.rng.below(16) {
            0 => {
                let f = self.schema.fields[self.rng.below(self.schema.fields.len())].path.clone();
                b.insert("stored_fields".into(), self.rng.pick(&[json!("_none_"), json!([f]), json!(["*"])]).clone());
            }
            1 => {
                if let Some(n) = self.num_field() {
                    b.insert("script_fields".into(), json!({"twice": {"script": {"source": format!("{} * 2", Self::doc_value_expr(&n))}}, "has": {"script": format!("doc['{}'].size()", n.path)}}));
                }
            }
            2 => {
                if let Some(n) = self.num_field() {
                    let rt = json!({"rt": {"type": "double", "script": {"source": format!("emit({} + 0.5)", Self::doc_value_expr(&n))}}});
                    b.insert("runtime_mappings".into(), rt);
                    match self.rng.below(3) {
                        0 => {
                            b.insert("fields".into(), json!(["rt"]));
                        }
                        1 => {
                            b.insert("aggs".into(), json!({"rt_stats": {"stats": {"field": "rt"}}}));
                        }
                        _ => {
                            b.insert("query".into(), json!({"range": {"rt": {"gte": 10}}}));
                        }
                    }
                }
            }
            3 => {
                if self.indices.len() > 1 {
                    b.insert("indices_boost".into(), json!([{self.indices[1].clone(): 2.0}, {self.indices[0].clone(): 0.5}]));
                }
            }
            4 => {
                b.insert("terminate_after".into(), json!(1 + self.rng.below(5)));
            }
            5 => {
                if sorted {
                    b.insert("track_scores".into(), json!(true));
                } else {
                    let missing = format!("missing_{}", self.rng.below(3));
                    b.insert("sort".into(), json!([{missing: {"order": "asc", "unmapped_type": *self.rng.pick(&["long", "keyword", "date"])}}, "_score"]));
                }
            }
            6 => {
                if let Some(n) = self.field(|f| f.kind.numeric() || f.kind == Kind::Date) {
                    let mut s = json!({"order": *self.rng.pick(&["asc", "desc"])});
                    if n.kind == Kind::Date {
                        s["format"] = json!(*self.rng.pick(&["epoch_second", "yyyy-MM-dd", "strict_date_optional_time_nanos"]));
                    } else {
                        s["numeric_type"] = json!(*self.rng.pick(&["long", "double", "date", "date_nanos"]));
                    }
                    b.insert("sort".into(), json!([{n.path: s}]));
                }
            }
            7 => {
                if let Some(n) = self.num_field() {
                    b.insert("sort".into(), json!([{"_script": {"type": "number", "script": {"source": Self::doc_value_expr(&n)}, "order": *self.rng.pick(&["asc", "desc"])}}]));
                }
            }
            8 => {
                if self.schema.has_nested {
                    let mut s = json!({"order": *self.rng.pick(&["asc", "desc"]), "mode": *self.rng.pick(&["min", "max", "avg", "sum"]), "nested": {"path": "n"}});
                    if self.rng.chance(0.5) {
                        s["nested"]["filter"] = json!({"range": {"n.long0": {"gte": 0}}});
                    }
                    b.insert("sort".into(), json!([{"n.long0": s}]));
                }
            }
            9 => {
                if let Some(t) = self.field(|f| f.kind == Kind::Text) {
                    let mut fh = Map::new();
                    match self.rng.below(6) {
                        0 => {
                            fh.insert("type".into(), json!("unified"));
                            fh.insert("boundary_scanner".into(), json!(*self.rng.pick(&["sentence", "word"])));
                            fh.insert("fragment_size".into(), json!(15));
                        }
                        1 => {
                            fh.insert("type".into(), json!("plain"));
                            fh.insert("fragmenter".into(), json!(*self.rng.pick(&["simple", "span"])));
                            fh.insert("fragment_size".into(), json!(18));
                        }
                        2 => {
                            fh.insert("no_match_size".into(), json!(12));
                        }
                        3 => {
                            fh.insert("highlight_query".into(), json!({"match": {t.path.clone(): self.words(1, 2)}}));
                        }
                        4 => {
                            fh.insert("order".into(), json!("score"));
                            fh.insert("number_of_fragments".into(), json!(1));
                            fh.insert("fragment_size".into(), json!(10));
                        }
                        _ => {
                            fh.insert("require_field_match".into(), json!(false));
                        }
                    }
                    let mut h = json!({"fields": {t.path.clone(): Value::Object(fh)}});
                    if self.rng.chance(0.3) {
                        h["encoder"] = json!("html");
                    }
                    if self.rng.chance(0.2) {
                        h["fields"]["*"] = json!({});
                    }
                    if !b.contains_key("query") {
                        b.insert("query".into(), json!({"match": {t.path: self.words(1, 2)}}));
                    }
                    b.insert("highlight".into(), h);
                }
            }
            10 => {
                if !sorted {
                    let window = 1 + self.rng.below(6);
                    let mode = *self.rng.pick(&["total", "multiply", "avg", "max", "min"]);
                    let (qw, rw) = (*self.rng.pick(&[1.0, 0.5, 2.0]), *self.rng.pick(&[1.0, 0.1, 3.0]));
                    b.insert("rescore".into(), json!({"window_size": window, "query": {"rescore_query": self.query(2), "score_mode": mode, "query_weight": qw, "rescore_query_weight": rw}}));
                }
            }
            11 => {
                if !sorted && let Some(k) = self.field(|f| f.kind == Kind::Keyword) {
                    let mut ih = json!({"name": "g", "size": 2});
                    if let Some(n) = self.num_field() {
                        ih["sort"] = json!([{n.path: "desc"}]);
                    }
                    b.insert("collapse".into(), json!({"field": k.path, "inner_hits": [ih, {"name": "h", "size": 1, "_source": false}], "max_concurrent_group_searches": 2}));
                }
            }
            12 => {
                let src = match self.rng.below(3) {
                    0 => json!({"includes": ["o.*", "kw*"]}),
                    1 => json!(["*0"]),
                    _ => json!({"includes": ["*"], "excludes": ["text*"]}),
                };
                b.insert("_source".into(), src);
            }
            13 => {
                let mut fs = vec![json!("*")];
                if let Some(d) = self.field(|f| f.kind == Kind::Date) {
                    fs = vec![json!({"field": d.path, "format": "epoch_millis"}), json!("kw*")];
                }
                b.insert("fields".into(), json!(fs));
            }
            14 => {
                if let Some(d) = self.field(|f| matches!(f.kind, Kind::Date | Kind::DateNanos | Kind::Double)) {
                    let format = if d.kind == Kind::Double { "#.0" } else { "yyyy-MM-dd'T'HH" };
                    b.insert("docvalue_fields".into(), json!([{"field": d.path, "format": format}]));
                }
            }
            _ => {
                b.insert("stats".into(), json!(["g1"]));
                b.insert("version".into(), json!(true));
            }
        }
    }

    // ---- other APIs ----------------------------------------------------------------------

    pub(super) fn ext_steps(&mut self) -> Vec<Step> {
        let idx = self.target();
        let name = self.indices[idx].clone();
        match self.rng.below(16) {
            0 => {
                // `search_after` over a field sort, continuing from the first page's last hit.
                let Some(f) = self.field(|f| f.kind.doc_values() && !matches!(f.kind, Kind::GeoPoint | Kind::Wildcard | Kind::Boolean)) else { return Vec::new() };
                let mut sort = vec![json!({f.path: {"order": *self.rng.pick(&["asc", "desc"]), "missing": *self.rng.pick(&["_first", "_last"])}})];
                if let Some(k) = self.keyword_field() {
                    sort.push(json!({k.path: "asc"}));
                }
                let path = self.search_path();
                let q = self.query(1);
                let size = 1 + self.rng.below(4);
                vec![
                    Step::new("POST", format!("/{path}/_search"), Body::Json(json!({"query": q, "size": size, "sort": sort}))).capture("after", "/hits/hits/-1/sort"),
                    Step::new("POST", format!("/{path}/_search"), Body::Json(json!({"query": q, "size": size, "sort": sort, "search_after": "${after}"}))),
                ]
            }
            1 => {
                let path = self.search_path();
                let mut q = format!("q={}", self.word().to_lowercase());
                if let Some(t) = self.field(|f| f.kind == Kind::Text)
                    && self.rng.chance(0.5)
                {
                    q.push_str(&format!("&df={}", t.path));
                }
                if let Some(n) = self.num_field()
                    && self.rng.chance(0.4)
                {
                    q.push_str(&format!("&sort={}:desc", n.path));
                }
                if self.rng.chance(0.3) {
                    q.push_str(&format!("&size={}&from=1", 1 + self.rng.below(3)));
                }
                if self.rng.chance(0.3) {
                    q.push_str("&default_operator=AND&analyze_wildcard=true");
                }
                if self.rng.chance(0.3) {
                    q.push_str("&track_total_hits=2&rest_total_hits_as_int=true");
                }
                vec![Step::new("GET", format!("/{path}/_search?{q}"), Body::None)]
            }
            2 => {
                let q = self.query(1);
                let opts = *self.rng.pick(&["explain=true", "explain=true&rewrite=true", "rewrite=true&all_shards=true", ""]);
                vec![Step::new("POST", format!("/{name}/_validate/query?{opts}"), Body::Json(json!({"query": q})))]
            }
            3 => {
                let Some(t) = self.field(|f| f.kind == Kind::Text) else { return Vec::new() };
                let id = self.existing_id(idx).unwrap_or_else(|| "1".into());
                let mut tv = json!({"fields": [t.path], "term_statistics": self.rng.chance(0.5), "field_statistics": self.rng.chance(0.5), "positions": true, "offsets": t.vectors});
                if self.rng.chance(0.3) {
                    tv["filter"] = json!({"max_num_terms": 3, "min_term_freq": 1, "min_doc_freq": 1});
                }
                if self.rng.chance(0.3) {
                    return vec![Step::new("POST", format!("/{name}/_termvectors"), Body::Json(json!({"doc": {t.path.clone(): self.words(2, 6)}, "fields": [t.path], "term_statistics": true})))];
                }
                vec![Step::new("POST", format!("/{name}/_termvectors/{id}"), Body::Json(tv))]
            }
            4 => {
                let Some(k) = self.field(|f| matches!(f.kind, Kind::Keyword | Kind::KeywordLower | Kind::Text)) else { return Vec::new() };
                let mut te = json!({"field": k.path, "string": *self.rng.pick(&["", "a", "x-", "B", "q"])});
                if self.rng.chance(0.3) {
                    te["case_insensitive"] = json!(true);
                }
                if self.rng.chance(0.3) {
                    te["index_filter"] = self.query(2);
                }
                if self.rng.chance(0.3) {
                    te["size"] = json!(2);
                    te["search_after"] = json!("alpha");
                }
                let path = self.search_path();
                vec![Step::new("POST", format!("/{path}/_terms_enum"), Body::Json(te))]
            }
            5 => {
                let id = self.existing_id(idx).unwrap_or_else(|| "1".into());
                let q = self.query(1);
                vec![Step::new("POST", format!("/{name}/_explain/{id}"), Body::Json(json!({"query": q})))]
            }
            6 => {
                // Bulk with every action, including ones that fail per item.
                let mut lines = Vec::new();
                for _ in 0..(2 + self.rng.below(4)) {
                    let id = self.existing_id(idx).unwrap_or_else(|| "nope".into());
                    match self.rng.below(5) {
                        0 => {
                            lines.push(json!({"update": {"_index": name, "_id": id}}));
                            let f = self.field(|f| !f.path.contains('.')).unwrap();
                            let v = self.scalar(f.kind);
                            lines.push(json!({"doc": {f.path: v}, "detect_noop": self.rng.chance(0.5)}));
                        }
                        1 => {
                            lines.push(json!({"update": {"_index": name, "_id": "missing-x", "retry_on_conflict": 1}}));
                            lines.push(json!({"doc": {"kw0": "alpha"}}));
                        }
                        2 => {
                            lines.push(json!({"create": {"_index": name, "_id": id}}));
                            lines.push(self.doc());
                        }
                        3 => {
                            lines.push(json!({"delete": {"_index": name, "_id": "missing-y"}}));
                        }
                        _ => {
                            let new = self.new_id();
                            lines.push(json!({"index": {"_index": name, "_id": new, "if_seq_no": 0, "if_primary_term": 1}}));
                            lines.push(self.doc());
                        }
                    }
                }
                vec![Step::new("POST", "/_bulk?refresh=true", Body::Lines(lines))]
            }
            7 => {
                let id = self.existing_id(idx).unwrap_or_else(|| "1".into());
                let doc = self.doc();
                match self.rng.below(3) {
                    0 => vec![Step::new("PUT", format!("/{name}/_create/{id}"), Body::Json(doc))],
                    1 => vec![Step::new("PUT", format!("/{name}/_doc/{id}?if_seq_no=999&if_primary_term=1"), Body::Json(doc))],
                    _ => vec![Step::new("PUT", format!("/{name}/_doc/{id}?version=1&version_type=external"), Body::Json(doc))],
                }
            }
            8 => {
                let a = self.existing_id(idx).unwrap_or_else(|| "1".into());
                let f = self.schema.fields[self.rng.below(self.schema.fields.len())].path.clone();
                let docs = json!([
                    {"_index": name, "_id": a, "_source": [f.clone()]},
                    {"_index": name, "_id": "nope", "routing": "r1"},
                    {"_index": name, "_id": a, "stored_fields": [f], "_source": false},
                ]);
                vec![Step::new("POST", "/_mget", Body::Json(json!({"docs": docs})))]
            }
            9 => {
                let path = self.search_path();
                let mut q = format!("q={}", self.word().to_lowercase());
                if self.rng.chance(0.5) {
                    q.push_str("&terminate_after=1");
                }
                if let Some(k) = self.keyword_field()
                    && self.rng.chance(0.4)
                {
                    q = format!("q={}:{}&lenient=true", k.path, self.rng.pick(KEYWORDS));
                }
                vec![Step::new("GET", format!("/{path}/_count?{q}"), Body::None)]
            }
            10 => {
                let id = self.existing_id(idx).unwrap_or_else(|| self.new_id());
                let Some(n) = self.field(|f| f.kind.integral() && !f.path.contains('.')) else { return Vec::new() };
                let p = n.path;
                let body = match self.rng.below(3) {
                    0 => json!({"script": {"source": format!("ctx._source.{p} = params.v"), "params": {"v": 7}}, "upsert": {p.clone(): 1}}),
                    1 => json!({"scripted_upsert": true, "script": {"source": format!("if (ctx._source.{p} == null) {{ ctx._source.{p} = 0 }} ctx._source.{p} += 1")}, "upsert": {}}),
                    _ => json!({"script": {"source": "ctx.op = params.op", "params": {"op": *self.rng.pick(&["noop", "delete", "none"])}}}),
                };
                vec![Step::new("POST", format!("/{name}/_update/{id}?refresh=true"), Body::Json(body))]
            }
            11 => {
                let Some(v) = self.field(|f| f.kind == Kind::DenseVector) else { return Vec::new() };
                let path = self.search_path();
                let mut knn = json!({"field": v.path, "query_vector": [1.5, self.rng.range(-200, 200) as f64 / 100.0, -0.5], "k": 1 + self.rng.below(4), "num_candidates": 10});
                if self.rng.chance(0.3) {
                    knn["boost"] = json!(2.0);
                }
                let mut body = json!({"knn": knn, "size": 1 + self.rng.below(5)});
                if self.rng.chance(0.4) {
                    body["query"] = self.query(1);
                }
                vec![Step::new("POST", format!("/{path}/_search"), Body::Json(body))]
            }
            12 => {
                let path = self.search_path();
                let body = json!({"index_filter": {"range": {"date0": {"gte": "2024-06-01"}}}});
                vec![Step::new("POST", format!("/{path}/_field_caps?fields=*&include_unmapped={}", self.rng.chance(0.5)), Body::Json(body))]
            }
            13 => {
                let Some(t) = self.field(|f| f.kind == Kind::Text) else { return Vec::new() };
                let path = self.search_path();
                let body = json!({
                    "source": {"query": {"match": {"{{f}}": "{{q}}"}}, "size": "{{s}}"},
                    "params": {"f": t.path, "q": self.words(1, 2), "s": 1 + self.rng.below(4)}
                });
                vec![Step::new("POST", format!("/{path}/_search/template"), Body::Json(body))]
            }
            14 => {
                let Some(t) = self.field(|f| f.kind.textual() || f.kind == Kind::KeywordLower) else { return Vec::new() };
                let mut body = json!({"field": t.path, "text": self.words(1, 4)});
                if self.rng.chance(0.3) {
                    body["explain"] = json!(true);
                }
                vec![Step::new("POST", format!("/{name}/_analyze"), Body::Json(body))]
            }
            _ => {
                let q = self.query(2);
                self.deleted = true;
                let opts = *self.rng.pick(&["max_docs=1&refresh=true", "conflicts=proceed&refresh=true", "refresh=true&slices=1"]);
                vec![Step::new("POST", format!("/{name}/_delete_by_query?{opts}"), Body::Json(json!({"query": q})))]
            }
        }
    }
}
