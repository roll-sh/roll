//! `composite`: paginated buckets over the combinations of several sources (`terms`,
//! `histogram`, `date_histogram`, `geotile_grid`), in key order, after an optional `after` key.
//!
//! Each shard keeps the `size` smallest composite keys after `after` in a bounded queue
//! (Elasticsearch's `CompositeValuesCollectorQueue`): a key already queued counts the doc, a new
//! key enters while there is room or when it sorts before the largest queued key, which it
//! evicts. Keys that ever leave the queue can never come back (the bound only shrinks), so the
//! counts of the final keys are exact. Documents yield the cartesian product of their sources'
//! values (consecutive duplicates skipped, as Elasticsearch does), and a missing value is a
//! `null` key part when `missing_bucket` is set. Sub-aggregations run in a second pass over the
//! recorded documents, for the final keys only. Reduces merge the key-sorted shard lists and
//! keep the first `size` keys; `after_key` is the last bucket's key.

use std::cmp::Ordering;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::Arc;

use search_index::{Searcher, SegmentContext};
use smallvec::SmallVec;

use crate::bucket::histogram::{DateInterval, create_rounding, parse_string_offset, set_interval};
use crate::collect::{Aggregator, Batch, BindCtx, SizedAncestor, Subs};
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{AggValue, Aggs, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, multi_bucket_property};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::rounding::Rounding;
use crate::values::{BoundVs, BytesReader, DocCounter, GeoReader, NumReader, VsSpec, VsType, bind};
use crate::xparser::{Field, Token, Tokens, XParser};

/// `MissingOrder`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingOrder {
    Default,
    First,
    Last,
}

impl MissingOrder {
    /// `compareAnyValueToMissing(reverseMul)`.
    fn value_vs_missing(self, reverse_mul: i32) -> i32 {
        match self {
            MissingOrder::Default => reverse_mul,
            MissingOrder::First => 1,
            MissingOrder::Last => -1,
        }
    }
}

/// A composite source's kind.
#[derive(Clone, Debug)]
pub enum SourceKind {
    Terms,
    Histogram { interval: f64 },
    DateHistogram { interval: DateInterval, offset: i64 },
    GeoTile { precision: u32, bounds: Option<crate::bucket::composite::GeoBounds> },
}

impl SourceKind {
    fn type_name(&self) -> &'static str {
        match self {
            SourceKind::Terms => "terms",
            SourceKind::Histogram { .. } => "histogram",
            SourceKind::DateHistogram { .. } => "date_histogram",
            SourceKind::GeoTile { .. } => "geotile_grid",
        }
    }
}

/// A `bounds` box (`top_left`, `bottom_right`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoBounds {
    pub top: f64,
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
}

/// One parsed source.
#[derive(Clone, Debug)]
pub struct SourceSpec {
    pub name: String,
    pub kind: SourceKind,
    pub vs: VsSpec,
    pub missing_bucket: bool,
    pub missing_order: MissingOrder,
    pub desc: bool,
}

/// A parsed `composite`.
#[derive(Clone, Debug)]
pub struct CompositeSpec {
    pub sources: Vec<SourceSpec>,
    pub size: i32,
    pub after: Option<Json>,
}

/// `CompositeAggregationBuilder.PARSER` (a constructing parser: the builder, and its checks
/// on `sources`, are made as soon as `sources` is read).
pub fn parse(p: &mut XParser<'_>) -> Result<CompositeSpec> {
    let fields = [Field::new("sources", Tokens::OBJECT_ARRAY), Field::new("size", Tokens::INT), Field::new("after", Tokens::OBJECT)];
    let mut sources: Option<Vec<SourceSpec>> = None;
    let mut size = 10;
    let mut after = None;
    p.object("composite", &fields, |p, i| {
        match i {
            0 => {
                let list = p.array_items(parse_source)?;
                validate_sources(&list)
                    .map_err(|e| AggError::new(400, "x_content_parse_exception", "Failed to build [composite] after last required field arrived").caused_by(e))?;
                sources = Some(list);
            }
            1 => size = p.int_value()?,
            _ => after = Some(p.map()?),
        }
        Ok(())
    })?;
    let Some(sources) = sources else {
        return Err(AggError::illegal_argument("Required [sources]"));
    };
    Ok(CompositeSpec { sources, size, after })
}

fn validate_sources(list: &[SourceSpec]) -> Result<()> {
    if list.is_empty() {
        return Err(AggError::illegal_argument("Composite [sources] cannot be null or empty"));
    }
    let mut seen: Vec<&str> = Vec::new();
    let mut dups: Vec<&str> = Vec::new();
    for s in list {
        if seen.contains(&s.name.as_str()) {
            if !dups.contains(&s.name.as_str()) {
                dups.push(&s.name);
            }
        } else {
            seen.push(&s.name);
        }
    }
    if !dups.is_empty() {
        return Err(AggError::illegal_argument(format!("Composite source names must be unique, found duplicates: [{}]", dups.join(", "))));
    }
    Ok(())
}

/// `CompositeValuesSourceParserHelper.fromXContent`.
fn parse_source(p: &mut XParser<'_>) -> Result<SourceSpec> {
    let expect = |p: &XParser<'_>, want: Token| -> Result<()> {
        let t = p.token();
        if t != want {
            return Err(p.parsing_error(format!("Failed to parse object: expecting token of type [{want}] but found [{t}]")));
        }
        Ok(())
    };
    expect(p, Token::StartObject)?;
    p.next_token();
    expect(p, Token::FieldName)?;
    let name = p.current_name().unwrap_or("").to_string();
    p.next_token();
    expect(p, Token::StartObject)?;
    p.next_token();
    expect(p, Token::FieldName)?;
    let ty = p.current_name().unwrap_or("").to_string();
    p.next_token();
    expect(p, Token::StartObject)?;
    let kind = match ty.as_str() {
        "terms" => SourceKind::Terms,
        "histogram" => SourceKind::Histogram { interval: 0.0 },
        "date_histogram" => SourceKind::DateHistogram { interval: DateInterval::None, offset: 0 },
        "geotile_grid" => SourceKind::GeoTile { precision: 7, bounds: None },
        _ => return Err(p.parsing_error(format!("invalid source type: {ty}"))),
    };
    let mut spec = SourceSpec { name, kind, vs: VsSpec::default(), missing_bucket: false, missing_order: MissingOrder::Default, desc: false };
    let mut fields = vec![
        Field::new("field", Tokens::STRING),
        Field::new("missing_bucket", Tokens::BOOLEAN),
        Field::new("missing_order", Tokens::STRING),
        Field::with_deprecated("value_type", &["valueType"], Tokens::STRING),
        Field::new("script", Tokens::OBJECT_OR_STRING),
        Field::new("order", Tokens::STRING),
    ];
    match &spec.kind {
        SourceKind::Histogram { .. } => fields.push(Field::new("interval", Tokens::FLOAT)),
        SourceKind::DateHistogram { .. } => {
            fields.extend_from_slice(&[
                Field::new("format", Tokens::STRING),
                Field::new("calendar_interval", Tokens::STRING),
                Field::new("fixed_interval", Tokens::STRING),
                Field::new("offset", Tokens::LONG),
                Field::new("time_zone", Tokens::LONG),
            ]);
        }
        SourceKind::GeoTile { .. } => {
            fields.push(Field::new("precision", Tokens::INT));
            fields.push(Field::new("bounds", Tokens::OBJECT));
        }
        SourceKind::Terms => {}
    }
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let type_name = spec.kind.type_name();
    p.object(type_name, &fields, |p, i| {
        let f = names[i];
        match f {
            "missing_bucket" => spec.missing_bucket = p.boolean_value()?,
            "missing_order" => {
                let v = p.text()?;
                spec.missing_order = match v.to_uppercase().as_str() {
                    "DEFAULT" => MissingOrder::Default,
                    "FIRST" => MissingOrder::First,
                    "LAST" => MissingOrder::Last,
                    other => {
                        return Err(AggError::illegal_argument(format!(
                            "No enum constant org.elasticsearch.search.aggregations.bucket.composite.MissingOrder.{other}"
                        )));
                    }
                };
            }
            "order" => {
                let v = p.text()?;
                spec.desc = match v.to_uppercase().as_str() {
                    "ASC" => false,
                    "DESC" => true,
                    other => return Err(AggError::illegal_argument(format!("No enum constant org.elasticsearch.search.sort.SortOrder.{other}"))),
                };
            }
            "interval" => {
                let v = p.double_value()?;
                if v <= 0.0 {
                    return Err(AggError::illegal_argument("[interval] must be greater than 0 for [histogram] source"));
                }
                spec.kind = SourceKind::Histogram { interval: v };
            }
            "calendar_interval" | "fixed_interval" => {
                if let SourceKind::DateHistogram { interval, .. } = &mut spec.kind {
                    set_interval(interval, f == "calendar_interval", &p.text()?)?;
                }
            }
            "offset" => {
                let v = if p.token() == Token::Number { p.long_value()? } else { parse_string_offset(&p.text()?)? };
                if let SourceKind::DateHistogram { offset, .. } = &mut spec.kind {
                    *offset = v;
                }
            }
            "precision" => {
                let v = p.int_value()?;
                if !(0..=crate::geo::MAX_ZOOM as i32).contains(&v) {
                    return Err(AggError::illegal_argument(format!("Invalid geotile_grid precision of {v}. Must be between 0 and 29.")));
                }
                if let SourceKind::GeoTile { precision, .. } = &mut spec.kind {
                    *precision = v as u32;
                }
            }
            "bounds" => {
                let b = parse_bounds(p)?;
                if let SourceKind::GeoTile { bounds, .. } = &mut spec.kind {
                    *bounds = Some(b);
                }
            }
            other => {
                spec.vs.parse_field(p, other)?;
            }
        }
        Ok(())
    })?;
    // The two END_OBJECTs closing `{"name": {"type": {…}}}`.
    p.next_token();
    p.next_token();
    Ok(spec)
}

/// `GeoBoundingBox.parseBoundingBox`: `top_left` + `bottom_right` (or `top`, `left`, `bottom`,
/// `right`), points as objects, arrays or `"lat,lon"` strings.
pub fn parse_bounds(p: &mut XParser<'_>) -> Result<GeoBounds> {
    let v = p.map()?;
    let point = |j: &Json| -> Result<(f64, f64)> { crate::values::parse_geo_point_text(j) };
    let (mut top, mut left, mut bottom, mut right) = (f64::NAN, f64::NAN, f64::NAN, f64::NAN);
    for (k, val) in v.as_object().unwrap_or(&[]) {
        match k.as_str() {
            "top_left" | "topLeft" => {
                let (lat, lon) = point(val)?;
                top = lat;
                left = lon;
            }
            "bottom_right" | "bottomRight" => {
                let (lat, lon) = point(val)?;
                bottom = lat;
                right = lon;
            }
            "top_right" | "topRight" => {
                let (lat, lon) = point(val)?;
                top = lat;
                right = lon;
            }
            "bottom_left" | "bottomLeft" => {
                let (lat, lon) = point(val)?;
                bottom = lat;
                left = lon;
            }
            "top" => top = val.as_f64().unwrap_or(f64::NAN),
            "left" => left = val.as_f64().unwrap_or(f64::NAN),
            "bottom" => bottom = val.as_f64().unwrap_or(f64::NAN),
            "right" => right = val.as_f64().unwrap_or(f64::NAN),
            "wkt" => return Err(AggError::new(400, "parse_exception", "wkt bounding boxes are not supported")),
            other => return Err(AggError::new(400, "parse_exception", format!("failed to parse bounding box. unexpected field [{other}]"))),
        }
    }
    if top.is_nan() || left.is_nan() || bottom.is_nan() || right.is_nan() {
        return Err(AggError::new(400, "parse_exception", "failed to parse bounding box. Missing value for [top, left, bottom, right]"));
    }
    Ok(GeoBounds { top, left, bottom, right })
}

/// One part of a composite key (`Comparable`: `BytesRef`, `Long` or `Double`; `None` missing).
#[derive(Clone, Debug)]
pub enum KeyPart {
    Null,
    Long(i64),
    Double(f64),
    Bytes(Box<[u8]>),
}

impl PartialEq for KeyPart {
    fn eq(&self, o: &Self) -> bool {
        match (self, o) {
            (KeyPart::Null, KeyPart::Null) => true,
            (KeyPart::Long(a), KeyPart::Long(b)) => a == b,
            (KeyPart::Double(a), KeyPart::Double(b)) => a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()),
            (KeyPart::Bytes(a), KeyPart::Bytes(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for KeyPart {}

impl Hash for KeyPart {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match self {
            KeyPart::Null => 0u8.hash(h),
            KeyPart::Long(v) => v.hash(h),
            KeyPart::Double(v) => (if v.is_nan() { f64::NAN } else { *v }).to_bits().hash(h),
            KeyPart::Bytes(b) => b.hash(h),
        }
    }
}

/// A composite key.
pub type CompositeKey = SmallVec<[KeyPart; 3]>;

/// A document's value of one source while collecting (bytes live in the source's arena):
/// keys are only materialized as [`KeyPart`]s when they enter the queue.
#[derive(Clone, Copy, Debug)]
enum Val {
    Null,
    Long(i64),
    Double(f64),
    /// `(start, len)` in the source's arena.
    Bytes(u32, u32),
}

/// A key part, borrowed: a collected [`Val`] or a stored [`KeyPart`].
#[derive(Clone, Copy, Debug)]
enum PartView<'a> {
    Null,
    Long(i64),
    Double(f64),
    Bytes(&'a [u8]),
}

impl KeyPart {
    fn view(&self) -> PartView<'_> {
        match self {
            KeyPart::Null => PartView::Null,
            KeyPart::Long(v) => PartView::Long(*v),
            KeyPart::Double(v) => PartView::Double(*v),
            KeyPart::Bytes(b) => PartView::Bytes(b),
        }
    }
}

impl PartView<'_> {
    fn to_owned_part(self) -> KeyPart {
        match self {
            PartView::Null => KeyPart::Null,
            PartView::Long(v) => KeyPart::Long(v),
            PartView::Double(v) => KeyPart::Double(v),
            PartView::Bytes(b) => KeyPart::Bytes(b.into()),
        }
    }

    /// `KeyPart` equality.
    fn same(self, o: PartView<'_>) -> bool {
        match (self, o) {
            (PartView::Null, PartView::Null) => true,
            (PartView::Long(a), PartView::Long(b)) => a == b,
            (PartView::Double(a), PartView::Double(b)) => a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()),
            (PartView::Bytes(a), PartView::Bytes(b)) => a == b,
            _ => false,
        }
    }

    fn hash_into(self, h: &mut impl Hasher) {
        match self {
            PartView::Null => h.write_u8(0),
            PartView::Long(v) => h.write_u64(v as u64),
            PartView::Double(v) => h.write_u64(if v.is_nan() { f64::NAN.to_bits() } else { v.to_bits() }),
            PartView::Bytes(b) => {
                h.write_usize(b.len());
                h.write(b);
            }
        }
    }

    /// One part of `compareKey`.
    fn compare(self, o: PartView<'_>, ord: &PartOrder) -> i32 {
        match (self, o) {
            (PartView::Null, PartView::Null) => 0,
            (PartView::Null, _) => -ord.missing.value_vs_missing(ord.reverse_mul),
            (_, PartView::Null) => ord.missing.value_vs_missing(ord.reverse_mul),
            (PartView::Long(a), PartView::Long(b)) => a.cmp(&b) as i32 * ord.reverse_mul,
            (PartView::Double(a), PartView::Double(b)) => crate::request::java_double_compare(a, b) as i32 * ord.reverse_mul,
            (PartView::Bytes(a), PartView::Bytes(b)) => a.cmp(b) as i32 * ord.reverse_mul,
            _ => 0,
        }
    }
}

/// The hash of a key's parts (collected and stored keys hash alike).
fn hash_key<'a>(parts: impl Iterator<Item = PartView<'a>>) -> u64 {
    let mut h = foldhash::fast::FixedState::with_seed(0x5eed_c0de).build_hasher();
    for p in parts {
        p.hash_into(&mut h);
    }
    h.finish()
}

/// `compareKey` over borrowed parts.
fn compare_views<'a, 'b>(a: impl Iterator<Item = PartView<'a>>, b: impl Iterator<Item = PartView<'b>>, orders: &[PartOrder]) -> Ordering {
    for ((x, y), o) in a.zip(b).zip(orders) {
        let c = x.compare(y, o);
        if c != 0 {
            return c.cmp(&0);
        }
    }
    Ordering::Equal
}

/// Key order of one source.
#[derive(Clone, Copy, Debug)]
pub struct PartOrder {
    pub reverse_mul: i32,
    pub missing: MissingOrder,
}

/// `InternalBucket.compareKey`.
pub fn compare_keys(a: &[KeyPart], b: &[KeyPart], orders: &[PartOrder]) -> Ordering {
    for ((x, y), o) in a.iter().zip(b).zip(orders) {
        let c = match (x, y) {
            (KeyPart::Null, KeyPart::Null) => 0,
            (KeyPart::Null, _) => -o.missing.value_vs_missing(o.reverse_mul),
            (_, KeyPart::Null) => o.missing.value_vs_missing(o.reverse_mul),
            (KeyPart::Long(a), KeyPart::Long(b)) => a.cmp(b) as i32 * o.reverse_mul,
            (KeyPart::Double(a), KeyPart::Double(b)) => crate::request::java_double_compare(*a, *b) as i32 * o.reverse_mul,
            (KeyPart::Bytes(a), KeyPart::Bytes(b)) => a.cmp(b) as i32 * o.reverse_mul,
            _ => 0,
        };
        if c != 0 {
            return c.cmp(&0);
        }
    }
    Ordering::Equal
}

/// `InternalComposite.formatObject`.
fn format_part(v: &KeyPart, f: &DocValueFormat) -> Json {
    match v {
        KeyPart::Null => Json::Null,
        KeyPart::Bytes(b) => match f {
            DocValueFormat::Raw => Json::Str(String::from_utf8_lossy(b).into_owned()),
            other => other.format_bytes(b),
        },
        KeyPart::Long(l) => match f {
            DocValueFormat::Raw => Json::Int(*l),
            other => other.format_long(*l),
        },
        KeyPart::Double(d) => match f {
            DocValueFormat::Raw => Json::Float(*d),
            other => other.format_double(*d),
        },
    }
}

/// A composite bucket.
#[derive(Clone, Debug)]
pub struct CompositeBucket {
    pub key: CompositeKey,
    pub doc_count: u64,
    pub aggs: Aggs,
}

/// `InternalComposite`.
#[derive(Clone, Debug)]
pub struct Composite {
    pub size: usize,
    pub names: Arc<Vec<String>>,
    pub formats: Arc<Vec<DocValueFormat>>,
    pub orders: Arc<Vec<PartOrder>>,
    pub buckets: Vec<CompositeBucket>,
    pub after_key: Option<CompositeKey>,
}

impl Composite {
    fn key_json(&self, key: &[KeyPart]) -> Json {
        let mut o = Json::obj();
        for ((n, v), f) in self.names.iter().zip(key).zip(self.formats.iter()) {
            o.push(n.clone(), format_part(v, f));
        }
        o
    }
}

impl Internal for Composite {
    fn type_name(&self) -> &'static str {
        "composite"
    }
    fn class_name(&self) -> &'static str {
        "InternalComposite"
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let first = &list[0];
        let (size, names, formats, orders) = (first.size, first.names.clone(), first.formats.clone(), first.orders.clone());
        let lists: Vec<Vec<CompositeBucket>> = list.into_iter().map(|c| c.buckets).collect();
        let ord = orders.clone();
        let groups = crate::util::lucene_merge_groups(lists, |a, b| compare_keys(&a.key, &b.key, &ord) == Ordering::Less);
        let mut out = Vec::new();
        for g in groups {
            if out.len() >= size {
                break;
            }
            let key = g[0].key.clone();
            let doc_count = g.iter().map(|b| b.doc_count).sum();
            let aggs = Aggs::reduce_hashed(g.into_iter().map(|b| b.aggs).collect(), ctx)?;
            out.push(CompositeBucket { key, doc_count, aggs });
        }
        ctx.consume_buckets(out.len())?;
        let after_key = out.last().map(|b| b.key.clone());
        Ok(Composite { size, names, formats, orders, buckets: out, after_key })
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        if let Some(k) = &self.after_key {
            out.push("after_key", self.key_json(k));
        }
        let buckets = self
            .buckets
            .iter()
            .map(|b| {
                let mut o = Json::obj();
                o.push("key", self.key_json(&b.key));
                o.push("doc_count", Json::from(b.doc_count));
                b.aggs.render_into(&mut o, rc);
                o
            })
            .collect();
        out.push("buckets", Json::Arr(buckets));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }
    fn ram_bytes(&self) -> usize {
        64 + self.buckets.iter().map(|b| 48 + b.key.len() * 24 + b.aggs.ram_bytes()).sum::<usize>()
    }
}

impl MultiBuckets for Composite {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    fn key(&self, i: usize) -> Json {
        self.key_json(&self.buckets[i].key)
    }
    /// `getKeyAsString()`: `{a=x, b=y}`.
    fn key_string(&self, i: usize) -> String {
        let mut s = String::from("{");
        for (j, ((n, v), f)) in self.names.iter().zip(&self.buckets[i].key).zip(self.formats.iter()).enumerate() {
            if j > 0 {
                s.push_str(", ");
            }
            s.push_str(n);
            s.push('=');
            s.push_str(&format_part(v, f).to_java_string());
        }
        s.push('}');
        s
    }
    fn aggs(&self, i: usize) -> &Aggs {
        &self.buckets[i].aggs
    }
    fn aggs_mut(&mut self, i: usize) -> &mut Aggs {
        &mut self.buckets[i].aggs
    }
    fn retain_buckets(&mut self, keep: &[bool]) {
        let mut i = 0;
        self.buckets.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
    }
    fn reorder(&mut self, order: &[usize]) {
        let mut slots: Vec<Option<CompositeBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
}

/// The Java factory class a parent aggregation type builds (for the parent check's message).
fn factory_class(t: &str) -> String {
    let camel: String = t
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect();
    format!("{camel}AggregatorFactory")
}

/// How a source reads a document.
enum SourceReader {
    None,
    /// Keyword-like fields without script or `missing`: ordinals, each term decoded once per
    /// segment (`terms[spans[ord]]`).
    Ords { dv: crate::values::DvOrds, spans: Vec<(u32, u32)>, terms: Vec<u8> },
    Bytes(BytesReader),
    Num(NumReader),
    Geo(GeoReader),
}

/// A bound source.
struct BoundSource {
    spec: SourceSpec,
    vs: BoundVs,
    /// Values are doubles (double fields, histograms).
    double: bool,
    /// Values are bytes (keyword-like terms).
    bytes: bool,
    rounding: Option<Rounding>,
    tile_bounds: Option<crate::geo::TileBounds>,
    reader: SourceReader,
    /// This document's values.
    vals: Vec<Val>,
    /// Bytes of this document's byte values.
    arena: Vec<u8>,
}

impl BoundSource {
    #[inline]
    fn view(&self, v: Val) -> PartView<'_> {
        match v {
            Val::Null => PartView::Null,
            Val::Long(x) => PartView::Long(x),
            Val::Double(x) => PartView::Double(x),
            Val::Bytes(s, l) => PartView::Bytes(&self.arena[s as usize..(s + l) as usize]),
        }
    }

    fn load(&mut self, doc: u32, score: f32) -> Result<()> {
        self.vals.clear();
        self.arena.clear();
        match &mut self.reader {
            SourceReader::None => {}
            SourceReader::Ords { dv, spans, terms } => {
                let arena = &mut self.arena;
                let vals = &mut self.vals;
                let mut buf = Vec::new();
                dv.for_each(doc, |o| {
                    let o = o as usize;
                    if spans[o].1 == u32::MAX {
                        dv.lookup(o as u64, &mut buf);
                        spans[o] = (terms.len() as u32, buf.len() as u32);
                        terms.extend_from_slice(&buf);
                    }
                    let (st, len) = spans[o];
                    let start = arena.len() as u32;
                    arena.extend_from_slice(&terms[st as usize..(st + len) as usize]);
                    vals.push(Val::Bytes(start, len));
                });
            }
            SourceReader::Bytes(r) => {
                let n = r.load(doc, score)?;
                for i in 0..n {
                    let v = r.value(i);
                    if i > 0 && v == r.value(i - 1) {
                        continue;
                    }
                    let start = self.arena.len() as u32;
                    self.arena.extend_from_slice(v);
                    self.vals.push(Val::Bytes(start, v.len() as u32));
                }
            }
            SourceReader::Num(r) => match &self.spec.kind {
                SourceKind::Histogram { interval } => {
                    let interval = *interval;
                    for &v in r.doubles(doc, score)? {
                        let k = (v / interval).floor() * interval;
                        if !matches!(self.vals.last(), Some(Val::Double(p)) if p.to_bits() == k.to_bits()) {
                            self.vals.push(Val::Double(k));
                        }
                    }
                }
                SourceKind::DateHistogram { .. } => {
                    let rounding = self.rounding.as_ref().expect("rounding");
                    for &v in r.longs(doc, score)? {
                        let k = rounding.round(v);
                        if !matches!(self.vals.last(), Some(Val::Long(p)) if *p == k) {
                            self.vals.push(Val::Long(k));
                        }
                    }
                }
                _ if self.double => {
                    for &v in r.doubles(doc, score)? {
                        if !matches!(self.vals.last(), Some(Val::Double(p)) if p.to_bits() == v.to_bits()) {
                            self.vals.push(Val::Double(v));
                        }
                    }
                }
                _ => {
                    for &v in r.longs(doc, score)? {
                        if !matches!(self.vals.last(), Some(Val::Long(p)) if *p == v) {
                            self.vals.push(Val::Long(v));
                        }
                    }
                }
            },
            SourceReader::Geo(r) => {
                let SourceKind::GeoTile { precision, .. } = &self.spec.kind else { unreachable!() };
                let precision = *precision;
                let n = 1i32 << precision;
                let mut tiles: SmallVec<[i64; 4]> = SmallVec::new();
                for &(lat, lon) in r.points(doc) {
                    let (x, y) = (crate::geo::tile_x(lon, n), crate::geo::tile_y(lat, n));
                    if self.tile_bounds.is_none_or(|b| b.valid(x as i64, y as i64, precision)) {
                        tiles.push(crate::geo::geotile_long_tiles(precision, x as u32, y as u32));
                    }
                }
                tiles.sort_unstable();
                for t in tiles {
                    if !matches!(self.vals.last(), Some(Val::Long(p)) if *p == t) {
                        self.vals.push(Val::Long(t));
                    }
                }
            }
        }
        if self.vals.is_empty() && self.spec.missing_bucket {
            self.vals.push(Val::Null);
        }
        Ok(())
    }

    /// `SingleDimensionValuesSource.setAfter`.
    fn after_part(&self, v: &Json, now: i64) -> Result<KeyPart> {
        let bad = || {
            AggError::illegal_argument(format!(
                "Invalid value for [after.{}], expected comparable, got [{}]",
                self.spec.name,
                match v {
                    Json::Null => "null",
                    Json::Arr(_) => "ArrayList",
                    Json::Obj(_) => "LinkedHashMap",
                    _ => "Object",
                }
            ))
        };
        if matches!(v, Json::Null) {
            return if self.spec.missing_bucket { Ok(KeyPart::Null) } else { Err(bad()) };
        }
        if matches!(v, Json::Arr(_) | Json::Obj(_)) {
            return Err(bad());
        }
        let text = || match v {
            Json::Str(s) => s.clone(),
            other => other.to_java_string(),
        };
        let format = &self.vs.format;
        if self.bytes {
            return Ok(KeyPart::Bytes(format.parse_bytes(&Json::Str(text()))?.into()));
        }
        if self.double {
            return Ok(KeyPart::Double(match v {
                Json::Int(_) | Json::UInt(_) | Json::Float(_) | Json::Float32(_) => v.as_f64().unwrap_or(0.0),
                _ => format.parse_double(&text(), false, now)?,
            }));
        }
        Ok(KeyPart::Long(match v {
            Json::Int(i) => *i,
            Json::UInt(u) => *u as i64,
            Json::Float(f) => *f as i64,
            Json::Float32(f) => *f as i64,
            Json::Bool(b) => i64::from(*b),
            _ => format.parse_long(&text(), false, now)?,
        }))
    }
}

/// Binds `composite`.
pub fn create(def: &AggDef, spec: &CompositeSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    if let Some(p) = ctx.parents.iter().rev().find(|t| !matches!(**t, "nested" | "filter" | "random_sampler")) {
        return Err(AggError::illegal_argument(format!(
            "[composite] aggregation cannot be used with a parent aggregation of type: [{}]",
            factory_class(p)
        )));
    }
    let now = ctx.lookup.now_millis();
    let mut sources = Vec::with_capacity(spec.sources.len());
    for s in &spec.sources {
        if !s.missing_bucket && s.missing_order != MissingOrder::Default {
            return Err(AggError::illegal_argument("missingOrder can only be set if missingBucket is true"));
        }
        let (default, tz_aware) = match &s.kind {
            SourceKind::Terms => (VsType::Keyword, false),
            SourceKind::Histogram { .. } => (VsType::Numeric, false),
            SourceKind::DateHistogram { .. } => (VsType::Date, true),
            SourceKind::GeoTile { .. } => (VsType::GeoPoint, false),
        };
        let mut vs = bind(&s.vs, ctx.lookup, default, tz_aware)?;
        let unsupported = match &s.kind {
            SourceKind::Terms => {
                matches!(vs.vs_type, VsType::GeoPoint | VsType::CartesianPoint | VsType::GeoShape | VsType::CartesianShape | VsType::Range | VsType::Histogram | VsType::AggregateMetric | VsType::ExponentialHistogram) || vs.is_counter()
            }
            SourceKind::Histogram { .. } => !vs.vs_type.is_numeric(),
            SourceKind::DateHistogram { .. } => !vs.vs_type.is_numeric() || vs.is_counter(),
            SourceKind::GeoTile { .. } => vs.vs_type != VsType::GeoPoint,
        };
        if unsupported && !vs.unmapped {
            return Err(vs.unsupported(s.kind.type_name()));
        }
        if vs.script.as_ref().is_some_and(|x| x.needs_score()) {
            return Err(AggError::illegal_argument("[sources] cannot access _score"));
        }
        let mut rounding = None;
        let mut tile_bounds = None;
        match &s.kind {
            SourceKind::Terms if vs.vs_type == VsType::Date && s.vs.format.is_none() => vs.format = DocValueFormat::Raw,
            SourceKind::DateHistogram { interval, offset } => {
                rounding = Some(create_rounding(interval, vs.time_zone.as_ref(), *offset)?);
                if s.vs.format.is_none() {
                    vs.format = DocValueFormat::Raw;
                }
            }
            SourceKind::GeoTile { precision, bounds } => {
                vs.format = DocValueFormat::GeoTile;
                tile_bounds = bounds.map(|b| crate::geo::TileBounds::new(*precision, b.top, b.left, b.bottom, b.right));
            }
            _ => {}
        }
        let bytes = matches!(s.kind, SourceKind::Terms) && matches!(vs.vs_type, VsType::Keyword | VsType::Ip);
        let double = match &s.kind {
            SourceKind::Histogram { .. } => true,
            SourceKind::Terms => !bytes && !vs.is_integral() && vs.vs_type == VsType::Numeric,
            _ => false,
        };
        sources.push(BoundSource { spec: s.clone(), vs, double, bytes, rounding, tile_bounds, reader: SourceReader::None, vals: Vec::new(), arena: Vec::new() });
    }
    let after = match &spec.after {
        None => None,
        Some(a) => {
            let obj = a.as_object().unwrap_or(&[]);
            if obj.len() != sources.len() {
                return Err(AggError::illegal_argument(format!("[after] has {} value(s) but [sources] has {}", obj.len(), sources.len())));
            }
            let mut key = CompositeKey::new();
            for s in &sources {
                let Some(v) = a.get(&s.spec.name) else {
                    return Err(AggError::illegal_argument(format!("Missing value for [after.{}]", s.spec.name)));
                };
                key.push(s.after_part(v, now)?);
            }
            Some(key)
        }
    };
    let orders: Vec<PartOrder> =
        sources.iter().map(|s| PartOrder { reverse_mul: if s.spec.desc { -1 } else { 1 }, missing: s.spec.missing_order }).collect();
    // `SizedBucketAggregator`: `rate` sub-aggregations divide by the size of the bucket of the one
    // `date_histogram` source.
    let date_sources: Vec<usize> = (0..sources.len()).filter(|&i| matches!(sources[i].spec.kind, SourceKind::DateHistogram { .. })).collect();
    let rounding = if date_sources.len() == 1 { sources[date_sources[0]].rounding.clone() } else { None };
    let sized = SizedAncestor::new(ctx.parents.len(), &def.name, rounding);
    let rate_starts = def.subs.iter().any(|d| d.type_name == "rate").then(|| (sized.starts.clone(), date_sources.first().copied().unwrap_or(0)));
    let saved = ctx.sized.replace(sized);
    let subs = Subs::bind(def, ctx);
    ctx.sized = saved;
    let subs = subs?;
    Ok(Box::new(CompositeAgg {
        rate_starts,
        names: Arc::new(spec.sources.iter().map(|s| s.name.clone()).collect()),
        formats: Arc::new(sources.iter().map(|s| s.vs.format.clone()).collect()),
        orders: Arc::new(orders),
        size: spec.size,
        after,
        sources,
        queue: Queue::default(),
        segments: Vec::new(),
        cur_seg: None,
        subs,
        searcher: ctx.searcher.clone(),
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
        replayed: false,
        idx: Vec::new(),
        ord_bound: None,
    }))
}

/// The bounded key queue (`CompositeValuesCollectorQueue`): slots with keys and counts, a map
/// from key to slot and a heap with the largest key on top.
#[derive(Default)]
struct Queue {
    keys: Vec<CompositeKey>,
    counts: Vec<u64>,
    /// Key hash of each slot.
    hashes: Vec<u64>,
    /// Slots by key hash.
    map: hashbrown::HashTable<u32>,
    heap: Vec<u32>,
    /// Bumped whenever the heap changes (cached bounds derived from the top key go stale).
    version: u64,
}

impl Queue {
    fn greater(&self, a: u32, b: u32, orders: &[PartOrder]) -> bool {
        compare_keys(&self.keys[a as usize], &self.keys[b as usize], orders) == Ordering::Greater
    }

    fn sift_up(&mut self, mut i: usize, orders: &[PartOrder]) {
        while i > 0 {
            let p = (i - 1) / 2;
            if self.greater(self.heap[i], self.heap[p], orders) {
                self.heap.swap(i, p);
                i = p;
            } else {
                break;
            }
        }
    }

    fn sift_down(&mut self, mut i: usize, orders: &[PartOrder]) {
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < self.heap.len() && self.greater(self.heap[l], self.heap[m], orders) {
                m = l;
            }
            if r < self.heap.len() && self.greater(self.heap[r], self.heap[m], orders) {
                m = r;
            }
            if m == i {
                break;
            }
            self.heap.swap(i, m);
            i = m;
        }
    }
}

/// Docs of one segment kept for the sub-aggregation pass.
struct SegDocs {
    ord: usize,
    docs: Vec<u32>,
    scores: Vec<f32>,
}

struct CompositeAgg {
    names: Arc<Vec<String>>,
    formats: Arc<Vec<DocValueFormat>>,
    orders: Arc<Vec<PartOrder>>,
    size: i32,
    after: Option<CompositeKey>,
    sources: Vec<BoundSource>,
    queue: Queue,
    segments: Vec<SegDocs>,
    cur_seg: Option<usize>,
    subs: Subs,
    searcher: Searcher,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    tracker: MemTracker,
    replayed: bool,
    /// Bucket starts published to direct `rate` sub-aggregations, and the date source's index.
    rate_starts: Option<(crate::collect::BucketStarts, usize)>,
    /// The value index of each source in the key being visited.
    idx: Vec<usize>,
    /// For a leading ordinals source: the segment ordinal bound a doc's ordinals must reach to
    /// compete with the full queue's largest key, and the queue version it was computed at.
    ord_bound: Option<(u64, Option<u64>)>,
}

impl CompositeAgg {
    fn open_sources(&mut self, seg: &SegmentContext) -> Result<()> {
        for s in &mut self.sources {
            s.reader = if s.vs.unmapped && s.vs.missing.is_none() {
                SourceReader::None
            } else {
                match &s.spec.kind {
                    SourceKind::GeoTile { .. } => SourceReader::Geo(s.vs.geo_reader(seg)?),
                    SourceKind::Terms if s.bytes && s.vs.has_ordinals() && s.vs.missing.is_none() => {
                        let dv = crate::values::DvOrds::open(seg, s.vs.field.as_ref().map(|f| f.number).unwrap_or(0));
                        let n = dv.value_count() as usize;
                        SourceReader::Ords { dv, spans: vec![(0, u32::MAX); n], terms: Vec::new() }
                    }
                    SourceKind::Terms if s.bytes => SourceReader::Bytes(s.vs.bytes_reader(seg)?),
                    _ => SourceReader::Num(s.vs.numeric_reader(seg)?),
                }
            };
        }
        Ok(())
    }

    /// Loads every source's values of `doc`; `false` when some source has none.
    fn load(&mut self, doc: u32, score: f32) -> Result<bool> {
        for s in &mut self.sources {
            s.load(doc, score)?;
            if s.vals.is_empty() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Loads the doc's values for collection: `false` when some source has none, or when the
    /// queue is full and even the doc's best leading value sorts after the queue's largest
    /// key (no key of the doc can compete, so the other sources are not read).
    fn load_competitive(&mut self, doc: u32, score: f32) -> Result<bool> {
        let full = self.size > 0 && self.queue.heap.len() >= self.size as usize;
        if full && !self.leading_ords_compete(doc) {
            return Ok(false);
        }
        let Some(first) = self.sources.first_mut() else { return Ok(false) };
        first.load(doc, score)?;
        if first.vals.is_empty() {
            return Ok(false);
        }
        if full {
            let o = &self.orders[0];
            let first = &self.sources[0];
            let best = if o.reverse_mul > 0 { first.vals[0] } else { first.vals[first.vals.len() - 1] };
            let top = &self.queue.keys[self.queue.heap[0] as usize];
            if first.view(best).compare(top[0].view(), o) > 0 {
                // Missing values may sort first (`missing_order`): only skip when the best
                // value can't be beaten by any other of the doc's values.
                if first.vals.iter().all(|&v| first.view(v).compare(top[0].view(), o) > 0) {
                    return Ok(false);
                }
            }
        }
        for s in &mut self.sources[1..] {
            s.load(doc, score)?;
            if s.vals.is_empty() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// With a full queue and a leading keyword source read by ordinals: whether the doc's
    /// ordinals can reach the queue's largest key, compared as segment ordinals (the top key's
    /// term is looked up in the segment dictionary once per queue change), without reading
    /// any term. `true` when the check doesn't apply.
    fn leading_ords_compete(&mut self, doc: u32) -> bool {
        let o = self.orders[0];
        let SourceReader::Ords { dv, .. } = &self.sources[0].reader else { return true };
        let version = self.queue.version;
        let bound = match self.ord_bound {
            Some((v, b)) if v == version => b,
            _ => {
                let b = match &self.queue.keys[self.queue.heap[0] as usize][0] {
                    KeyPart::Bytes(t) => match dv {
                        crate::values::DvOrds::Single(d) => Some(d.lookup_term(t)),
                        crate::values::DvOrds::Multi(d) => Some(d.lookup_term(t)),
                        crate::values::DvOrds::None => None,
                    }
                    .map(|r| match r {
                        // Ascending: the doc needs an ordinal <= the bound; descending: >=.
                        Ok(ord) => ord,
                        Err(ip) if o.reverse_mul > 0 => ip.wrapping_sub(1),
                        Err(ip) => ip,
                    }),
                    _ => None,
                };
                self.ord_bound = Some((version, b));
                b
            }
        };
        let Some(bound) = bound else { return true };
        let (mut min, mut max, mut any) = (u64::MAX, 0u64, false);
        dv.for_each(doc, |ord| {
            any = true;
            min = min.min(ord);
            max = max.max(ord);
        });
        if !any {
            // Missing value: the full load decides (missing_bucket ordering).
            return true;
        }
        if o.reverse_mul > 0 { bound != u64::MAX && min <= bound } else { max >= bound }
    }

    /// The parts of the key at `idx` (one value per source).
    #[inline]
    fn parts<'a>(&'a self, idx: &'a [usize]) -> impl Iterator<Item = PartView<'a>> + 'a {
        self.sources.iter().zip(idx).map(|(s, &i)| s.view(s.vals[i]))
    }

    /// Advances `idx` to the next key of the loaded document (cartesian product, the last
    /// source fastest); `false` after the last one.
    fn next_key(&self, idx: &mut [usize]) -> bool {
        let mut d = idx.len();
        while d > 0 {
            d -= 1;
            idx[d] += 1;
            if idx[d] < self.sources[d].vals.len() {
                return true;
            }
            idx[d] = 0;
        }
        false
    }

    /// The queue slot of the key at `idx`, if queued.
    fn find_slot(&self, idx: &[usize], hash: u64) -> Option<u32> {
        let keys = &self.queue.keys;
        self.queue
            .map
            .find(hash, |&slot| keys[slot as usize].iter().zip(self.parts(idx)).all(|(k, v)| k.view().same(v)))
            .copied()
    }

    /// `addIfCompetitive` for the key at `idx`.
    fn add(&mut self, idx: &[usize], inc: u64) -> Result<()> {
        let hash = hash_key(self.parts(idx));
        if let Some(slot) = self.find_slot(idx, hash) {
            self.queue.counts[slot as usize] += inc;
            return Ok(());
        }
        if let Some(a) = &self.after
            && compare_views(self.parts(idx), a.iter().map(KeyPart::view), &self.orders) != Ordering::Greater
        {
            return Ok(());
        }
        let max = self.size.max(0) as usize;
        let orders = self.orders.clone();
        if self.queue.heap.len() >= max {
            let Some(&top) = self.queue.heap.first() else {
                return Err(AggError::new(
                    500,
                    "null_pointer_exception",
                    "Cannot invoke \"java.lang.Integer.intValue()\" because the return value of \"org.elasticsearch.search.aggregations.bucket.composite.CompositeValuesCollectorQueue.top()\" is null",
                ));
            };
            if compare_views(self.parts(idx), self.queue.keys[top as usize].iter().map(KeyPart::view), &orders) == Ordering::Greater {
                return Ok(());
            }
            // Evict the largest key and recycle its slot.
            let old_hash = self.queue.hashes[top as usize];
            if let Ok(e) = self.queue.map.find_entry(old_hash, |&s| s == top) {
                e.remove();
            }
            let key: CompositeKey = self.parts(idx).map(PartView::to_owned_part).collect();
            self.queue.keys[top as usize] = key;
            self.queue.counts[top as usize] = inc;
            self.queue.hashes[top as usize] = hash;
            let hashes = &self.queue.hashes;
            self.queue.map.insert_unique(hash, top, |&s| hashes[s as usize]);
            self.queue.sift_down(0, &orders);
            self.queue.version += 1;
            return Ok(());
        }
        let slot = self.queue.keys.len() as u32;
        let key: CompositeKey = self.parts(idx).map(PartView::to_owned_part).collect();
        let key_len = key.len();
        self.queue.keys.push(key);
        self.queue.counts.push(inc);
        self.queue.hashes.push(hash);
        let hashes = &self.queue.hashes;
        self.queue.map.insert_unique(hash, slot, |&s| hashes[s as usize]);
        self.queue.heap.push(slot);
        let n = self.queue.heap.len() - 1;
        self.queue.sift_up(n, &orders);
        self.queue.version += 1;
        self.tracker.ensure(self.queue.keys.len() * (72 + key_len * 24))?;
        Ok(())
    }

    /// The queued slots, smallest key first.
    fn sorted_slots(&self) -> Vec<u32> {
        let mut slots: Vec<u32> = self.queue.heap.clone();
        slots.sort_by(|&a, &b| compare_keys(&self.queue.keys[a as usize], &self.queue.keys[b as usize], &self.orders));
        slots
    }

    /// Replays the recorded docs into the sub-aggregations, bucket ordinal = queue slot.
    fn replay(&mut self) -> Result<()> {
        let segs: Vec<SegmentContext> = self.searcher.segments().to_vec();
        let recorded = std::mem::take(&mut self.segments);
        for sd in &recorded {
            let seg = &segs[sd.ord];
            self.open_sources(seg)?;
            self.subs.set_segment(seg)?;
            for (i, &doc) in sd.docs.iter().enumerate() {
                let score = sd.scores.get(i).copied().unwrap_or(f32::NAN);
                if !self.load(doc, score)? {
                    continue;
                }
                let mut slots: SmallVec<[u32; 4]> = SmallVec::new();
                let mut idx = std::mem::take(&mut self.idx);
                idx.clear();
                idx.resize(self.sources.len(), 0);
                loop {
                    let hash = hash_key(self.parts(&idx));
                    if let Some(s) = self.find_slot(&idx, hash) {
                        slots.push(s);
                    }
                    if !self.next_key(&mut idx) {
                        break;
                    }
                }
                self.idx = idx;
                for s in slots {
                    self.subs.push(doc, s, score)?;
                }
            }
            self.subs.finish_segment()?;
        }
        self.segments = recorded;
        Ok(())
    }
}

impl Aggregator for CompositeAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.open_sources(seg)?;
        self.ord_bound = None;
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        if !self.subs.is_empty() {
            let ord = self.searcher.segments().iter().position(|s| s.base == seg.base).unwrap_or(0);
            if self.segments.last().is_none_or(|s| s.ord != ord) {
                self.segments.push(SegDocs { ord, docs: Vec::new(), scores: Vec::new() });
            }
            self.cur_seg = Some(self.segments.len() - 1);
        }
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let needs_scores = self.subs.needs_scores();
        for i in 0..b.len() {
            let doc = b.docs[i];
            let score = b.score(i);
            if let Some(s) = self.cur_seg {
                let sd = &mut self.segments[s];
                sd.docs.push(doc);
                if needs_scores {
                    sd.scores.push(score);
                }
            }
            if !self.load_competitive(doc, score)? {
                continue;
            }
            let inc = self.doc_counter.count(doc);
            let mut idx = std::mem::take(&mut self.idx);
            idx.clear();
            idx.resize(self.sources.len(), 0);
            let r: Result<()> = (|| {
                loop {
                    self.add(&idx, inc)?;
                    if !self.next_key(&mut idx) {
                        return Ok(());
                    }
                }
            })();
            self.idx = idx;
            r?;
        }
        Ok(())
    }

    fn post_collection(&mut self) -> Result<()> {
        if !self.replayed && !self.subs.is_empty() {
            self.replayed = true;
            self.replay()?;
        }
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let slots = self.sorted_slots();
        if let Some((starts, src)) = &self.rate_starts {
            let keys = &self.queue.keys;
            SizedAncestor::publish(
                starts,
                slots.iter().map(|&s| (s, match keys[s as usize].get(*src) {
                    Some(KeyPart::Long(v)) => Some(*v),
                    _ => None,
                })),
            );
        }
        let aggs = self.subs.build(&slots)?;
        let buckets: Vec<CompositeBucket> = slots
            .iter()
            .zip(aggs)
            .map(|(&s, aggs)| CompositeBucket { key: self.queue.keys[s as usize].clone(), doc_count: self.queue.counts[s as usize], aggs })
            .collect();
        let value = Composite {
            size: self.size.max(0) as usize,
            names: self.names.clone(),
            formats: self.formats.clone(),
            orders: self.orders.clone(),
            after_key: buckets.last().map(|b| b.key.clone()),
            buckets,
        };
        Ok(owning.iter().enumerate().map(|(i, _)| if i == 0 { AggValue::Composite(Box::new(value.clone())) } else { self.build_empty() }).collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::Composite(Box::new(Composite {
            size: self.size.max(0) as usize,
            names: self.names.clone(),
            formats: self.formats.clone(),
            orders: self.orders.clone(),
            buckets: Vec::new(),
            after_key: None,
        }))
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores() || self.sources.iter().any(|s| s.vs.script.as_ref().is_some_and(|x| x.needs_score()))
    }

    fn validate_sort(&self, _name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.",
        ))
    }
}
