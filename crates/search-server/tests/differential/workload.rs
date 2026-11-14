//! Seeded workload generator for the differential suite.
//!
//! A *case* is a list of [`Step`]s against one or two fresh indices: create them with a random
//! mapping (every common field type, a `keyword` multi-field under text, nested and object
//! fields, sometimes a completion field) and shard count, bulk-load random documents in one or
//! more batches (refreshing between them so shards hold several segments), then interleave
//! searches and other reads with writes (index, partial update, delete, by-query APIs) and
//! refreshes.
//!
//! Everything comes from one [`Rng`] seeded per case, so a case is reproduced from its seed
//! alone. Values are drawn from small vocabularies so that queries match, aggregations bucket and
//! scores tie. The generator stays clear of what differs by nature between two engines:
//! `refresh_interval` is `-1` (nothing becomes visible except through an explicit refresh),
//! every document has an explicit id, nothing depends on `now`, and no request asks for
//! resources that could take a node down.

use serde_json::{Map, Value, json};

/// SplitMix64: tiny, fast and good enough for workload generation.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % ((hi - lo + 1) as u64)) as i64
    }
    /// True with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p
    }
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

const WORDS: &[&str] = &[
    "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "search", "engine", "roll", "elastic", "index", "shard", "query", "score",
    "running", "runs", "ran", "cats", "cat", "data", "stream", "fast", "slow", "blue", "green", "red", "apple", "banana", "cherry",
    "the", "and", "of", "Quick", "Dogs",
];
const KEYWORDS: &[&str] = &["alpha", "beta", "gamma", "delta", "Alpha", "omega", "pi", "rho", "x-1", "x-2", "user@example.com", ""];
const ANALYZERS: &[&str] = &["standard", "english", "whitespace", "simple", "stop", "keyword"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Text,
    Keyword,
    Long,
    Integer,
    Short,
    Double,
    Float,
    HalfFloat,
    ScaledFloat,
    Boolean,
    Date,
    Ip,
    GeoPoint,
    Completion,
    Wildcard,
    MatchOnlyText,
    // The extended grammar's types (`extended.rs` adds them to a schema).
    /// `keyword` with the `lowercase` normalizer.
    KeywordLower,
    DateNanos,
    UnsignedLong,
    Byte,
    Flattened,
    /// `dense_vector`, three dimensions, exact (`flat`) search.
    DenseVector,
    SearchAsYouType,
    IntegerRange,
}

impl Kind {
    fn mapping_type(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Keyword => "keyword",
            Kind::Long => "long",
            Kind::Integer => "integer",
            Kind::Short => "short",
            Kind::Double => "double",
            Kind::Float => "float",
            Kind::HalfFloat => "half_float",
            Kind::ScaledFloat => "scaled_float",
            Kind::Boolean => "boolean",
            Kind::Date => "date",
            Kind::Ip => "ip",
            Kind::GeoPoint => "geo_point",
            Kind::Completion => "completion",
            Kind::Wildcard => "wildcard",
            Kind::MatchOnlyText => "match_only_text",
            Kind::KeywordLower => "keyword",
            Kind::DateNanos => "date_nanos",
            Kind::UnsignedLong => "unsigned_long",
            Kind::Byte => "byte",
            Kind::Flattened => "flattened",
            Kind::DenseVector => "dense_vector",
            Kind::SearchAsYouType => "search_as_you_type",
            Kind::IntegerRange => "integer_range",
        }
    }
    pub fn numeric(self) -> bool {
        matches!(self, Kind::Long | Kind::Integer | Kind::Short | Kind::Double | Kind::Float | Kind::HalfFloat | Kind::ScaledFloat | Kind::UnsignedLong | Kind::Byte)
    }
    fn integral(self) -> bool {
        matches!(self, Kind::Long | Kind::Integer | Kind::Short | Kind::Byte)
    }
    fn textual(self) -> bool {
        matches!(self, Kind::Text | Kind::MatchOnlyText)
    }
    /// Has doc values: sortable and aggregatable.
    fn doc_values(self) -> bool {
        !matches!(self, Kind::Text | Kind::MatchOnlyText | Kind::Completion | Kind::Flattened | Kind::DenseVector | Kind::SearchAsYouType | Kind::IntegerRange)
    }
}

#[derive(Clone, Debug)]
pub struct Field {
    /// Full path (`n.kw` for nested sub-fields).
    pub path: String,
    pub kind: Kind,
    /// Text fields: the `raw` keyword multi-field exists.
    pub raw: bool,
    /// Text fields: stores term vectors with offsets (the `fvh` highlighter works).
    pub vectors: bool,
    /// Inside the nested object `n`.
    pub nested: bool,
}

/// The fields of a generated index.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub fields: Vec<Field>,
    pub has_nested: bool,
    pub has_object: bool,
}

impl Schema {
    fn of(&self, pred: impl Fn(&Field) -> bool) -> Vec<&Field> {
        self.fields.iter().filter(|f| pred(f)).collect()
    }
    fn top(&self, pred: impl Fn(&Field) -> bool) -> Vec<&Field> {
        self.of(|f| !f.nested && pred(f))
    }
}

/// One request of a case.
#[derive(Clone, Debug)]
pub struct Step {
    pub method: &'static str,
    /// Path and query string; `${var}` is replaced with a value captured earlier on the same side.
    pub path: String,
    pub body: Body,
    /// Values to remember from the response: (variable, JSON pointer).
    pub capture: Vec<(String, String)>,
    /// Compare this step's responses (setup steps can be replayed without comparing).
    pub compare: bool,
}

#[derive(Clone, Debug)]
pub enum Body {
    None,
    Json(Value),
    /// NDJSON (`_bulk`, `_msearch`): one JSON value per line.
    Lines(Vec<Value>),
}

impl Step {
    pub fn new(method: &'static str, path: impl Into<String>, body: Body) -> Step {
        Step { method, path: path.into(), body, capture: Vec::new(), compare: true }
    }
    fn capture(mut self, var: &str, pointer: &str) -> Step {
        self.capture.push((var.into(), pointer.into()));
        self
    }
    /// What the step does, for reports: `POST /i/_search`.
    pub fn label(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

/// A generated case.
pub struct Case {
    /// The index-name prefix every step uses (`diff-c<id>`).
    pub prefix: String,
    pub steps: Vec<Step>,
}

struct Gen<'a> {
    rng: Rng,
    schema: Schema,
    /// Ids that exist (as far as the generator knows) per index.
    ids: Vec<Vec<String>>,
    next_id: usize,
    indices: &'a [String],
    agg_seq: usize,
    /// Number of fields queries may reference (every field after this was added dynamically).
    _dynamic: usize,
    /// A delete ran: Elasticsearch indexes a tombstone per delete, shifting the Lucene doc ids
    /// that `_doc` order follows (across shards, by nature).
    deleted: bool,
    /// The extended grammar's own stream (`extended.rs`): it decides where extensions go and
    /// draws them, so the base grammar's draws stay where they were.
    ext: Rng,
}

pub fn case(id: u64, seed: u64) -> Case {
    let mut rng = Rng::new(seed);
    let prefix = format!("diff-c{id}");
    let indices: Vec<String> =
        if rng.chance(0.2) { vec![format!("{prefix}-a"), format!("{prefix}-b")] } else { vec![format!("{prefix}-a")] };
    let schema = schema(&mut rng);
    let ext = Rng::new(seed ^ 0x6578_7465_6e64_6564);
    let mut g = Gen { rng, schema, ids: vec![Vec::new(); indices.len()], next_id: 0, indices: &indices, agg_seq: 0, _dynamic: 0, deleted: false, ext };
    g.extend_schema();
    let mut steps = Vec::new();
    for (i, name) in indices.iter().enumerate() {
        let shards = *g.rng.pick(&[1, 1, 1, 2, 3, 5]);
        let mut settings = json!({"number_of_shards": shards, "number_of_replicas": 0, "refresh_interval": -1});
        if g.rng.chance(0.15) {
            // Disable merges' effect on doc order and stats: a few small segments never merge
            // under the tiered policy anyway, but say so explicitly sometimes.
            settings["merge.policy.segments_per_tier"] = json!(20);
        }
        let _ = i;
        steps.push(Step::new("PUT", name.clone(), Body::Json(json!({"settings": settings, "mappings": mapping(&g.schema)}))));
    }
    // Initial load: one to three bulks, refreshing after each so shards get several segments.
    let batches = 1 + g.rng.below(3);
    for _ in 0..batches {
        let n = 3 + g.rng.below(25);
        steps.push(g.bulk(n));
        steps.push(g.refresh());
    }
    let ops = 10 + g.rng.below(25);
    for _ in 0..ops {
        let r = g.rng.below(100);
        let mut produced = match r {
            0..=49 => vec![g.search_step()],
            50..=55 => vec![g.count_step()],
            56..=58 => vec![g.msearch_step()],
            59..=61 => vec![g.get_step()],
            62..=63 => vec![g.explain_step()],
            64..=66 => g.scroll_steps(),
            67..=68 => g.pit_steps(),
            69 => vec![g.field_caps_step()],
            70..=76 => vec![g.index_step()],
            77..=80 => vec![g.update_step()],
            81..=84 => vec![g.delete_step()],
            85..=86 => vec![g.delete_by_query_step()],
            87 => vec![g.update_by_query_step()],
            88..=89 => {
                let n = 1 + g.rng.below(6);
                vec![g.bulk(n)]
            }
            _ => vec![g.refresh()],
        };
        steps.append(&mut produced);
        if g.ext.chance(0.15) {
            let mut more = g.with_ext(|g| g.ext_steps());
            steps.append(&mut more);
        }
    }
    // A final refresh and a few more searches over everything that was written.
    steps.push(g.refresh());
    for _ in 0..3 {
        steps.push(g.search_step());
    }
    Case { prefix: format!("diff-c{id}"), steps }
}

fn schema(rng: &mut Rng) -> Schema {
    let mut fields = Vec::new();
    let add = add_fields;
    let (texts, keywords) = (1 + rng.below(2), 1 + rng.below(2));
    add(rng, Kind::Text, texts, "", false, &mut fields);
    add(rng, Kind::Keyword, keywords, "", false, &mut fields);
    add(rng, Kind::Date, 1, "", false, &mut fields);
    let first = *rng.pick(&[Kind::Long, Kind::Integer, Kind::Double]);
    add(rng, first, 1, "", false, &mut fields);
    for kind in [
        Kind::Long,
        Kind::Integer,
        Kind::Short,
        Kind::Double,
        Kind::Float,
        Kind::HalfFloat,
        Kind::ScaledFloat,
        Kind::Boolean,
        Kind::Ip,
        Kind::GeoPoint,
        Kind::Completion,
        Kind::Wildcard,
        Kind::MatchOnlyText,
    ] {
        if kind != first && rng.chance(0.3) {
            add(rng, kind, 1, "", false, &mut fields);
        }
    }
    let has_object = rng.chance(0.3);
    if has_object {
        add(rng, Kind::Keyword, 1, "o.", false, &mut fields);
        add(rng, Kind::Integer, 1, "o.", false, &mut fields);
    }
    let has_nested = rng.chance(0.3);
    if has_nested {
        add(rng, Kind::Keyword, 1, "n.", true, &mut fields);
        add(rng, Kind::Long, 1, "n.", true, &mut fields);
        add(rng, Kind::Text, 1, "n.", true, &mut fields);
    }
    Schema { fields, has_nested, has_object }
}

fn add_fields(rng: &mut Rng, kind: Kind, n: usize, prefix: &str, nested: bool, fields: &mut Vec<Field>) {
    for i in 0..n {
        let name = match kind {
            Kind::Text => "text",
            Kind::Keyword => "kw",
            Kind::Long => "long",
            Kind::Integer => "int",
            Kind::Short => "short",
            Kind::Double => "dbl",
            Kind::Float => "flt",
            Kind::HalfFloat => "half",
            Kind::ScaledFloat => "scaled",
            Kind::Boolean => "bool",
            Kind::Date => "date",
            Kind::Ip => "ip",
            Kind::GeoPoint => "geo",
            Kind::Completion => "comp",
            Kind::Wildcard => "wild",
            Kind::MatchOnlyText => "mot",
            Kind::KeywordLower => "kwl",
            Kind::DateNanos => "dnanos",
            Kind::UnsignedLong => "ulong",
            Kind::Byte => "byte",
            Kind::Flattened => "flat",
            Kind::DenseVector => "vec",
            Kind::SearchAsYouType => "sayt",
            Kind::IntegerRange => "irange",
        };
        fields.push(Field {
            path: format!("{prefix}{name}{i}"),
            kind,
            raw: kind == Kind::Text && rng.chance(0.7),
            vectors: kind == Kind::Text && rng.chance(0.3),
            nested,
        });
    }
}

fn field_mapping(rng_free: &Field) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!(rng_free.kind.mapping_type()));
    match rng_free.kind {
        Kind::Text => {
            if rng_free.raw {
                m.insert("fields".into(), json!({"raw": {"type": "keyword", "ignore_above": 32}}));
            }
            if rng_free.vectors {
                m.insert("term_vector".into(), json!("with_positions_offsets"));
            }
        }
        Kind::ScaledFloat => {
            m.insert("scaling_factor".into(), json!(100));
        }
        Kind::Date => {
            m.insert("format".into(), json!("strict_date_optional_time||epoch_millis"));
        }
        Kind::KeywordLower => {
            m.insert("normalizer".into(), json!("lowercase"));
        }
        Kind::DenseVector => {
            m.insert("dims".into(), json!(3));
            m.insert("similarity".into(), json!("l2_norm"));
            m.insert("index_options".into(), json!({"type": "flat"}));
        }
        _ => {}
    }
    Value::Object(m)
}

fn mapping(s: &Schema) -> Value {
    let mut props = Map::new();
    let mut obj = Map::new();
    let mut nested = Map::new();
    for f in &s.fields {
        let mut fm = field_mapping(f);
        // Text fields pick their analyzer from the field's name so it is stable per schema.
        if f.kind == Kind::Text {
            let h = f.path.bytes().fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize));
            let a = ANALYZERS[h % ANALYZERS.len()];
            if a != "standard" {
                fm["analyzer"] = json!(a);
            }
        }
        if let Some(rest) = f.path.strip_prefix("o.") {
            obj.insert(rest.into(), fm);
        } else if let Some(rest) = f.path.strip_prefix("n.") {
            nested.insert(rest.into(), fm);
        } else {
            props.insert(f.path.clone(), fm);
        }
    }
    if s.has_object {
        props.insert("o".into(), json!({"properties": obj}));
    }
    if s.has_nested {
        props.insert("n".into(), json!({"type": "nested", "properties": nested}));
    }
    json!({"properties": props})
}

const DAY: i64 = 86_400_000;
const BASE: i64 = 1_704_067_200_000; // 2024-01-01T00:00:00Z

fn iso(ms: i64, date_only: bool) -> String {
    // Civil-from-days (Howard Hinnant), enough for 2020–2030.
    let days = ms.div_euclid(DAY);
    let rem = ms.rem_euclid(DAY);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    if date_only {
        format!("{y:04}-{m:02}-{d:02}")
    } else {
        let (h, mi, s) = (rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60);
        format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    }
}

impl Gen<'_> {
    fn words(&mut self, lo: usize, hi: usize) -> String {
        let n = lo + self.rng.below(hi - lo + 1);
        let mut out = String::new();
        for i in 0..n {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(self.rng.pick(WORDS));
        }
        out
    }

    fn date_ms(&mut self) -> i64 {
        BASE + self.rng.range(0, 400) * DAY + self.rng.range(0, 23) * 3_600_000 + if self.rng.chance(0.2) { self.rng.range(0, 59) * 60_000 } else { 0 }
    }

    fn scalar(&mut self, kind: Kind) -> Value {
        match kind {
            Kind::Text | Kind::MatchOnlyText => json!(self.words(1, 9)),
            Kind::Keyword | Kind::Wildcard => json!(*self.rng.pick(KEYWORDS)),
            Kind::Long => json!(if self.rng.chance(0.05) { self.rng.range(-4_000_000_000, 4_000_000_000) } else { self.rng.range(-20, 100) }),
            Kind::Integer => json!(self.rng.range(-50, 200)),
            Kind::Short => json!(self.rng.range(-300, 300)),
            Kind::Double | Kind::Float | Kind::HalfFloat | Kind::ScaledFloat => {
                let v = self.rng.range(-5000, 20000) as f64 / 100.0;
                if self.rng.chance(0.1) { json!(v.trunc() as i64) } else { json!(v) }
            }
            Kind::Boolean => json!(self.rng.chance(0.5)),
            Kind::Date => {
                let ms = self.date_ms();
                match self.rng.below(4) {
                    0 => json!(ms),
                    1 => json!(iso(ms, true)),
                    _ => json!(iso(ms, false)),
                }
            }
            Kind::Ip => json!(format!("10.{}.{}.{}", self.rng.below(3), self.rng.below(4), self.rng.below(256))),
            Kind::GeoPoint => {
                let lat = self.rng.range(-6000, 6000) as f64 / 100.0;
                let lon = self.rng.range(-17000, 17000) as f64 / 100.0;
                if self.rng.chance(0.5) { json!({"lat": lat, "lon": lon}) } else { json!(format!("{lat},{lon}")) }
            }
            Kind::Completion => json!(self.words(1, 3)),
            Kind::KeywordLower => json!(*self.rng.pick(KEYWORDS)),
            Kind::DateNanos => {
                let ms = self.date_ms();
                match self.rng.below(3) {
                    0 => json!(ms),
                    1 => json!(iso(ms, false)),
                    // Nanosecond precision: `…T01:02:03.123456789Z`.
                    _ => json!(format!("{}.{:09}Z", iso(ms, false).trim_end_matches('Z'), self.rng.range(0, 999_999_999))),
                }
            }
            Kind::UnsignedLong => match self.rng.below(10) {
                0 => json!(u64::MAX - self.rng.below(3) as u64),
                1 => json!((1u64 << 63) + self.rng.below(1000) as u64),
                _ => json!(self.rng.range(0, 120)),
            },
            Kind::Byte => json!(self.rng.range(-128, 127)),
            Kind::Flattened => {
                let mut o = Map::new();
                o.insert("a".into(), json!(*self.rng.pick(KEYWORDS)));
                if self.rng.chance(0.6) {
                    o.insert("b".into(), json!({"c": *self.rng.pick(KEYWORDS)}));
                }
                if self.rng.chance(0.3) {
                    o.insert("n".into(), json!(self.rng.range(0, 20)));
                }
                Value::Object(o)
            }
            Kind::DenseVector => {
                // Never the zero vector; two decimals so both sides parse the same floats.
                let c = |r: &mut Rng| r.range(-300, 300) as f64 / 100.0;
                json!([1.0 + self.rng.range(0, 200) as f64 / 100.0, c(&mut self.rng), c(&mut self.rng)])
            }
            Kind::SearchAsYouType => json!(self.words(1, 5)),
            Kind::IntegerRange => {
                let lo = self.rng.range(-20, 80);
                json!({"gte": lo, "lte": lo + self.rng.range(0, 30)})
            }
        }
    }

    fn value(&mut self, f: &Field) -> Value {
        let multi = matches!(f.kind, Kind::Keyword | Kind::Long | Kind::Integer | Kind::Text | Kind::Double | Kind::Date) && self.rng.chance(0.12);
        if multi {
            let n = 2 + self.rng.below(2);
            Value::Array((0..n).map(|_| self.scalar(f.kind)).collect())
        } else if self.rng.chance(0.03) && f.kind != Kind::Completion {
            Value::Null
        } else {
            self.scalar(f.kind)
        }
    }

    fn doc(&mut self) -> Value {
        let mut d = Map::new();
        let fields: Vec<Field> = self.schema.fields.clone();
        let mut obj = Map::new();
        for f in fields.iter().filter(|f| !f.nested) {
            if !self.rng.chance(0.85) {
                continue;
            }
            let v = self.value(f);
            if let Some(rest) = f.path.strip_prefix("o.") {
                obj.insert(rest.into(), v);
            } else {
                d.insert(f.path.clone(), v);
            }
        }
        if !obj.is_empty() {
            d.insert("o".into(), Value::Object(obj));
        }
        if self.schema.has_nested && self.rng.chance(0.8) {
            let n = self.rng.below(4);
            let mut items = Vec::new();
            for _ in 0..n {
                let mut item = Map::new();
                for f in fields.iter().filter(|f| f.nested) {
                    if self.rng.chance(0.85) {
                        let v = self.scalar(f.kind);
                        item.insert(f.path.strip_prefix("n.").unwrap().into(), v);
                    }
                }
                items.push(Value::Object(item));
            }
            d.insert("n".into(), Value::Array(items));
        }
        Value::Object(d)
    }

    fn target(&mut self) -> usize {
        self.rng.below(self.indices.len())
    }

    fn new_id(&mut self) -> String {
        self.next_id += 1;
        format!("{}", self.next_id)
    }

    fn existing_id(&mut self, idx: usize) -> Option<String> {
        if self.ids[idx].is_empty() { None } else { Some(self.ids[idx][self.rng.below(self.ids[idx].len())].clone()) }
    }

    fn bulk(&mut self, n: usize) -> Step {
        let mut lines = Vec::new();
        for _ in 0..n {
            let idx = self.target();
            let reuse = self.rng.chance(0.1);
            let id = if reuse { self.existing_id(idx).unwrap_or_else(|| self.new_id()) } else { self.new_id() };
            let op = if self.rng.chance(0.1) { "create" } else { "index" };
            let routing = self.rng.chance(0.05);
            let mut meta = json!({"_index": self.indices[idx], "_id": id});
            if routing {
                // Routing to the shard the id would go to anyway would hide nothing: use a word.
                meta["routing"] = json!(*self.rng.pick(&["r1", "r2"]));
            }
            lines.push(json!({ op: meta }));
            lines.push(self.doc());
            if !self.ids[idx].contains(&id) && !routing {
                self.ids[idx].push(id);
            }
        }
        Step::new("POST", "/_bulk", Body::Lines(lines))
    }

    fn refresh(&mut self) -> Step {
        let path = if self.indices.len() > 1 && self.rng.chance(0.5) {
            format!("/{}/_refresh", self.indices.join(","))
        } else {
            format!("/{}/_refresh", self.indices[self.target()])
        };
        Step::new("POST", path, Body::None)
    }

    fn search_path(&mut self) -> String {
        match (self.indices.len(), self.rng.below(3)) {
            (1, _) | (_, 0) => self.indices[self.target()].clone(),
            (_, 1) => self.indices.join(","),
            _ => format!("{}-*", self.indices[0].trim_end_matches("-a")),
        }
    }

    fn field(&mut self, pred: impl Fn(&Field) -> bool) -> Option<Field> {
        let c: Vec<Field> = self.schema.top(pred).into_iter().cloned().collect();
        if c.is_empty() { None } else { Some(c[self.rng.below(c.len())].clone()) }
    }

    fn nested_field(&mut self, pred: impl Fn(&Field) -> bool) -> Option<Field> {
        let c: Vec<Field> = self.schema.of(|f| f.nested && pred(f)).into_iter().cloned().collect();
        if c.is_empty() { None } else { Some(c[self.rng.below(c.len())].clone()) }
    }

    fn word(&mut self) -> String {
        self.rng.pick(WORDS).to_string()
    }

    // ---- queries -------------------------------------------------------------------------

    pub fn query(&mut self, depth: usize) -> Value {
        let compound = depth < 3 && self.rng.chance(0.35);
        if compound {
            return self.compound(depth + 1);
        }
        for _ in 0..8 {
            if let Some(q) = self.leaf() {
                return q;
            }
        }
        json!({"match_all": {}})
    }

    fn compound(&mut self, depth: usize) -> Value {
        match self.rng.below(10) {
            0..=4 => {
                let mut b = Map::new();
                for clause in ["must", "should", "filter", "must_not"] {
                    let p = if clause == "should" { 0.6 } else { 0.4 };
                    if self.rng.chance(p) {
                        let n = 1 + self.rng.below(3);
                        let qs: Vec<Value> = (0..n).map(|_| self.query(depth)).collect();
                        let v = if n == 1 && self.rng.chance(0.4) { qs.into_iter().next().unwrap() } else { Value::Array(qs) };
                        b.insert(clause.into(), v);
                    }
                }
                if b.contains_key("should") && self.rng.chance(0.3) {
                    b.insert("minimum_should_match".into(), json!(*self.rng.pick(&[json!(1), json!(2), json!("50%"), json!("-1")])));
                }
                if self.rng.chance(0.1) {
                    b.insert("boost".into(), json!(2.5));
                }
                json!({"bool": b})
            }
            5 => json!({"constant_score": {"filter": self.query(depth), "boost": *self.rng.pick(&[1.0, 1.5, 3.0])}}),
            6 => json!({"dis_max": {"queries": [self.query(depth), self.query(depth)], "tie_breaker": *self.rng.pick(&[0.0, 0.3, 0.7])}}),
            7 => json!({"boosting": {"positive": self.query(depth), "negative": self.query(depth), "negative_boost": 0.2}}),
            8 => self.function_score(depth),
            _ => {
                if let (true, Some(f)) = (self.schema.has_nested, self.nested_field(|_| true)) {
                    let inner = self.leaf_on(&f).unwrap_or(json!({"match_all": {}}));
                    let mut q = json!({"nested": {"path": "n", "query": inner}});
                    q["nested"]["score_mode"] = json!(*self.rng.pick(&["avg", "sum", "max", "min", "none"]));
                    if self.rng.chance(0.3) {
                        q["nested"]["inner_hits"] = json!({"size": 2});
                    }
                    q
                } else {
                    json!({"bool": {"should": [self.query(depth)], "filter": [self.query(depth)]}})
                }
            }
        }
    }

    fn function_score(&mut self, depth: usize) -> Value {
        let mut fs = Map::new();
        fs.insert("query".into(), self.query(depth));
        let mut fns = Vec::new();
        let n = 1 + self.rng.below(2);
        for _ in 0..n {
            let mut f = Map::new();
            match self.rng.below(3) {
                0 => {
                    f.insert("weight".into(), json!(*self.rng.pick(&[0.5, 2.0, 3.0])));
                }
                1 => {
                    if let Some(nf) = self.field(|f| matches!(f.kind, Kind::Integer | Kind::Short | Kind::Double | Kind::Float)) {
                        f.insert(
                            "field_value_factor".into(),
                            json!({"field": nf.path, "factor": 1.2, "modifier": *self.rng.pick(&["none", "log1p", "sqrt", "square"]), "missing": 1}),
                        );
                    } else {
                        f.insert("weight".into(), json!(2.0));
                    }
                }
                _ => {
                    if let Some(df) = self.field(|f| f.kind == Kind::Date) {
                        let origin = iso(BASE + 100 * DAY, true);
                        f.insert(
                            self.rng.pick(&["gauss", "exp", "linear"]).to_string(),
                            json!({df.path.clone(): {"origin": origin, "scale": "30d", "decay": 0.5}}),
                        );
                    } else {
                        f.insert("weight".into(), json!(2.0));
                    }
                }
            }
            if self.rng.chance(0.4) {
                f.insert("filter".into(), self.query(3));
            }
            fns.push(Value::Object(f));
        }
        fs.insert("functions".into(), Value::Array(fns));
        fs.insert("score_mode".into(), json!(*self.rng.pick(&["multiply", "sum", "avg", "first", "max", "min"])));
        fs.insert("boost_mode".into(), json!(*self.rng.pick(&["multiply", "replace", "sum", "avg", "max", "min"])));
        json!({"function_score": fs})
    }

    fn leaf(&mut self) -> Option<Value> {
        if self.ext.chance(0.2)
            && let Some(q) = self.with_ext(|g| g.ext_leaf())
        {
            return Some(q);
        }
        match self.rng.below(20) {
            0 => Some(json!({"match_all": {}})),
            1 => {
                let idx = self.target();
                let ids: Vec<String> = (0..3).filter_map(|_| self.existing_id(idx)).collect();
                Some(json!({"ids": {"values": ids}}))
            }
            2 => {
                let f = self.field(|f| f.kind.textual())?;
                let fields: Vec<Value> = self.schema.top(|g| g.kind == Kind::Text).iter().map(|g| json!(g.path)).collect();
                let mut fields = fields;
                if self.rng.chance(0.3) {
                    fields.push(json!(format!("{}^2", f.path)));
                }
                let mut mm = json!({"query": self.words(1, 3), "fields": fields});
                mm["type"] = json!(*self.rng.pick(&["best_fields", "most_fields", "cross_fields", "phrase", "phrase_prefix", "bool_prefix"]));
                if self.rng.chance(0.3) {
                    mm["operator"] = json!("and");
                }
                if self.rng.chance(0.2) {
                    mm["tie_breaker"] = json!(0.3);
                }
                Some(json!({"multi_match": mm}))
            }
            3 => {
                let words = self.words(1, 3);
                let q = match self.rng.below(5) {
                    0 => words.replace(' ', " AND "),
                    1 => words.replace(' ', " OR "),
                    2 => format!("{}*", &words[..words.len().min(3)]),
                    3 => {
                        let f = self.field(|f| f.kind == Kind::Keyword || f.kind.textual())?;
                        format!("{}:{}", f.path, self.word())
                    }
                    _ => format!("\"{words}\""),
                };
                let mut qs = json!({"query": q});
                if self.rng.chance(0.5) {
                    let f = self.field(|f| f.kind.textual())?;
                    qs["default_field"] = json!(f.path);
                }
                if self.rng.chance(0.2) {
                    qs["default_operator"] = json!("AND");
                }
                let kind = if self.rng.chance(0.6) { "query_string" } else { "simple_query_string" };
                if kind == "simple_query_string" {
                    qs.as_object_mut().unwrap().remove("default_field");
                    qs["query"] = json!(self.words(1, 3).replace(' ', self.rng.pick(&[" + ", " | ", " -", " "])));
                }
                Some(json!({ kind: qs }))
            }
            4 => {
                let fields: Vec<Value> = self.schema.top(|g| g.kind == Kind::Text).iter().map(|g| json!(g.path)).collect();
                Some(json!({"combined_fields": {"query": self.words(1, 3), "fields": fields}}))
            }
            _ => {
                let f = self.field(|_| true)?;
                self.leaf_on(&f)
            }
        }
    }

    /// A leaf query on one field, appropriate to its type.
    fn leaf_on(&mut self, f: &Field) -> Option<Value> {
        let p = f.path.clone();
        if self.rng.chance(0.08) {
            return Some(json!({"exists": {"field": p}}));
        }
        Some(match f.kind {
            Kind::Text | Kind::MatchOnlyText => match self.rng.below(9) {
                0..=2 => {
                    let mut m = json!({"query": self.words(1, 3)});
                    if self.rng.chance(0.3) {
                        m["operator"] = json!("and");
                    }
                    if self.rng.chance(0.15) {
                        m["fuzziness"] = json!("AUTO");
                    }
                    if self.rng.chance(0.15) {
                        m["minimum_should_match"] = json!("2");
                    }
                    if self.rng.chance(0.2) {
                        m["boost"] = json!(*self.rng.pick(&[0.5, 2.0, 4.0]));
                    }
                    json!({"match": {p: m}})
                }
                3 => json!({"match_phrase": {p: {"query": self.words(2, 3), "slop": self.rng.below(3)}}}),
                4 => json!({"match_phrase_prefix": {p: {"query": format!("{} {}", self.word(), &self.word()[..2])}}}),
                5 => json!({"match_bool_prefix": {p: format!("{} {}", self.word(), &self.word()[..2])}}),
                6 => {
                    if f.kind == Kind::MatchOnlyText {
                        json!({"match": {p: self.word()}})
                    } else {
                        json!({"span_near": {"clauses": [{"span_term": {p.clone(): self.word().to_lowercase()}}, {"span_term": {p: self.word().to_lowercase()}}], "slop": 2, "in_order": self.rng.chance(0.5)}})
                    }
                }
                7 => {
                    if f.kind == Kind::MatchOnlyText {
                        json!({"match_phrase": {p: self.words(2, 2)}})
                    } else {
                        json!({"intervals": {p: {"match": {"query": self.words(2, 2), "max_gaps": 2, "ordered": self.rng.chance(0.5)}}}})
                    }
                }
                _ => json!({"term": {p: self.word().to_lowercase()}}),
            },
            Kind::Keyword | Kind::Wildcard => {
                let v = self.rng.pick(KEYWORDS).to_string();
                match self.rng.below(8) {
                    0..=2 => json!({"term": {p: v}}),
                    3 => json!({"terms": {p: [v, *self.rng.pick(KEYWORDS), *self.rng.pick(KEYWORDS)]}}),
                    4 => json!({"prefix": {p: &v[..v.len().min(1)]}}),
                    5 => json!({"wildcard": {p: {"value": format!("*{}*", &v[v.len().min(1)..v.len().min(3)])}}}),
                    6 => json!({"regexp": {p: {"value": "(al|be).*"}}}),
                    _ => {
                        if f.kind == Kind::Wildcard {
                            json!({"term": {p: {"value": v, "case_insensitive": true}}})
                        } else {
                            json!({"fuzzy": {p: {"value": "alfa", "fuzziness": 1}}})
                        }
                    }
                }
            }
            k if k.numeric() => match self.rng.below(4) {
                0 => json!({"term": {p: self.scalar(k)}}),
                1 => json!({"terms": {p: [self.scalar(k), self.scalar(k)]}}),
                _ => {
                    let (a, b) = (self.scalar(k), self.scalar(k));
                    let (lo, hi) = if a.as_f64() <= b.as_f64() { (a, b) } else { (b, a) };
                    let mut r = Map::new();
                    r.insert(self.rng.pick(&["gt", "gte"]).to_string(), lo);
                    if self.rng.chance(0.7) {
                        r.insert(self.rng.pick(&["lt", "lte"]).to_string(), hi);
                    }
                    json!({"range": {p: r}})
                }
            },
            Kind::Boolean => json!({"term": {p: self.rng.chance(0.5)}}),
            Kind::Date => {
                let (a, b) = (self.date_ms(), self.date_ms());
                let (lo, hi) = (a.min(b), a.max(b));
                let mut r = json!({"gte": iso(lo, self.rng.chance(0.5)), "lt": iso(hi, false)});
                if self.rng.chance(0.2) {
                    r = json!({"gte": iso(lo, true), "lte": iso(hi, true), "format": "strict_date"});
                }
                if self.rng.chance(0.15) {
                    r = json!({"gte": format!("{}||/M", iso(lo, true)), "lt": format!("{}||+1M/d", iso(hi, true))});
                }
                json!({"range": {p: r}})
            }
            Kind::Ip => match self.rng.below(2) {
                0 => json!({"term": {p: format!("10.{}.0.0/16", self.rng.below(3))}}),
                _ => json!({"range": {p: {"gte": "10.0.2.0", "lt": "10.1.3.255"}}}),
            },
            Kind::GeoPoint => {
                if self.rng.chance(0.5) {
                    json!({"geo_distance": {"distance": format!("{}km", 500 + self.rng.below(4000)), p: {"lat": 10.0, "lon": 20.0}}})
                } else {
                    json!({"geo_bounding_box": {p: {"top_left": {"lat": 40.0, "lon": -100.0}, "bottom_right": {"lat": -20.0, "lon": 60.0}}}})
                }
            }
            Kind::Completion => json!({"exists": {"field": p}}),
            _ => json!({"exists": {"field": p}}),
        })
    }

    // ---- aggregations --------------------------------------------------------------------

    fn agg_name(&mut self, kind: &str) -> String {
        self.agg_seq += 1;
        format!("{kind}_{}", self.agg_seq)
    }

    fn aggs(&mut self, depth: usize, in_nested: bool) -> Value {
        let mut out = Map::new();
        let n = 1 + self.rng.below(if depth == 0 { 3 } else { 2 });
        for _ in 0..n {
            if let Some((name, a)) = self.agg(depth, in_nested) {
                out.insert(name, a);
            }
        }
        Value::Object(out)
    }

    fn agg_field(&mut self, in_nested: bool, pred: impl Fn(&Field) -> bool) -> Option<Field> {
        if in_nested { self.nested_field(|f| pred(f)) } else { self.field(pred) }
    }

    fn agg(&mut self, depth: usize, in_nested: bool) -> Option<(String, Value)> {
        if self.ext.chance(0.25)
            && let Some(a) = self.with_ext(|g| g.ext_agg(depth, in_nested))
        {
            return Some(a);
        }
        let bucket = depth < 2 && self.rng.chance(0.55);
        if bucket {
            let (kind, mut body) = self.bucket_agg(in_nested)?;
            let name = self.agg_name(&kind);
            if self.rng.chance(0.5) && kind != "composite" {
                let entering = kind == "nested";
                let subs = self.aggs(depth + 1, in_nested || entering);
                body["aggs"] = subs;
                if (kind == "terms" || kind == "histogram" || kind == "date_histogram") && self.rng.chance(0.4) {
                    // A pipeline over the buckets' doc counts or a metric sibling.
                    let pipe = match self.rng.below(4) {
                        0 => json!({"cumulative_sum": {"buckets_path": "_count"}}),
                        1 => json!({"bucket_sort": {"sort": [{"_count": {"order": "desc"}}], "size": 3}}),
                        2 => json!({"derivative": {"buckets_path": "_count"}}),
                        _ => json!({"bucket_selector": {"buckets_path": {"c": "_count"}, "script": "params.c > 1"}}),
                    };
                    let is_hist = kind != "terms";
                    let pk = pipe.as_object().unwrap().keys().next().unwrap().clone();
                    if (pk == "derivative" || pk == "cumulative_sum") && !is_hist {
                        // these need a histogram parent
                    } else {
                        let pname = self.agg_name(&pk);
                        body["aggs"][pname] = pipe;
                    }
                }
            }
            Some((name, body))
        } else {
            let (kind, body) = self.metric_agg(in_nested)?;
            Some((self.agg_name(&kind), body))
        }
    }

    fn bucket_agg(&mut self, in_nested: bool) -> Option<(String, Value)> {
        let r = self.rng.below(12);
        Some(match r {
            0..=3 => {
                let f = self.agg_field(in_nested, |f| matches!(f.kind, Kind::Keyword | Kind::Long | Kind::Integer | Kind::Boolean | Kind::Ip | Kind::Date | Kind::Double) || (f.kind == Kind::Text && f.raw))?;
                let path = if f.kind == Kind::Text { format!("{}.raw", f.path) } else { f.path.clone() };
                let mut t = json!({"field": path});
                if self.rng.chance(0.5) {
                    t["size"] = json!(1 + self.rng.below(6));
                }
                match self.rng.below(5) {
                    0 => t["order"] = json!({"_key": "asc"}),
                    1 => t["order"] = json!({"_count": "asc"}),
                    _ => {}
                }
                if self.rng.chance(0.15) {
                    t["min_doc_count"] = json!(2);
                }
                if self.rng.chance(0.1) {
                    t["missing"] = json!(if f.kind == Kind::Keyword { json!("N/A") } else { json!(0) });
                }
                if self.rng.chance(0.1) {
                    t["show_term_doc_count_error"] = json!(true);
                }
                ("terms".into(), json!({"terms": t}))
            }
            4 => {
                let f = self.agg_field(in_nested, |f| f.kind.numeric())?;
                let mut h = json!({"field": f.path, "interval": *self.rng.pick(&[5, 10, 50, 1000])});
                if self.rng.chance(0.3) {
                    h["min_doc_count"] = json!(1);
                }
                if self.rng.chance(0.2) {
                    h["offset"] = json!(2);
                }
                ("histogram".into(), json!({"histogram": h}))
            }
            5 => {
                let f = self.agg_field(in_nested, |f| f.kind == Kind::Date)?;
                let mut h = if self.rng.chance(0.6) {
                    json!({"field": f.path, "calendar_interval": *self.rng.pick(&["month", "week", "quarter", "1d"])})
                } else {
                    json!({"field": f.path, "fixed_interval": *self.rng.pick(&["7d", "12h", "30d"])})
                };
                if self.rng.chance(0.3) {
                    h["format"] = json!("yyyy-MM-dd");
                }
                if self.rng.chance(0.2) {
                    h["time_zone"] = json!(*self.rng.pick(&["+02:00", "America/New_York", "Europe/Budapest"]));
                }
                if self.rng.chance(0.3) {
                    h["min_doc_count"] = json!(1);
                }
                ("date_histogram".into(), json!({"date_histogram": h}))
            }
            6 => {
                let f = self.agg_field(in_nested, |f| f.kind.numeric())?;
                let keyed = self.rng.chance(0.3);
                ("range".into(), json!({"range": {"field": f.path, "keyed": keyed, "ranges": [{"to": 0}, {"from": 0, "to": 50}, {"from": 50}]}}))
            }
            7 => {
                let f = self.agg_field(in_nested, |f| f.kind == Kind::Date)?;
                ("date_range".into(), json!({"date_range": {"field": f.path, "ranges": [{"to": "2024-04-01"}, {"from": "2024-04-01", "to": "2024-09-01"}, {"from": "2024-09-01"}]}}))
            }
            8 => {
                let a = self.query(2);
                let b = self.query(2);
                if self.rng.chance(0.5) {
                    ("filters".into(), json!({"filters": {"filters": {"a": a, "b": b}, "other_bucket_key": "rest"}}))
                } else {
                    ("filter".into(), json!({"filter": a}))
                }
            }
            9 => {
                let f = self.agg_field(in_nested, |f| f.kind.doc_values() && f.kind != Kind::GeoPoint)?;
                ("missing".into(), json!({"missing": {"field": f.path}}))
            }
            10 => {
                if self.schema.has_nested && !in_nested {
                    ("nested".into(), json!({"nested": {"path": "n"}}))
                } else {
                    return None;
                }
            }
            _ => {
                if in_nested {
                    return None;
                }
                let k = self.field(|f| f.kind == Kind::Keyword)?;
                let mut sources = vec![json!({"k": {"terms": {"field": k.path}}})];
                if let Some(n) = self.field(|f| f.kind.integral()) {
                    sources.push(json!({"h": {"histogram": {"field": n.path, "interval": 25, "missing_bucket": true}}}));
                }
                ("composite".into(), json!({"composite": {"size": 3, "sources": sources}}))
            }
        })
    }

    fn metric_agg(&mut self, in_nested: bool) -> Option<(String, Value)> {
        let kinds = ["avg", "sum", "min", "max", "stats", "extended_stats", "value_count", "cardinality", "percentiles", "top_hits", "geo_bounds", "weighted_avg"];
        let kind = *self.rng.pick(&kinds);
        Some(match kind {
            "value_count" | "cardinality" => {
                let f = self.agg_field(in_nested, |f| f.kind.doc_values() && f.kind != Kind::GeoPoint)?;
                (kind.into(), json!({kind: {"field": f.path}}))
            }
            "top_hits" => {
                let mut th = json!({"size": 1 + self.rng.below(3)});
                if let Some(s) = self.field(|f| f.kind.numeric() || f.kind == Kind::Date)
                    && self.rng.chance(0.6)
                {
                    th["sort"] = json!([{s.path: {"order": *self.rng.pick(&["asc", "desc"])}}]);
                }
                if self.rng.chance(0.5) {
                    th["_source"] = json!(false);
                }
                (kind.into(), json!({"top_hits": th}))
            }
            "geo_bounds" => {
                let f = self.agg_field(in_nested, |f| f.kind == Kind::GeoPoint)?;
                (kind.into(), json!({"geo_bounds": {"field": f.path}}))
            }
            "weighted_avg" => {
                let v = self.agg_field(in_nested, |f| f.kind.numeric())?;
                let w = self.agg_field(in_nested, |f| f.kind.numeric())?;
                (kind.into(), json!({"weighted_avg": {"value": {"field": v.path}, "weight": {"field": w.path, "missing": 1}}}))
            }
            "percentiles" => {
                let f = self.agg_field(in_nested, |f| f.kind.numeric())?;
                (kind.into(), json!({"percentiles": {"field": f.path, "percents": [1, 50, 99]}}))
            }
            _ => {
                let f = self.agg_field(in_nested, |f| f.kind.numeric() || (f.kind == Kind::Date && kind != "sum"))?;
                let mut m = json!({"field": f.path});
                if self.rng.chance(0.1) {
                    m["missing"] = json!(0);
                }
                (kind.into(), json!({kind: m}))
            }
        })
    }

    // ---- search requests -----------------------------------------------------------------

    fn sort(&mut self) -> Option<Value> {
        let mut keys = Vec::new();
        let n = 1 + self.rng.below(2);
        for _ in 0..n {
            if self.rng.chance(0.2) {
                keys.push(json!({"_score": {"order": *self.rng.pick(&["desc", "asc"])}}));
                continue;
            }
            let f = self.field(|f| f.kind.doc_values() && f.kind != Kind::GeoPoint && f.kind != Kind::Wildcard)?;
            let mut s = json!({"order": *self.rng.pick(&["asc", "desc"])});
            if self.rng.chance(0.3) {
                s["missing"] = json!(*self.rng.pick(&["_first", "_last"]));
            }
            if self.rng.chance(0.15) && (f.kind.numeric()) {
                s["mode"] = json!(*self.rng.pick(&["min", "max", "avg", "sum"]));
            }
            keys.push(json!({f.path: s}));
        }
        if self.rng.chance(0.3)
            && let Some(g) = self.field(|f| f.kind == Kind::GeoPoint)
        {
            keys.push(json!({"_geo_distance": {g.path: {"lat": 0.0, "lon": 0.0}, "order": "asc", "unit": "km"}}));
        }
        Some(Value::Array(keys))
    }

    fn search_body(&mut self) -> Value {
        let mut b = Map::new();
        if self.rng.chance(0.9) {
            b.insert("query".into(), self.query(0));
        }
        let aggs = self.rng.chance(0.35);
        if aggs {
            b.insert("aggs".into(), self.aggs(0, false));
        }
        if aggs && self.rng.chance(0.4) {
            b.insert("size".into(), json!(0));
        } else if self.rng.chance(0.4) {
            b.insert("size".into(), json!(*self.rng.pick(&[1, 3, 5, 50])));
        }
        if self.rng.chance(0.15) {
            b.insert("from".into(), json!(1 + self.rng.below(5)));
        }
        let sorted = self.rng.chance(0.3);
        if sorted
            && let Some(s) = self.sort()
        {
            b.insert("sort".into(), s);
        }
        if self.rng.chance(0.2) {
            b.insert("track_total_hits".into(), json!(*self.rng.pick(&[json!(true), json!(false), json!(3)])));
        }
        if self.rng.chance(0.2) {
            let src = match self.rng.below(3) {
                0 => json!(false),
                1 => json!([self.schema.fields[0].path.clone()]),
                _ => json!({"excludes": [self.schema.fields[0].path.clone()]}),
            };
            b.insert("_source".into(), src);
        }
        if self.rng.chance(0.15)
            && let Some(f) = self.field(|f| f.kind.doc_values() && f.kind != Kind::GeoPoint)
        {
            b.insert("docvalue_fields".into(), json!([f.path]));
        }
        if self.rng.chance(0.15) {
            let f = self.schema.fields[self.rng.below(self.schema.fields.len())].path.clone();
            b.insert("fields".into(), json!([f]));
        }
        if self.rng.chance(0.2)
            && let Some(t) = self.field(|f| f.kind == Kind::Text)
        {
            let mut h = Map::new();
            let mut fh = Map::new();
            match self.rng.below(4) {
                0 => {
                    fh.insert("type".into(), json!("plain"));
                }
                1 if t.vectors => {
                    fh.insert("type".into(), json!("fvh"));
                }
                _ => {}
            }
            if self.rng.chance(0.3) {
                fh.insert("fragment_size".into(), json!(20));
                fh.insert("number_of_fragments".into(), json!(2));
            }
            if self.rng.chance(0.2) {
                fh.insert("number_of_fragments".into(), json!(0));
            }
            h.insert("fields".into(), json!({t.path.clone(): fh}));
            if self.rng.chance(0.3) {
                h.insert("pre_tags".into(), json!(["<b>"]));
                h.insert("post_tags".into(), json!(["</b>"]));
            }
            if !b.contains_key("query") || self.rng.chance(0.5) {
                b.insert("query".into(), json!({"match": {t.path: self.words(1, 2)}}));
            }
            b.insert("highlight".into(), Value::Object(h));
        }
        if self.rng.chance(0.1) {
            b.insert("post_filter".into(), self.query(2));
        }
        if self.rng.chance(0.05) {
            b.insert("min_score".into(), json!(0.5));
        }
        if self.rng.chance(0.08) && !sorted
            && let Some(k) = self.field(|f| f.kind == Kind::Keyword)
        {
            let mut c = json!({"field": k.path});
            if self.rng.chance(0.4) {
                c["inner_hits"] = json!({"name": "top", "size": 2});
            }
            b.insert("collapse".into(), c);
        }
        if self.rng.chance(0.07) && !sorted {
            b.insert("rescore".into(), json!({"window_size": 5, "query": {"rescore_query": self.query(2), "query_weight": 0.7, "rescore_query_weight": 1.3}}));
        }
        if self.rng.chance(0.1) {
            b.insert("suggest".into(), self.suggest());
        }
        if self.rng.chance(0.03) {
            b.insert("explain".into(), json!(true));
        }
        if self.rng.chance(0.05) {
            b.insert("version".into(), json!(true));
            b.insert("seq_no_primary_term".into(), json!(true));
        }
        if self.rng.chance(0.08) {
            b.insert("timeout".into(), json!("10s"));
        }
        if self.ext.chance(0.3) {
            self.with_ext(|g| g.ext_search(&mut b, sorted));
        }
        Value::Object(b)
    }

    fn suggest(&mut self) -> Value {
        let misspelt = |w: &str, r: &mut Rng| -> String {
            let mut c: Vec<char> = w.chars().collect();
            if c.len() > 3 {
                let i = 1 + r.below(c.len() - 2);
                c.swap(i, i + 1);
            }
            c.into_iter().collect()
        };
        let mut s = Map::new();
        if let Some(t) = self.field(|f| f.kind == Kind::Text) {
            let w = self.word().to_lowercase();
            let text = misspelt(&w, &mut self.rng);
            let mut term = json!({"field": t.path});
            if self.rng.chance(0.3) {
                term["suggest_mode"] = json!(*self.rng.pick(&["always", "popular", "missing"]));
            }
            s.insert("t".into(), json!({"text": text, "term": term}));
            if self.rng.chance(0.3) {
                let text = format!("{} {}", misspelt(&self.word().to_lowercase(), &mut self.rng), self.word().to_lowercase());
                s.insert("p".into(), json!({"text": text, "phrase": {"field": t.path, "size": 2}}));
            }
        }
        if let Some(c) = self.field(|f| f.kind == Kind::Completion) {
            let prefix = self.word().to_lowercase()[..2].to_string();
            let mut comp = json!({"field": c.path});
            if self.rng.chance(0.3) {
                comp["skip_duplicates"] = json!(true);
            }
            if self.rng.chance(0.3) {
                comp["fuzzy"] = json!({"fuzziness": 1});
            }
            s.insert("c".into(), json!({"prefix": prefix, "completion": comp}));
        }
        if s.is_empty() {
            s.insert("t".into(), json!({"text": "fxo", "term": {"field": self.schema.fields[0].path}}));
        }
        Value::Object(s)
    }

    fn search_step(&mut self) -> Step {
        let path = self.search_path();
        let body = self.search_body();
        let mut q = String::new();
        if self.rng.chance(0.1) {
            q.push_str("?search_type=dfs_query_then_fetch");
        } else if self.rng.chance(0.05) {
            q.push_str("?typed_keys=true");
        }
        Step::new("POST", format!("/{path}/_search{q}"), Body::Json(body))
    }

    fn count_step(&mut self) -> Step {
        let path = self.search_path();
        let body = if self.rng.chance(0.8) { json!({"query": self.query(0)}) } else { json!({}) };
        Step::new("POST", format!("/{path}/_count"), Body::Json(body))
    }

    fn msearch_step(&mut self) -> Step {
        let mut lines = Vec::new();
        for _ in 0..(1 + self.rng.below(3)) {
            let path = self.search_path();
            lines.push(json!({"index": path}));
            let mut body = self.search_body();
            body.as_object_mut().unwrap().remove("suggest");
            lines.push(body);
        }
        Step::new("POST", "/_msearch", Body::Lines(lines))
    }

    fn get_step(&mut self) -> Step {
        let idx = self.target();
        let name = self.indices[idx].clone();
        let id = self.existing_id(idx).unwrap_or_else(|| "missing".into());
        match self.rng.below(4) {
            0 => {
                let other = self.existing_id(idx).unwrap_or_else(|| "nope".into());
                Step::new("POST", format!("/{name}/_mget"), Body::Json(json!({"ids": [id, other, "nope"]})))
            }
            1 => Step::new("GET", format!("/{name}/_doc/{id}?_source_excludes={}", self.schema.fields[0].path), Body::None),
            2 => Step::new("GET", format!("/{name}/_source/{id}"), Body::None),
            _ => Step::new("GET", format!("/{name}/_doc/{id}"), Body::None),
        }
    }

    fn explain_step(&mut self) -> Step {
        let idx = self.target();
        let name = self.indices[idx].clone();
        let id = self.existing_id(idx).unwrap_or_else(|| "1".into());
        let f = self.field(|f| f.kind == Kind::Text).unwrap();
        let q = json!({"query": {"match": {f.path: self.words(1, 2)}}});
        Step::new("POST", format!("/{name}/_explain/{id}"), Body::Json(q))
    }

    fn scroll_steps(&mut self) -> Vec<Step> {
        let path = self.search_path();
        let mut body = json!({"query": self.query(1), "size": 1 + self.rng.below(4)});
        // (The draw happens either way, so seeds keep their cases.)
        if self.rng.chance(0.5) && !self.deleted {
            body["sort"] = json!(["_doc"]);
        }
        let first = Step::new("POST", format!("/{path}/_search?scroll=1m"), Body::Json(body)).capture("scroll", "/_scroll_id");
        let mut out = vec![first];
        for _ in 0..(1 + self.rng.below(3)) {
            out.push(Step::new("POST", "/_search/scroll", Body::Json(json!({"scroll": "1m", "scroll_id": "${scroll}"}))).capture("scroll", "/_scroll_id"));
        }
        out.push(Step::new("DELETE", "/_search/scroll", Body::Json(json!({"scroll_id": "${scroll}"}))));
        out
    }

    fn pit_steps(&mut self) -> Vec<Step> {
        let path = self.search_path();
        let open = Step::new("POST", format!("/{path}/_pit?keep_alive=1m"), Body::None).capture("pit", "/id");
        let size = 1 + self.rng.below(4);
        let sort_field = self.field(|f| f.kind.integral() || f.kind == Kind::Date).map(|f| f.path);
        let mut sort = vec![];
        if let Some(f) = sort_field {
            sort.push(json!({f: "asc"}));
        }
        sort.push(json!({"_shard_doc": "desc"}));
        let q = self.query(1);
        let mut out = vec![open];
        out.push(
            Step::new("POST", "/_search", Body::Json(json!({"query": q, "size": size, "pit": {"id": "${pit}", "keep_alive": "1m"}, "sort": sort})))
                .capture("after", "/hits/hits/-1/sort")
                .capture("pit", "/pit_id"),
        );
        out.push(Step::new(
            "POST",
            "/_search",
            Body::Json(json!({"query": q, "size": size, "pit": {"id": "${pit}"}, "sort": sort, "search_after": "${after}"})),
        ));
        out.push(Step::new("DELETE", "/_pit", Body::Json(json!({"id": "${pit}"}))));
        out
    }

    fn field_caps_step(&mut self) -> Step {
        let path = self.search_path();
        let f = if self.rng.chance(0.5) { "*".to_string() } else { self.schema.fields[self.rng.below(self.schema.fields.len())].path.clone() };
        Step::new("GET", format!("/{path}/_field_caps?fields={f}"), Body::None)
    }

    fn index_step(&mut self) -> Step {
        let idx = self.target();
        let name = self.indices[idx].clone();
        let id = if self.rng.chance(0.4) { self.existing_id(idx).unwrap_or_else(|| self.new_id()) } else { self.new_id() };
        if !self.ids[idx].contains(&id) {
            self.ids[idx].push(id.clone());
        }
        let refresh = if self.rng.chance(0.2) { "?refresh=true" } else { "" };
        Step::new("PUT", format!("/{name}/_doc/{id}{refresh}"), Body::Json(self.doc()))
    }

    fn update_step(&mut self) -> Step {
        let idx = self.target();
        let name = self.indices[idx].clone();
        let id = self.existing_id(idx).unwrap_or_else(|| "missing".into());
        let mut partial = Map::new();
        let f = self.field(|f| !f.path.contains('.')).unwrap();
        partial.insert(f.path.clone(), self.scalar(f.kind));
        let mut body = json!({"doc": partial});
        if self.rng.chance(0.2) {
            body["doc_as_upsert"] = json!(true);
        }
        if self.rng.chance(0.2)
            && let Some(n) = self.field(|f| f.kind.integral() && !f.path.contains('.'))
        {
            let p = n.path;
            body = json!({"script": {"source": format!("if (ctx._source.{p} instanceof Number) {{ ctx._source.{p} += params.d }} else {{ ctx._source.{p} = params.d }}"), "params": {"d": 3}}});
        }
        Step::new("POST", format!("/{name}/_update/{id}"), Body::Json(body))
    }

    fn delete_step(&mut self) -> Step {
        let idx = self.target();
        let name = self.indices[idx].clone();
        let id = match self.existing_id(idx) {
            Some(id) => {
                self.ids[idx].retain(|x| x != &id);
                id
            }
            None => "missing".into(),
        };
        self.deleted = true;
        Step::new("DELETE", format!("/{name}/_doc/{id}"), Body::None)
    }

    fn delete_by_query_step(&mut self) -> Step {
        let idx = self.target();
        let name = self.indices[idx].clone();
        let q = match self.field(|f| f.kind == Kind::Keyword) {
            Some(k) => json!({"term": {k.path: *self.rng.pick(KEYWORDS)}}),
            None => self.query(2),
        };
        // The generator no longer knows which ids exist; reads of deleted ids are fine.
        self.deleted = true;
        Step::new("POST", format!("/{name}/_delete_by_query?refresh=true"), Body::Json(json!({"query": q})))
    }

    fn update_by_query_step(&mut self) -> Step {
        let idx = self.target();
        let name = self.indices[idx].clone();
        let mut body = json!({"query": self.query(2)});
        if let Some(n) = self.field(|f| f.kind.integral() && !f.path.contains('.')) {
            let p = n.path;
            body["script"] = json!({"source": format!("if (ctx._source.{p} instanceof Number) {{ ctx._source.{p} += 1 }}")});
        }
        Step::new("POST", format!("/{name}/_update_by_query?refresh=true"), Body::Json(body))
    }
}

#[path = "extended.rs"]
mod extended;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let a = case(1, 42);
        let b = case(1, 42);
        assert_eq!(a.steps.len(), b.steps.len());
        for (x, y) in a.steps.iter().zip(&b.steps) {
            assert_eq!(x.label(), y.label());
        }
    }

    #[test]
    fn iso_dates() {
        assert_eq!(iso(BASE, true), "2024-01-01");
        assert_eq!(iso(BASE + 59 * DAY + 3_600_000, false), "2024-02-29T01:00:00Z");
    }
}
