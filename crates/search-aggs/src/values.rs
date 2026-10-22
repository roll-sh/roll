//! Values sources: the `field` / `script` / `missing` / `value_type` / `format` part of an
//! aggregation (Elasticsearch's `ValuesSourceConfig`), parsed once and bound per shard, and the
//! per-segment readers aggregators pull values from.
//!
//! Readers decode doc values straight into reusable buffers: numeric readers yield doubles or
//! longs (Elasticsearch's `SortedNumericDoubleValues` / `SortedNumericLongValues`, ascending),
//! byte readers yield segment ordinals with term lookup (`SortedSetDocValues`), geo readers
//! decode `LatLonDocValuesField` points. Scripts and value scripts run through
//! [`LeafValuesScript`]; `missing` substitutes a value for docs without one. Nothing allocates
//! per document once buffers have grown. Hot aggregators decode a whole batch of docs at once
//! ([`DvNum::load_batch`], [`DvOrds::load_batch`] into a [`DvBatch`]): one dispatch on the
//! value encoding per batch and no per-doc rank lookups on dense fields.

use std::sync::Arc;

use search_codec::{DocValuesReader, NumericDocValues, SortedDocValues, SortedNumericDocValues, SortedSetDocValues};
use search_codec::points::encode;
use search_index::SegmentContext;

use crate::error::{AggError, Result};
use crate::field::{AggField, FieldLookup, NumericType, SourceType};
use crate::format::{DocValueFormat, TimeZone, parse_ip};
use crate::json::Json;
use crate::script::{LeafValuesScript, Script, ScriptValue, ValuesScript};
use crate::xparser::{Field, Token, Tokens, XParser};

/// `value_type` (Elasticsearch's `ValueType.lenientParse`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    String,
    Long,
    Double,
    Date,
    Ip,
    Boolean,
}

impl ValueType {
    pub fn lenient_parse(s: &str) -> Option<ValueType> {
        Some(match s {
            "string" => ValueType::String,
            "double" | "float" => ValueType::Double,
            "number" | "numeric" | "long" | "integer" | "short" | "byte" => ValueType::Long,
            "date" => ValueType::Date,
            "ip" => ValueType::Ip,
            "boolean" => ValueType::Boolean,
            _ => return None,
        })
    }

    /// `getPreferredName()`.
    pub fn preferred_name(self) -> &'static str {
        match self {
            ValueType::String => "string",
            ValueType::Long => "long",
            ValueType::Double => "double",
            ValueType::Date => "date",
            ValueType::Ip => "ip",
            ValueType::Boolean => "boolean",
        }
    }

    /// The values source type it implies.
    pub fn source(self) -> VsType {
        match self {
            ValueType::String => VsType::Keyword,
            ValueType::Long | ValueType::Double => VsType::Numeric,
            ValueType::Date => VsType::Date,
            ValueType::Ip => VsType::Ip,
            ValueType::Boolean => VsType::Boolean,
        }
    }
}

/// Elasticsearch's `CoreValuesSourceType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VsType {
    Numeric,
    Keyword,
    GeoPoint,
    CartesianPoint,
    /// `geo_shape` fields.
    GeoShape,
    /// `shape` fields.
    CartesianShape,
    /// Pre-aggregated `histogram` fields.
    Histogram,
    /// `aggregate_metric_double` fields.
    AggregateMetric,
    /// `exponential_histogram` fields.
    ExponentialHistogram,
    Range,
    Ip,
    Date,
    Boolean,
}

impl VsType {
    pub fn type_name(self) -> &'static str {
        match self {
            VsType::Numeric => "numeric",
            VsType::Keyword => "keyword",
            VsType::GeoPoint => "geopoint",
            VsType::CartesianPoint => "point",
            VsType::GeoShape => "geo_shape",
            VsType::CartesianShape => "shape",
            VsType::Histogram => "histogram",
            VsType::AggregateMetric => "aggregate_metric",
            VsType::ExponentialHistogram => "exponential_histogram",
            VsType::Range => "range",
            VsType::Ip => "ip",
            VsType::Date => "date",
            VsType::Boolean => "boolean",
        }
    }

    /// Numeric-like (doc values are longs or doubles).
    pub fn is_numeric(self) -> bool {
        matches!(self, VsType::Numeric | VsType::Date | VsType::Boolean)
    }

    fn of_field(s: SourceType) -> Option<VsType> {
        Some(match s {
            SourceType::Numeric(_) => VsType::Numeric,
            SourceType::Date { .. } => VsType::Date,
            SourceType::Boolean => VsType::Boolean,
            SourceType::Keyword => VsType::Keyword,
            SourceType::Ip => VsType::Ip,
            SourceType::GeoPoint => VsType::GeoPoint,
            SourceType::CartesianPoint => VsType::CartesianPoint,
            SourceType::GeoShape { .. } => VsType::GeoShape,
            SourceType::CartesianShape { .. } => VsType::CartesianShape,
            SourceType::Histogram => VsType::Histogram,
            SourceType::AggregateMetric(_) => VsType::AggregateMetric,
            SourceType::ExponentialHistogram => VsType::ExponentialHistogram,
            SourceType::Range(_) => VsType::Range,
            SourceType::None => return None,
        })
    }
}

/// Which values-source fields an aggregation declares.
#[derive(Clone, Copy, Debug)]
pub struct VsOpts {
    pub scriptable: bool,
    pub formattable: bool,
    pub timezone: bool,
    pub field_required: bool,
}

impl VsOpts {
    pub const fn new(scriptable: bool, formattable: bool, timezone: bool) -> VsOpts {
        VsOpts { scriptable, formattable, timezone, field_required: true }
    }
}

/// The parsed values-source part of an aggregation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VsSpec {
    pub field: Option<String>,
    pub script: Option<Script>,
    pub missing: Option<Json>,
    pub value_type: Option<ValueType>,
    pub format: Option<String>,
    pub time_zone: Option<TimeZone>,
}

impl VsSpec {
    /// `ValuesSourceAggregationBuilder.declareFields`.
    pub fn fields(o: VsOpts) -> Vec<Field> {
        let mut f = vec![
            Field::new("field", Tokens::STRING),
            Field::new("missing", Tokens::VALUE),
            Field::with_deprecated("value_type", &["valueType"], Tokens::STRING),
        ];
        if o.formattable {
            f.push(Field::new("format", Tokens::STRING));
        }
        if o.scriptable {
            f.push(Field::new("script", Tokens::OBJECT_OR_STRING));
        }
        if o.timezone {
            f.push(Field::new("time_zone", Tokens::LONG));
        }
        f
    }

    /// Parses one of the declared fields; `Ok(false)` when `name` is not a values-source field.
    pub fn parse_field(&mut self, p: &mut XParser<'_>, name: &str) -> Result<bool> {
        match name {
            "field" => self.field = Some(p.text()?),
            "missing" => self.missing = p.object_text()?,
            "value_type" | "valueType" => {
                let t = p.text()?;
                self.value_type = Some(ValueType::lenient_parse(&t).ok_or_else(|| AggError::illegal_argument(format!("Unknown value type [{t}]")))?);
            }
            "format" => self.format = Some(p.text()?),
            "script" => self.script = Some(Script::parse(p)?),
            "time_zone" => {
                self.time_zone = Some(if p.token() == Token::String { TimeZone::parse(&p.text()?)? } else { TimeZone::of_hours(p.int_value()? as i64)? })
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// `declareRequiredFieldSet(field, script)`.
    pub fn check_required(&self, o: VsOpts) -> Result<()> {
        if !o.field_required {
            return Ok(());
        }
        if o.scriptable {
            if self.field.is_none() && self.script.is_none() {
                return Err(AggError::illegal_argument("Required one of fields [field, script], but none were specified."));
            }
        } else if self.field.is_none() {
            return Err(AggError::illegal_argument("Required one of fields [field], but none were specified."));
        }
        Ok(())
    }

    /// Fields written back (`toXContent`) for definitions echoed in responses.
    pub fn to_json(&self, out: &mut Json) {
        if let Some(f) = &self.field {
            out.push("field", f.as_str());
        }
        if let Some(s) = &self.script {
            out.push("script", s.to_json());
        }
        if let Some(m) = &self.missing {
            out.push("missing", m.clone());
        }
        if let Some(v) = &self.value_type {
            out.push("value_type", v.preferred_name());
        }
        if let Some(f) = &self.format {
            out.push("format", f.as_str());
        }
    }
}

/// A substitute for docs without values.
#[derive(Clone, Debug, PartialEq)]
pub enum MissingValue {
    Double(f64),
    Long(i64),
    Bytes(Vec<u8>),
    Geo(f64, f64),
    /// A cartesian point, `XYDocValuesField`-encoded.
    Xy(i64),
    /// A shape: its doc value and centroid (`ShapeValues.missing`).
    Shape(Vec<u8>, [u8; 17]),
}

/// A values source bound to a shard.
#[derive(Clone)]
pub struct BoundVs {
    pub vs_type: VsType,
    pub field: Option<AggField>,
    /// A script (no field) or a value script (with a field).
    pub script: Option<Arc<dyn ValuesScript>>,
    pub script_value_type: Option<ValueType>,
    pub unmapped: bool,
    pub missing: Option<MissingValue>,
    pub format: DocValueFormat,
    pub time_zone: Option<TimeZone>,
}

impl std::fmt::Debug for BoundVs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundVs").field("vs_type", &self.vs_type).field("field", &self.field).field("unmapped", &self.unmapped).finish()
    }
}

impl BoundVs {
    /// `ValuesSourceConfig.getDescription()`.
    pub fn description(&self) -> String {
        if self.script.is_some() && self.field.is_none() {
            return format!("Script yielding [{}]", self.script_value_type.map(|v| v.preferred_name()).unwrap_or("unknown type"));
        }
        match &self.field {
            Some(f) if f.counter => format!("Field [{}] of type [{}][counter]", f.name, f.type_name),
            Some(f) => format!("Field [{}] of type [{}]", f.name, f.type_name),
            None => format!("unmapped field with value source type [{}]", self.vs_type.type_name()),
        }
    }

    /// A counter field (`TimeSeriesValuesSourceType.COUNTER`).
    pub fn is_counter(&self) -> bool {
        self.field.as_ref().is_some_and(|f| f.counter)
    }

    /// Whether the source can produce values (`hasValues`).
    pub fn has_values(&self) -> bool {
        self.field.is_some() || self.script.is_some() || self.missing.is_some()
    }

    /// The error for a values-source type an aggregation doesn't support.
    pub fn unsupported(&self, agg_type: &str) -> AggError {
        let msg = format!("{} is not supported for aggregation [{agg_type}]", self.description());
        if matches!(self.vs_type, VsType::AggregateMetric | VsType::ExponentialHistogram) && !self.unmapped {
            return AggError::new(400, "unsupported_aggregation_on_downsampled_index", msg);
        }
        AggError::illegal_argument(msg)
    }

    /// The `aggregate_metric_double` sub-fields of a mapped field.
    pub fn aggregate_metric(&self) -> Option<crate::histogram_field::AggregateMetricFields> {
        match self.field.as_ref().map(|f| f.source) {
            Some(SourceType::AggregateMetric(m)) => Some(m),
            _ => None,
        }
    }

    /// Whether numeric values are integers (longs).
    pub fn is_integral(&self) -> bool {
        if self.script.is_some() && self.field.is_none() {
            return matches!(self.script_value_type, Some(ValueType::Long | ValueType::Date | ValueType::Boolean));
        }
        match self.field.as_ref().map(|f| f.source) {
            Some(SourceType::Numeric(t)) => t.is_integral() && self.script.is_none(),
            Some(SourceType::Date { .. }) | Some(SourceType::Boolean) => self.script.is_none(),
            _ => self.vs_type != VsType::Numeric,
        }
    }

    /// Whether the doc values are ordinals of a terms dictionary.
    /// The range type of a mapped range field.
    pub fn range_type(&self) -> Option<crate::range_field::RangeType> {
        match self.field.as_ref().map(|f| f.source) {
            Some(SourceType::Range(t)) => Some(t),
            _ => None,
        }
    }

    pub fn has_ordinals(&self) -> bool {
        self.script.is_none() && matches!(self.field.as_ref().map(|f| f.source), Some(SourceType::Keyword | SourceType::Ip))
    }

    pub fn numeric_reader(&self, seg: &SegmentContext) -> Result<NumReader> {
        NumReader::open(self, seg)
    }

    pub fn bytes_reader(&self, seg: &SegmentContext) -> Result<BytesReader> {
        BytesReader::open(self, seg)
    }

    pub fn geo_reader(&self, seg: &SegmentContext) -> Result<GeoReader> {
        GeoReader::open(self, seg)
    }
}

/// Resolves a values source spec against an index (`ValuesSourceConfig.resolve`).
pub fn bind(spec: &VsSpec, lookup: &dyn FieldLookup, default_type: VsType, timezone_aware: bool) -> Result<BoundVs> {
    let mut vs_type = spec.value_type.map(|v| v.source());
    let mut field: Option<AggField> = None;
    let mut unmapped = false;
    let mut script: Option<Arc<dyn ValuesScript>> = None;
    if let Some(s) = &spec.script {
        let host = lookup.scripts().ok_or_else(|| AggError::illegal_argument("scripts are not supported"))?;
        script = Some(host.values(s)?);
    }
    match &spec.field {
        None => {
            if spec.script.is_none() {
                return Err(AggError::illegal_state("value source config is invalid; must have either a field or a script"));
            }
        }
        Some(name) => match lookup.field(name)? {
            None => {
                unmapped = true;
                script = None;
            }
            Some(f) => {
                let ft = VsType::of_field(f.source);
                match (vs_type, ft) {
                    (None, Some(t)) => vs_type = Some(t),
                    (Some(user), Some(t)) if user != t && spec.script.is_none() => {
                        // Numeric-compatible hints are accepted (ValueType.isA compatibility).
                        let compatible = (user.is_numeric() && t.is_numeric()) || (matches!(user, VsType::Keyword | VsType::Ip) && matches!(t, VsType::Keyword | VsType::Ip));
                        if !compatible {
                            return Err(AggError::illegal_argument(format!(
                                "Field type [{}] is incompatible with specified value_type [{}]",
                                f.type_name,
                                spec.value_type.map(|v| v.preferred_name()).unwrap_or("")
                            )));
                        }
                        vs_type = Some(t);
                    }
                    (_, None) => {
                        return Err(AggError::illegal_argument(format!("Field [{}] of type [{}] is not supported for aggregation", f.name, f.type_name)));
                    }
                    _ => {}
                }
                // `CoreValuesSourceType.RANGE.getField` ignores the script.
                if matches!(f.source, SourceType::Range(_)) {
                    script = None;
                }
                field = Some(f);
            }
        },
    }
    let vs_type = vs_type.unwrap_or(default_type);
    let tz = if timezone_aware { spec.time_zone.clone() } else { None };
    let format = resolve_format(spec.format.as_deref(), vs_type, tz.as_ref(), field.as_ref(), lookup)?;
    let mut b = BoundVs { vs_type, field, script, script_value_type: spec.value_type, unmapped, missing: None, format, time_zone: tz };
    if let Some(m) = &spec.missing {
        b.missing = Some(parse_missing(&b, m, lookup.now_millis())?);
    }
    Ok(b)
}

/// `MappedFieldType.docValueFormat(format, tz)` / `ValuesSourceType.getFormatter`.
pub fn resolve_format(format: Option<&str>, vs_type: VsType, tz: Option<&TimeZone>, field: Option<&AggField>, lookup: &dyn FieldLookup) -> Result<DocValueFormat> {
    if let Some(f) = field {
        return field_format(f, format, tz, lookup);
    }
    Ok(match vs_type {
        VsType::Numeric | VsType::Range => match format {
            None => DocValueFormat::Raw,
            Some(p) => DocValueFormat::Decimal(lookup.decimal_format(p)?),
        },
        VsType::Keyword => DocValueFormat::Raw,
        VsType::Ip => DocValueFormat::Ip,
        VsType::GeoPoint | VsType::GeoShape => DocValueFormat::GeoHash,
        VsType::CartesianPoint | VsType::CartesianShape | VsType::Histogram | VsType::AggregateMetric | VsType::ExponentialHistogram => DocValueFormat::Raw,
        VsType::Boolean => DocValueFormat::Boolean,
        VsType::Date => DocValueFormat::DateTime {
            formatter: lookup.date_formatter(format.unwrap_or(crate::format::DEFAULT_DATE_FORMAT))?,
            zone: tz.cloned().unwrap_or_default(),
            nanos: false,
        },
    })
}

/// A mapped field's `docValueFormat(format, timeZone)`.
pub fn field_format(f: &AggField, format: Option<&str>, tz: Option<&TimeZone>, lookup: &dyn FieldLookup) -> Result<DocValueFormat> {
    Ok(match f.source {
        // Aggregations read `date_nanos` values as millis (Elasticsearch's `toMilliSeconds`).
        SourceType::Date { .. } | SourceType::Range(crate::range_field::RangeType::Date) => {
            let pattern = format.map(str::to_string).or_else(|| f.date_format.clone()).unwrap_or_else(|| crate::format::DEFAULT_DATE_FORMAT.to_string());
            DocValueFormat::DateTime { formatter: lookup.date_formatter(&pattern)?, zone: tz.cloned().unwrap_or_default(), nanos: false }
        }
        SourceType::Numeric(t) => {
            if tz.is_some() {
                return Err(AggError::illegal_argument(format!("Field [{}] of type [{}] does not support custom time zones", f.name, f.type_name)));
            }
            match format {
                Some(p) => DocValueFormat::Decimal(lookup.decimal_format(p)?),
                None if t == NumericType::UnsignedLong => DocValueFormat::UnsignedLongShifted,
                None => DocValueFormat::Raw,
            }
        }
        SourceType::Boolean => {
            check_no_format(f, format, tz)?;
            DocValueFormat::Boolean
        }
        SourceType::Ip => {
            check_no_format(f, format, tz)?;
            DocValueFormat::Ip
        }
        SourceType::GeoPoint => {
            check_no_format(f, format, tz)?;
            DocValueFormat::GeoHash
        }
        _ => {
            check_no_format(f, format, tz)?;
            DocValueFormat::Raw
        }
    })
}

fn check_no_format(f: &AggField, format: Option<&str>, tz: Option<&TimeZone>) -> Result<()> {
    if format.is_some() {
        return Err(AggError::illegal_argument(format!("Field [{}] of type [{}] does not support custom formats", f.name, f.type_name)));
    }
    if tz.is_some() {
        return Err(AggError::illegal_argument(format!("Field [{}] of type [{}] does not support custom time zones", f.name, f.type_name)));
    }
    Ok(())
}

/// `ValuesSourceType.replaceMissing`: parses `missing` for the values source.
pub fn parse_missing(vs: &BoundVs, m: &Json, now: i64) -> Result<MissingValue> {
    let text = match m {
        Json::Str(s) => s.clone(),
        other => other.to_java_string(),
    };
    Ok(match vs.vs_type {
        VsType::Numeric | VsType::Date | VsType::Boolean => {
            let v = match m {
                Json::Int(i) if vs.vs_type == VsType::Numeric => *i as f64,
                Json::Float(f) if vs.vs_type == VsType::Numeric => *f,
                Json::UInt(u) if vs.vs_type == VsType::Numeric => *u as f64,
                Json::Int(i) => *i as f64,
                Json::Float(f) => *f,
                _ => vs.format.parse_double(&text, false, now)?,
            };
            MissingValue::Double(v)
        }
        VsType::Keyword => MissingValue::Bytes(text.into_bytes()),
        VsType::Ip => MissingValue::Bytes(parse_ip(&text)?.to_vec()),
        VsType::GeoPoint => {
            let (lat, lon) = parse_geo_point_text(m)?;
            MissingValue::Geo(lat, lon)
        }
        VsType::CartesianPoint => {
            let (x, y) = crate::metrics::cartesian::parse_point_text(&text)?;
            MissingValue::Xy(crate::metrics::cartesian::encode_xy(x, y))
        }
        VsType::Range => {
            return Err(AggError::illegal_argument("Can't apply missing values on a class org.elasticsearch.search.aggregations.support.ValuesSource$Range"));
        }
        VsType::GeoShape | VsType::CartesianShape => crate::shape_field::parse_missing_shape(&text, vs.vs_type == VsType::GeoShape)?,
        VsType::Histogram => {
            return Err(AggError::illegal_argument(
                "Can't apply missing values on a class org.elasticsearch.xpack.analytics.aggregations.support.HistogramValuesSource$Histogram$Fielddata",
            ));
        }
        VsType::AggregateMetric => {
            return Err(AggError::illegal_argument(
                "Can't apply missing values on a class org.elasticsearch.xpack.aggregatemetric.aggregations.support.AggregateMetricsValuesSource$AggregateMetricDouble$Fielddata",
            ));
        }
        VsType::ExponentialHistogram => {
            return Err(AggError::illegal_argument(
                "Can't apply missing values on a class org.elasticsearch.xpack.analytics.aggregations.support.ExponentialHistogramValuesSource$ExponentialHistogram$Fielddata",
            ));
        }
    })
}

/// A geo point given as `"lat,lon"`, a geohash, `[lon, lat]` or `{lat, lon}`.
pub fn parse_geo_point_text(v: &Json) -> Result<(f64, f64)> {
    match v {
        Json::Str(s) => {
            if let Some((a, b)) = s.split_once(',') {
                let lat: f64 = a.trim().parse().map_err(|_| AggError::new(400, "parse_exception", "latitude must be a number"))?;
                let lon: f64 = b.trim().parse().map_err(|_| AggError::new(400, "parse_exception", "longitude must be a number"))?;
                Ok((lat, lon))
            } else {
                crate::geo::geohash_decode(s).ok_or_else(|| AggError::new(400, "parse_exception", format!("unsupported symbol [{s}] in geohash [{s}]")))
            }
        }
        Json::Arr(a) if a.len() >= 2 => Ok((a[1].as_f64().unwrap_or(f64::NAN), a[0].as_f64().unwrap_or(f64::NAN))),
        Json::Obj(_) => {
            let lat = v.get("lat").and_then(Json::as_f64).ok_or_else(|| AggError::new(400, "parse_exception", "field [lat] missing"))?;
            let lon = v.get("lon").and_then(Json::as_f64).ok_or_else(|| AggError::new(400, "parse_exception", "field [lon] missing"))?;
            Ok((lat, lon))
        }
        _ => Err(AggError::new(400, "parse_exception", format!("unsupported geo point [{v}]"))),
    }
}

/// Numeric doc values of one segment.
#[derive(Clone)]
pub enum DvNum {
    None,
    Single(NumericDocValues),
    Multi(SortedNumericDocValues),
}

impl DvNum {
    pub fn open(seg: &SegmentContext, field: u32) -> DvNum {
        match seg.reader.doc_values(field) {
            Some(DocValuesReader::Numeric(n)) => DvNum::Single(n.clone()),
            Some(DocValuesReader::SortedNumeric(s)) => DvNum::Multi(s.clone()),
            _ => DvNum::None,
        }
    }

    /// Calls `f` with each raw value of `doc` (ascending).
    #[inline]
    pub fn for_each(&self, doc: u32, mut f: impl FnMut(i64)) {
        match self {
            DvNum::None => {}
            DvNum::Single(n) => {
                if let Some(v) = n.get(doc) {
                    f(v)
                }
            }
            DvNum::Multi(s) => {
                for v in s.values(doc) {
                    f(v)
                }
            }
        }
    }

    /// Whether `doc` has a value.
    #[inline]
    pub fn has_value(&self, doc: u32) -> bool {
        match self {
            DvNum::None => false,
            DvNum::Single(n) => n.docs().contains(doc),
            DvNum::Multi(s) => s.docs().contains(doc),
        }
    }

    /// Whether every doc has at most one value.
    pub fn is_single_valued(&self) -> bool {
        match self {
            DvNum::None | DvNum::Single(_) => true,
            DvNum::Multi(s) => s.is_single_valued(),
        }
    }

    /// Decodes the values of every doc of `docs` (non-decreasing) at once into `b`: one
    /// dispatch on the encoding and no per-doc rank lookups on dense fields (see
    /// [`DvBatch::fill`]).
    pub fn load_batch(&self, docs: &[u32], b: &mut NumBatch) {
        match self {
            DvNum::None => b.clear(),
            DvNum::Single(n) => b.fill(docs, |d, found, counts, vals| {
                n.get_batch(d, found, vals);
                counts.resize(found.len(), 1);
            }),
            DvNum::Multi(m) => b.fill(docs, |d, found, counts, vals| m.get_batch(d, found, counts, vals)),
        }
    }
}

/// The doc values of a batch of docs decoded at once ([`DvNum::load_batch`],
/// [`DvOrds::load_batch`]).
#[derive(Clone, Debug, Default)]
pub struct DvBatch<T> {
    /// For each doc with values: its index in the batch.
    pub pos: Vec<u32>,
    /// Its value count.
    pub counts: Vec<u32>,
    /// The values (raw doc-values longs, or ordinals), doc after doc, ascending within a doc.
    pub vals: Vec<T>,
    found: Vec<u32>,
    unique: Vec<u32>,
    slots: Vec<(u32, u32)>,
    inner: Option<Box<DvBatch<T>>>,
}

/// Batch-decoded numeric doc values.
pub type NumBatch = DvBatch<i64>;
/// A codec batch call: `(docs, found docs, value counts, values)`.
type BatchLoad<'a, T> = dyn Fn(&[u32], &mut Vec<u32>, &mut Vec<u32>, &mut Vec<T>) + 'a;
/// Batch-decoded ordinals.
pub type OrdBatch = DvBatch<u64>;

impl<T: Copy + Default> DvBatch<T> {
    fn clear(&mut self) {
        self.pos.clear();
        self.counts.clear();
        self.vals.clear();
        self.found.clear();
    }

    /// Fills the batch for `docs` (non-decreasing) with `load`, which decodes strictly
    /// ascending docs into `(found docs, counts, values)`. A doc repeated in the batch
    /// (sub-aggregation batches repeat a doc per bucket; join and nested batches may be out of
    /// order) is decoded once and gets its values at each of its positions.
    pub fn fill(&mut self, docs: &[u32], load: impl Fn(&[u32], &mut Vec<u32>, &mut Vec<u32>, &mut Vec<T>)) {
        self.clear();
        if docs.windows(2).all(|w| w[0] < w[1]) {
            self.fill_strict(docs, &load);
            return;
        }
        self.unique.clear();
        self.unique.extend_from_slice(docs);
        self.unique.sort_unstable();
        self.unique.dedup();
        let mut u = self.inner.take().unwrap_or_default();
        u.clear();
        u.fill_strict(&self.unique, &load);
        // Distinct doc -> (offset, count) of its values.
        self.slots.clear();
        self.slots.resize(self.unique.len(), (0, 0));
        let mut off = 0u32;
        for (k, &p) in u.pos.iter().enumerate() {
            self.slots[p as usize] = (off, u.counts[k]);
            off += u.counts[k];
        }
        for (i, d) in docs.iter().enumerate() {
            let ui = self.unique.binary_search(d).unwrap_or(0);
            let (o, c) = self.slots[ui];
            if c > 0 {
                self.pos.push(i as u32);
                self.counts.push(c);
                self.vals.extend_from_slice(&u.vals[o as usize..(o + c) as usize]);
            }
        }
        self.inner = Some(u);
    }

    fn fill_strict(&mut self, docs: &[u32], load: &BatchLoad<'_, T>) {
        load(docs, &mut self.found, &mut self.counts, &mut self.vals);
        if self.found.len() == docs.len() {
            self.pos.extend(0..docs.len() as u32);
            return;
        }
        let mut j = 0usize;
        for &d in &self.found {
            while docs[j] != d {
                j += 1;
            }
            self.pos.push(j as u32);
            j += 1;
        }
    }

    /// `(batch index, values)` of each doc with values.
    #[inline]
    pub fn docs(&self) -> impl Iterator<Item = (usize, &[T])> + '_ {
        let mut off = 0usize;
        self.pos.iter().zip(&self.counts).map(move |(&p, &c)| {
            let v = &self.vals[off..off + c as usize];
            off += c as usize;
            (p as usize, v)
        })
    }

    /// Whether every doc of the batch has exactly one value.
    #[inline]
    pub fn all_single(&self) -> bool {
        self.vals.len() == self.pos.len()
    }
}

/// Decodes a raw doc value of a numeric field to a double.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decode {
    Long,
    /// `date_nanos`: aggregations see epoch millis (Elasticsearch's `toMilliSeconds`).
    Nanos,
    UnsignedLong,
    Double,
    Float,
    HalfFloat,
    Scaled(f64),
}

impl Decode {
    pub fn of(s: SourceType) -> Decode {
        match s {
            SourceType::Numeric(t) => match t {
                NumericType::Double => Decode::Double,
                NumericType::Float => Decode::Float,
                NumericType::HalfFloat => Decode::HalfFloat,
                NumericType::ScaledFloat(f) => Decode::Scaled(f),
                NumericType::UnsignedLong => Decode::UnsignedLong,
                _ => Decode::Long,
            },
            SourceType::Date { nanos: true } => Decode::Nanos,
            _ => Decode::Long,
        }
    }

    /// The value as a long (integral sources).
    #[inline]
    pub fn to_i64(self, v: i64) -> i64 {
        match self {
            Decode::Nanos => v.div_euclid(1_000_000),
            Decode::Long => v,
            other => other.to_f64(v) as i64,
        }
    }

    #[inline]
    pub fn to_f64(self, v: i64) -> f64 {
        match self {
            Decode::Long => v as f64,
            Decode::Nanos => v.div_euclid(1_000_000) as f64,
            Decode::UnsignedLong => ((v as u64) ^ (1u64 << 63)) as f64,
            Decode::Double => encode::sortable_f64(v),
            Decode::Float => encode::sortable_f32(v as i32) as f64,
            Decode::HalfFloat => encode::sortable_f16(v as i16) as f64,
            Decode::Scaled(f) => v as f64 / f,
        }
    }
}

/// Numeric values of one segment (`SortedNumericDoubleValues` / `SortedNumericLongValues`).
pub struct NumReader {
    dv: DvNum,
    decode: Decode,
    script: Option<Box<dyn LeafValuesScript>>,
    value_script: bool,
    missing: Option<f64>,
    dbuf: Vec<f64>,
    lbuf: Vec<i64>,
    sbuf: Vec<ScriptValue>,
    tmp: Vec<ScriptValue>,
}

impl NumReader {
    pub fn open(vs: &BoundVs, seg: &SegmentContext) -> Result<NumReader> {
        let (dv, decode) = match &vs.field {
            Some(f) => (DvNum::open(seg, f.number), Decode::of(f.source)),
            None => (DvNum::None, Decode::Long),
        };
        let script = match &vs.script {
            Some(s) => Some(s.leaf(seg)?),
            None => None,
        };
        let missing = match &vs.missing {
            Some(MissingValue::Double(d)) => Some(*d),
            Some(MissingValue::Long(l)) => Some(*l as f64),
            _ => None,
        };
        Ok(NumReader {
            dv,
            decode,
            value_script: script.is_some() && vs.field.is_some(),
            script,
            missing,
            dbuf: Vec::new(),
            lbuf: Vec::new(),
            sbuf: Vec::new(),
            tmp: Vec::new(),
        })
    }

    /// An empty reader (unmapped fields).
    pub fn empty(missing: Option<f64>) -> NumReader {
        NumReader { dv: DvNum::None, decode: Decode::Long, script: None, value_script: false, missing, dbuf: Vec::new(), lbuf: Vec::new(), sbuf: Vec::new(), tmp: Vec::new() }
    }

    /// The doc values, when there is no script and no `missing` (fast paths read them directly).
    pub fn raw(&self) -> Option<(&DvNum, Decode)> {
        if self.script.is_none() && self.missing.is_none() { Some((&self.dv, self.decode)) } else { None }
    }

    /// The values of `doc` as doubles, ascending.
    #[inline]
    pub fn doubles(&mut self, doc: u32, score: f32) -> Result<&[f64]> {
        self.dbuf.clear();
        match &mut self.script {
            None => {
                let decode = self.decode;
                let buf = &mut self.dbuf;
                self.dv.for_each(doc, |v| buf.push(decode.to_f64(v)));
            }
            Some(script) => {
                self.sbuf.clear();
                if self.value_script {
                    let decode = self.decode;
                    let mut vals: smallvec::SmallVec<[f64; 4]> = smallvec::SmallVec::new();
                    self.dv.for_each(doc, |v| vals.push(decode.to_f64(v)));
                    for v in vals {
                        self.tmp.clear();
                        let sv = if decode == Decode::Long { ScriptValue::Long(v as i64) } else { ScriptValue::Double(v) };
                        script.run(doc, score, Some(&sv), &mut self.tmp)?;
                        self.sbuf.append(&mut self.tmp);
                    }
                } else {
                    script.run(doc, score, None, &mut self.sbuf)?;
                }
                for v in &self.sbuf {
                    match v.as_f64() {
                        Some(d) => self.dbuf.push(d),
                        None => {
                            if matches!(v, ScriptValue::Null) {
                                continue;
                            }
                            return Err(AggError::illegal_argument(format!(
                                "Unsupported script value [{}], expected a number, date, or boolean",
                                v.to_java_string()
                            )));
                        }
                    }
                }
                if self.dbuf.len() > 1 {
                    self.dbuf.sort_by(|a, b| a.total_cmp(b));
                }
            }
        }
        if self.dbuf.is_empty()
            && let Some(m) = self.missing
        {
            self.dbuf.push(m);
        }
        Ok(&self.dbuf)
    }

    /// The values of `doc` as longs, ascending (integral sources).
    #[inline]
    pub fn longs(&mut self, doc: u32, score: f32) -> Result<&[i64]> {
        self.lbuf.clear();
        if self.script.is_none() && matches!(self.decode, Decode::Long | Decode::Nanos) {
            let buf = &mut self.lbuf;
            let decode = self.decode;
            self.dv.for_each(doc, |v| buf.push(decode.to_i64(v)));
            if self.lbuf.is_empty()
                && let Some(m) = self.missing
            {
                self.lbuf.push(m as i64);
            }
            return Ok(&self.lbuf);
        }
        let n = self.doubles(doc, score)?.len();
        for i in 0..n {
            let d = self.dbuf[i];
            self.lbuf.push(d as i64);
        }
        Ok(&self.lbuf)
    }
}

/// Ordinal doc values of one segment.
#[derive(Clone)]
pub enum DvOrds {
    None,
    Single(SortedDocValues),
    Multi(SortedSetDocValues),
}

impl DvOrds {
    pub fn open(seg: &SegmentContext, field: u32) -> DvOrds {
        match seg.reader.doc_values(field) {
            Some(DocValuesReader::Sorted(s)) => DvOrds::Single(s.clone()),
            Some(DocValuesReader::SortedSet(s)) => DvOrds::Multi(s.clone()),
            _ => DvOrds::None,
        }
    }

    #[inline]
    pub fn for_each(&self, doc: u32, mut f: impl FnMut(u64)) {
        match self {
            DvOrds::None => {}
            DvOrds::Single(s) => {
                if let Some(o) = s.ord(doc) {
                    f(o)
                }
            }
            DvOrds::Multi(s) => {
                for o in s.ords(doc) {
                    f(o)
                }
            }
        }
    }

    pub fn value_count(&self) -> u64 {
        match self {
            DvOrds::None => 0,
            DvOrds::Single(s) => s.value_count(),
            DvOrds::Multi(s) => s.value_count(),
        }
    }

    pub fn lookup(&self, ord: u64, out: &mut Vec<u8>) {
        out.clear();
        match self {
            DvOrds::None => {}
            DvOrds::Single(s) => s.lookup_ord(ord, out),
            DvOrds::Multi(s) => s.lookup_ord(ord, out),
        }
    }

    pub fn dict(&self) -> Option<&search_codec::TermDict> {
        match self {
            DvOrds::None => None,
            DvOrds::Single(s) => Some(s.dict()),
            DvOrds::Multi(s) => Some(s.dict()),
        }
    }

    pub fn has_value(&self, doc: u32) -> bool {
        match self {
            DvOrds::None => false,
            DvOrds::Single(s) => s.docs().contains(doc),
            DvOrds::Multi(s) => s.docs().contains(doc),
        }
    }

    /// The ordinals of every doc of `docs` (non-decreasing) at once ([`DvBatch::fill`]).
    pub fn load_batch(&self, docs: &[u32], b: &mut OrdBatch) {
        match self {
            DvOrds::None => b.clear(),
            DvOrds::Single(s) => b.fill(docs, |d, found, counts, vals| {
                s.ords_batch(d, found, vals);
                counts.resize(found.len(), 1);
            }),
            DvOrds::Multi(m) => b.fill(docs, |d, found, counts, vals| m.ords_batch(d, found, counts, vals)),
        }
    }
}

/// Byte-string values of one segment (`SortedBinaryDocValues`): terms of ordinal fields, or a
/// script's strings.
pub struct BytesReader {
    pub ords: DvOrds,
    /// Binary doc values (range fields: one encoded value per doc).
    binary: Option<Box<search_codec::BinaryDocValues>>,
    script: Option<Box<dyn LeafValuesScript>>,
    value_script: bool,
    missing: Option<Vec<u8>>,
    /// Values of the current doc, flattened (`offsets[i]..offsets[i + 1]`).
    data: Vec<u8>,
    offsets: Vec<usize>,
    sbuf: Vec<ScriptValue>,
    tmp: Vec<ScriptValue>,
    term: Vec<u8>,
    ordbuf: Vec<u64>,
    /// How script results print (dates and doubles as Elasticsearch's `toString`).
    script_format: DocValueFormat,
}

impl BytesReader {
    pub fn open(vs: &BoundVs, seg: &SegmentContext) -> Result<BytesReader> {
        let ords = match &vs.field {
            Some(f) => DvOrds::open(seg, f.number),
            None => DvOrds::None,
        };
        let binary = match &vs.field {
            Some(f) if matches!(f.source, SourceType::Range(_)) => seg.reader.binary(f.number).cloned().map(Box::new),
            _ => None,
        };
        let script = match &vs.script {
            Some(s) => Some(s.leaf(seg)?),
            None => None,
        };
        let missing = match &vs.missing {
            Some(MissingValue::Bytes(b)) => Some(b.clone()),
            Some(MissingValue::Double(d)) => Some(crate::json::java_double(*d).into_bytes()),
            Some(MissingValue::Long(l)) => Some(l.to_string().into_bytes()),
            _ => None,
        };
        Ok(BytesReader {
            ords,
            binary,
            value_script: script.is_some() && vs.field.is_some(),
            script,
            missing,
            data: Vec::new(),
            offsets: Vec::new(),
            sbuf: Vec::new(),
            tmp: Vec::new(),
            term: Vec::new(),
            ordbuf: Vec::new(),
            script_format: vs.format.clone(),
        })
    }

    /// Whether values come straight from ordinals (no script, no missing).
    pub fn is_plain_ords(&self) -> bool {
        self.script.is_none() && self.missing.is_none()
    }

    /// Loads `doc`'s values; returns how many (read them with [`value`](Self::value)).
    pub fn load(&mut self, doc: u32, score: f32) -> Result<usize> {
        self.data.clear();
        self.offsets.clear();
        self.offsets.push(0);
        match &mut self.script {
            None if self.binary.is_some() => {
                if let Some(v) = self.binary.as_ref().and_then(|b| b.get(doc)) {
                    self.data.extend_from_slice(v);
                    self.offsets.push(self.data.len());
                }
            }
            None => {
                self.ordbuf.clear();
                let ob = &mut self.ordbuf;
                self.ords.for_each(doc, |o| ob.push(o));
                for i in 0..self.ordbuf.len() {
                    self.ords.lookup(self.ordbuf[i], &mut self.term);
                    self.data.extend_from_slice(&self.term);
                    self.offsets.push(self.data.len());
                }
            }
            Some(script) => {
                self.sbuf.clear();
                if self.value_script {
                    self.ordbuf.clear();
                    let ob = &mut self.ordbuf;
                    self.ords.for_each(doc, |o| ob.push(o));
                    for i in 0..self.ordbuf.len() {
                        self.ords.lookup(self.ordbuf[i], &mut self.term);
                        let sv = ScriptValue::Str(String::from_utf8_lossy(&self.term).into_owned());
                        self.tmp.clear();
                        script.run(doc, score, Some(&sv), &mut self.tmp)?;
                        self.sbuf.append(&mut self.tmp);
                    }
                } else {
                    script.run(doc, score, None, &mut self.sbuf)?;
                }
                let mut vals: Vec<Vec<u8>> = Vec::with_capacity(self.sbuf.len());
                for v in &self.sbuf {
                    let s = match v {
                        ScriptValue::Null => continue,
                        ScriptValue::Date(ms) => match &self.script_format {
                            f @ DocValueFormat::DateTime { .. } => f.long_string(*ms),
                            _ => ms.to_string(),
                        },
                        other => other.to_java_string(),
                    };
                    vals.push(s.into_bytes());
                }
                vals.sort();
                for v in vals {
                    self.data.extend_from_slice(&v);
                    self.offsets.push(self.data.len());
                }
            }
        }
        if self.offsets.len() == 1
            && let Some(m) = &self.missing
        {
            self.data.extend_from_slice(m);
            self.offsets.push(self.data.len());
        }
        Ok(self.offsets.len() - 1)
    }

    #[inline]
    pub fn value(&self, i: usize) -> &[u8] {
        &self.data[self.offsets[i]..self.offsets[i + 1]]
    }
}

/// Geo point values of one segment (`MultiGeoPointValues`).
pub struct GeoReader {
    dv: DvNum,
    missing: Option<(f64, f64)>,
    buf: Vec<(f64, f64)>,
}

impl GeoReader {
    pub fn open(vs: &BoundVs, seg: &SegmentContext) -> Result<GeoReader> {
        if vs.script.is_some() {
            return Err(AggError::illegal_argument("value source of type [geo_point] is not supported by scripts"));
        }
        let dv = match &vs.field {
            Some(f) => DvNum::open(seg, f.number),
            None => DvNum::None,
        };
        // The missing point is quantized like indexed points.
        let missing = match &vs.missing {
            Some(MissingValue::Geo(lat, lon)) => {
                Some((encode::decode_latitude(encode::encode_latitude(*lat)), encode::decode_longitude(encode::encode_longitude(*lon))))
            }
            _ => None,
        };
        Ok(GeoReader { dv, missing, buf: Vec::new() })
    }

    /// The points of `doc` as `(lat, lon)` (decoded `LatLonDocValuesField` values).
    #[inline]
    pub fn points(&mut self, doc: u32) -> &[(f64, f64)] {
        self.buf.clear();
        let buf = &mut self.buf;
        self.dv.for_each(doc, |v| buf.push(decode_lat_lon(v)));
        if self.buf.is_empty()
            && let Some(m) = self.missing
        {
            self.buf.push(m);
        }
        &self.buf
    }
}

/// Decodes a `LatLonDocValuesField` value (`lat << 32 | lon`).
#[inline]
pub fn decode_lat_lon(v: i64) -> (f64, f64) {
    let lat = (v >> 32) as i32;
    let lon = v as i32;
    (encode::decode_latitude(lat), encode::decode_longitude(lon))
}

/// Per-document counts (`_doc_count` field, 1 otherwise).
#[derive(Clone)]
pub struct DocCounter {
    dv: Option<NumericDocValues>,
    multi: Option<SortedNumericDocValues>,
}

impl DocCounter {
    pub fn none() -> DocCounter {
        DocCounter { dv: None, multi: None }
    }

    pub fn open(seg: &SegmentContext, field: Option<u32>) -> DocCounter {
        let Some(f) = field else { return DocCounter::none() };
        match seg.reader.doc_values(f) {
            Some(DocValuesReader::Numeric(n)) => DocCounter { dv: Some(n.clone()), multi: None },
            Some(DocValuesReader::SortedNumeric(s)) => DocCounter { dv: None, multi: Some(s.clone()) },
            _ => DocCounter::none(),
        }
    }

    #[inline]
    pub fn count(&self, doc: u32) -> u64 {
        if let Some(n) = &self.dv {
            return n.get(doc).map_or(1, |v| v.max(0) as u64);
        }
        if let Some(s) = &self.multi {
            return s.values(doc).next().map_or(1, |v| v.max(0) as u64);
        }
        1
    }

    #[inline]
    pub fn is_constant(&self) -> bool {
        self.dv.is_none() && self.multi.is_none()
    }
}
