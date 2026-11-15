//! ES|QL scalar functions: name resolution (with Elasticsearch's aliases), arity and
//! argument-type checks with Elasticsearch's exact messages, result types, and evaluation.
//!
//! A call is resolved once, when the query is compiled, to an [`F`]; evaluating a row is then a
//! jump on that enum rather than a name lookup. Checks happen in Elasticsearch's order: an unknown
//! name is a `verification_exception`, a wrong argument count a `parsing_exception` ("error
//! building [f]: expects ..."), a wrong argument type a `verification_exception` naming the
//! argument ("second argument of [f(a, b)] must be [integer], found value [b] type [boolean]").
//!
//! Evaluation failures don't fail the query: like Elasticsearch, the value becomes `null` and two
//! warnings are recorded ("evaluation of [..] failed, treating result as null" plus the Java
//! exception text), which [`Fail::Warn`] carries. Single-value functions given a multi-value do
//! the same with `single-value function encountered multi-value`. A few argument errors on
//! constants are request errors in Elasticsearch (planning folds them); [`Fail::Error`] carries
//! those.

use super::value::{self, DataType, V};
use crate::error::EsError;
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum F {
    // math
    Abs,
    Acos,
    Acosh,
    Asin,
    Asinh,
    Atan,
    Atanh,
    Atan2,
    Cbrt,
    Ceil,
    Clamp,
    ClampMax,
    ClampMin,
    CopySign,
    Cos,
    Cosh,
    E,
    Exp,
    Floor,
    Greatest,
    Hypot,
    Least,
    Log,
    Log10,
    Pi,
    Pow,
    Round,
    RoundTo,
    Scalb,
    Signum,
    Sin,
    Sinh,
    Sqrt,
    Tan,
    Tanh,
    Tau,
    ToDegrees,
    ToRadians,
    // string
    BitLength,
    ByteLength,
    Concat,
    Contains,
    EndsWith,
    FromBase64,
    Hash,
    LTrim,
    Left,
    Length,
    Locate,
    Md5,
    RTrim,
    Repeat,
    Replace,
    Reverse,
    Right,
    Sha1,
    Sha256,
    Space,
    Split,
    StartsWith,
    Substring,
    ToBase64,
    ToLower,
    ToUpper,
    Trim,
    UrlDecode,
    UrlEncode,
    UrlEncodeComponent,
    // date
    DateDiff,
    DateExtract,
    DateFormat,
    DateParse,
    DateTrunc,
    DayName,
    MonthName,
    Now,
    // null, ip
    Coalesce,
    CidrMatch,
    IpPrefix,
    // conversion
    ToBoolean,
    ToDatetime,
    ToDateNanos,
    ToDouble,
    ToInteger,
    ToIp,
    ToLong,
    ToString,
    ToText,
    ToAggregateMetricDouble,
    ToUnsignedLong,
    ToVersion,
    ToGeoPoint,
    ToCartesianPoint,
    ToGeoShape,
    ToCartesianShape,
    ToGeohash,
    ToGeotile,
    ToGeohex,
    // spatial
    StX,
    StY,
    StGeometryType,
    StDimension,
    StIsEmpty,
    // multi-value
    MvAppend,
    MvAvg,
    MvConcat,
    MvContains,
    MvCount,
    MvDedupe,
    MvDifference,
    MvFirst,
    MvIntersection,
    MvLast,
    MvMax,
    MvMedian,
    MvMedianAbsoluteDeviation,
    MvMin,
    MvPercentile,
    MvPseriesWeightedSum,
    MvSlice,
    MvSort,
    MvSum,
    MvUnion,
    MvZip,
}

/// Every name (primary names and aliases, lower case) and its function.
const NAMES: &[(&str, F)] = &[
    ("abs", F::Abs),
    ("acos", F::Acos),
    ("acosh", F::Acosh),
    ("asin", F::Asin),
    ("asinh", F::Asinh),
    ("atan", F::Atan),
    ("atanh", F::Atanh),
    ("atan2", F::Atan2),
    ("cbrt", F::Cbrt),
    ("ceil", F::Ceil),
    ("clamp", F::Clamp),
    ("clamp_max", F::ClampMax),
    ("clamp_min", F::ClampMin),
    ("copy_sign", F::CopySign),
    ("cos", F::Cos),
    ("cosh", F::Cosh),
    ("e", F::E),
    ("exp", F::Exp),
    ("floor", F::Floor),
    ("greatest", F::Greatest),
    ("hypot", F::Hypot),
    ("least", F::Least),
    ("log", F::Log),
    ("log10", F::Log10),
    ("pi", F::Pi),
    ("pow", F::Pow),
    ("round", F::Round),
    ("round_to", F::RoundTo),
    ("scalb", F::Scalb),
    ("signum", F::Signum),
    ("sin", F::Sin),
    ("sinh", F::Sinh),
    ("sqrt", F::Sqrt),
    ("tan", F::Tan),
    ("tanh", F::Tanh),
    ("tau", F::Tau),
    ("to_degrees", F::ToDegrees),
    ("to_radians", F::ToRadians),
    ("bit_length", F::BitLength),
    ("byte_length", F::ByteLength),
    ("concat", F::Concat),
    ("contains", F::Contains),
    ("ends_with", F::EndsWith),
    ("from_base64", F::FromBase64),
    ("hash", F::Hash),
    ("ltrim", F::LTrim),
    ("left", F::Left),
    ("length", F::Length),
    ("locate", F::Locate),
    ("md5", F::Md5),
    ("rtrim", F::RTrim),
    ("repeat", F::Repeat),
    ("replace", F::Replace),
    ("reverse", F::Reverse),
    ("right", F::Right),
    ("sha1", F::Sha1),
    ("sha256", F::Sha256),
    ("space", F::Space),
    ("split", F::Split),
    ("starts_with", F::StartsWith),
    ("substring", F::Substring),
    ("to_base64", F::ToBase64),
    ("to_lower", F::ToLower),
    ("to_upper", F::ToUpper),
    ("trim", F::Trim),
    ("url_decode", F::UrlDecode),
    ("url_encode", F::UrlEncode),
    ("url_encode_component", F::UrlEncodeComponent),
    ("date_diff", F::DateDiff),
    ("date_extract", F::DateExtract),
    ("date_format", F::DateFormat),
    ("date_parse", F::DateParse),
    ("date_trunc", F::DateTrunc),
    ("day_name", F::DayName),
    ("month_name", F::MonthName),
    ("now", F::Now),
    ("coalesce", F::Coalesce),
    ("cidr_match", F::CidrMatch),
    ("ip_prefix", F::IpPrefix),
    ("to_boolean", F::ToBoolean),
    ("to_bool", F::ToBoolean),
    ("to_datetime", F::ToDatetime),
    ("to_dt", F::ToDatetime),
    ("to_date_nanos", F::ToDateNanos),
    ("to_datenanos", F::ToDateNanos),
    ("to_double", F::ToDouble),
    ("to_dbl", F::ToDouble),
    ("to_integer", F::ToInteger),
    ("to_int", F::ToInteger),
    ("to_ip", F::ToIp),
    ("to_long", F::ToLong),
    ("to_string", F::ToString),
    ("to_str", F::ToString),
    ("to_text", F::ToText),
    ("to_aggregate_metric_double", F::ToAggregateMetricDouble),
    ("to_aggregatemetricdouble", F::ToAggregateMetricDouble),
    ("to_unsigned_long", F::ToUnsignedLong),
    ("to_ulong", F::ToUnsignedLong),
    ("to_ul", F::ToUnsignedLong),
    ("to_version", F::ToVersion),
    ("to_ver", F::ToVersion),
    ("to_geopoint", F::ToGeoPoint),
    ("to_cartesianpoint", F::ToCartesianPoint),
    ("to_geoshape", F::ToGeoShape),
    ("to_cartesianshape", F::ToCartesianShape),
    ("to_geohash", F::ToGeohash),
    ("to_geotile", F::ToGeotile),
    ("to_geohex", F::ToGeohex),
    ("st_x", F::StX),
    ("st_y", F::StY),
    ("st_geometrytype", F::StGeometryType),
    ("st_dimension", F::StDimension),
    ("st_isempty", F::StIsEmpty),
    ("mv_append", F::MvAppend),
    ("mv_avg", F::MvAvg),
    ("mv_concat", F::MvConcat),
    ("mv_contains", F::MvContains),
    ("mv_count", F::MvCount),
    ("mv_dedupe", F::MvDedupe),
    ("mv_difference", F::MvDifference),
    ("mv_first", F::MvFirst),
    ("mv_intersection", F::MvIntersection),
    ("mv_last", F::MvLast),
    ("mv_max", F::MvMax),
    ("mv_median", F::MvMedian),
    ("mv_median_absolute_deviation", F::MvMedianAbsoluteDeviation),
    ("mv_min", F::MvMin),
    ("mv_percentile", F::MvPercentile),
    ("mv_pseries_weighted_sum", F::MvPseriesWeightedSum),
    ("mv_slice", F::MvSlice),
    ("mv_sort", F::MvSort),
    ("mv_sum", F::MvSum),
    ("mv_union", F::MvUnion),
    ("mv_zip", F::MvZip),
];

/// The function a (lower-case) name calls.
/// A function's (first) name.
pub fn name_of(f: F) -> &'static str {
    NAMES.iter().find(|(_, g)| *g == f).map_or("", |(n, _)| n)
}

pub fn resolve(name: &str) -> Option<F> {
    NAMES.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
}

#[derive(Clone, Copy)]
enum Arity {
    Exact(usize),
    Range(usize, usize),
    AtLeast(usize),
}

impl Arity {
    fn accepts(self, n: usize) -> bool {
        match self {
            Arity::Exact(k) => n == k,
            Arity::Range(a, b) => (a..=b).contains(&n),
            Arity::AtLeast(k) => n >= k,
        }
    }

    /// `EsqlFunctionRegistry`'s messages.
    fn message(self) -> &'static str {
        match self {
            Arity::Exact(0) => "expects no arguments",
            Arity::Exact(1) => "expects exactly one argument",
            Arity::Exact(2) => "expects exactly two arguments",
            Arity::Exact(3) => "expects exactly three arguments",
            Arity::Exact(_) => "expects exactly four arguments",
            Arity::Range(1, 2) => "expects one or two arguments",
            Arity::Range(2, 3) => "expects two or three arguments",
            Arity::Range(1, 3) => "expects one, two or three arguments",
            Arity::Range(3, 4) => "expects three or four arguments",
            Arity::Range(..) => "expects minimum two, maximum four arguments",
            Arity::AtLeast(1) => "expects at least one argument",
            Arity::AtLeast(_) => "expects at least two arguments",
        }
    }
}

/// Why an evaluation produced no value.
#[derive(Debug)]
pub enum Fail {
    /// `null` plus a warning with this Java exception text.
    Warn(String),
    /// A conversion of a multi-value where some values failed: the values that converted (or
    /// `null`) plus one warning per failed value, as Elasticsearch's converters record them.
    Partial(V, Vec<String>),
    /// The request fails.
    Error(EsError),
}

fn warn(s: impl Into<String>) -> Fail {
    Fail::Warn(s.into())
}

pub(crate) const MULTI: &str = "java.lang.IllegalArgumentException: single-value function encountered multi-value";

fn arith_err(msg: &str) -> Fail {
    warn(format!("java.lang.ArithmeticException: {msg}"))
}

fn invalid_arg(msg: impl std::fmt::Display) -> Fail {
    warn(format!("org.elasticsearch.xpack.esql.core.InvalidArgumentException: {msg}"))
}

fn is_num(t: DataType) -> bool {
    t.is_numeric()
}
fn is_str(t: DataType) -> bool {
    t.is_string()
}
fn is_int(t: DataType) -> bool {
    t == DataType::Integer
}
fn is_whole(t: DataType) -> bool {
    matches!(t, DataType::Integer | DataType::Long)
}
fn is_date(t: DataType) -> bool {
    matches!(t, DataType::Date | DataType::DateNanos)
}
fn is_span(t: DataType) -> bool {
    matches!(t, DataType::DatePeriod | DataType::TimeDuration)
}

type Check = (&'static str, fn(DataType) -> bool);

const NUMERIC: Check = ("numeric", is_num);
const STRING: Check = ("string", is_str);
const INTEGER: Check = ("integer", is_int);
const WHOLE: Check = ("whole number except unsigned_long or counter types", is_whole);
const DATE: Check = ("datetime or date_nanos", is_date);
const IP: Check = ("ip", |t| t == DataType::Ip);
const ANY: Check = ("any", |_| true);

/// Same type as another argument (strings of either kind match each other).
fn same(a: DataType, b: DataType) -> bool {
    a == b || (a.is_string() && b.is_string())
}

impl F {
    pub fn name(self) -> &'static str {
        NAMES.iter().find(|(_, f)| *f == self).map_or("", |(n, _)| n)
    }

    fn arity(self) -> Arity {
        use Arity::*;
        use F::*;
        match self {
            E | Pi | Tau | Now => Exact(0),
            Atan2 | CopySign | Hypot | Pow | Scalb | ClampMax | ClampMin | Contains | EndsWith | StartsWith | Hash | Left | Right | Split | DateExtract | DateTrunc
            | MvAppend | MvConcat | MvContains | MvDifference | MvIntersection | MvPercentile | MvPseriesWeightedSum | MvUnion => Exact(2),
            Clamp | Replace | DateDiff => Exact(3),
            Log | Round | DateFormat | MvSort | ToInteger | ToLong | ToIp | Repeat => Range(1, 2),
            Substring | Locate | IpPrefix | MvSlice | MvZip => Range(2, 3),
            DateParse => Range(1, 3),
            Greatest | Least | Coalesce => AtLeast(1),
            Concat | CidrMatch | RoundTo => AtLeast(2),
            _ => Exact(1),
        }
    }

    /// The type check of argument `i` of `n` given the earlier arguments' types (`None`: any).
    fn check(self, n: usize, i: usize, types: &[DataType]) -> Option<Check> {
        use F::*;
        let first = types.first().copied().unwrap_or(DataType::Null);
        Some(match (self, i) {
            (Abs | Acos | Acosh | Asin | Asinh | Atan | Atanh | Cbrt | Ceil | Cos | Cosh | Exp | Floor | Log10 | Signum | Sin | Sinh | Sqrt | Tan | Tanh | ToDegrees | ToRadians, 0) => NUMERIC,
            (Atan2 | CopySign | Hypot | Pow | Log, _) => NUMERIC,
            (Round, 0) => NUMERIC,
            (Round | Scalb, 1) => WHOLE,
            (Scalb, 0) => NUMERIC,
            (MvAvg | MvMedian | MvMedianAbsoluteDeviation | MvSum, 0) => NUMERIC,
            (MvPercentile, _) => ("numeric except unsigned_long", |t| t.is_numeric() && t != DataType::UnsignedLong),
            (MvPseriesWeightedSum, _) => ("double", |t| t == DataType::Double),
            (BitLength | ByteLength | Concat | Contains | EndsWith | StartsWith | Hash | LTrim | RTrim | Trim | Length | Md5 | Sha1 | Sha256 | Reverse | ToLower | ToUpper | FromBase64 | ToBase64 | UrlDecode | UrlEncode | UrlEncodeComponent | Split | ToText, 0) => STRING,
            (Concat | Contains | EndsWith | StartsWith | Hash | Split | Replace | DateParse | MvConcat | MvZip | CidrMatch, _) if i > 0 => STRING,
            (Left | Right | Substring | Repeat | Locate | MvSlice | IpPrefix, _) if i >= 1 && !(self == Locate && i == 1) => INTEGER,
            (Locate, 1) => STRING,
            (Left | Right | Substring | Repeat | Locate | Replace, 0) => STRING,
            (Space, 0) => INTEGER,
            (DateDiff, 0) => STRING,
            (DateDiff, _) | (DateExtract, 1) => DATE,
            (DateExtract, 0) => STRING,
            (DateFormat, 0) if n == 1 => DATE,
            (DateFormat, 0) => STRING,
            (DateFormat, 1) => DATE,
            (DateParse, 0) => STRING,
            (DateTrunc, 0) => ("dateperiod or timeduration", is_span),
            (DateTrunc, 1) => ("date_nanos or datetime", is_date),
            (DayName | MonthName, 0) => DATE,
            (CidrMatch | IpPrefix, 0) => IP,
            (MvConcat | MvZip, 0) => STRING,
            (MvSort, 1) => STRING,
            (ToDatetime, 0) => ("date_nanos or datetime or numeric or string", |t| is_date(t) || t.is_numeric() || t.is_string()),
            (ToDateNanos, 0) => ("date_nanos or datetime or double or long or string or unsigned_long", |t| {
                is_date(t) || matches!(t, DataType::Double | DataType::Long | DataType::UnsignedLong) || t.is_string()
            }),
            (ToIp, 0) => ("ip or string", |t| t == DataType::Ip || t.is_string()),
            (ToAggregateMetricDouble, 0) => ("numeric or aggregate_metric_double", |t| t.is_numeric() || t == DataType::AggregateMetricDouble),
            (ToDouble, 0) => ("boolean or counter_double or counter_integer or counter_long or datetime or numeric or string", |t| t.is_counter() || t.is_numeric() || t.is_string() || matches!(t, DataType::Boolean | DataType::Date)),
            (ToLong, 0) if n == 1 => ("boolean or counter_integer or counter_long or date_nanos or datetime or geohash or geohex or geotile or numeric or string", |t| {
                matches!(t, DataType::CounterInteger | DataType::CounterLong | DataType::Boolean | DataType::Date | DataType::DateNanos) || t.is_grid() || t.is_numeric() || t.is_string()
            }),
            (ToInteger, 0) if n == 1 => ("boolean or counter_integer or datetime or numeric or string", |t| matches!(t, DataType::CounterInteger | DataType::Boolean | DataType::Date) || t.is_numeric() || t.is_string()),
            (ToString, 0) => (
                "aggregate_metric_double or boolean or cartesian_point or cartesian_shape or date_nanos or date_range or datetime or dense_vector or exponential_histogram or flattened or geo_point or geo_shape or geohash or geohex or geotile or histogram or ip or numeric or string or version",
                |t| !t.is_counter() && !matches!(t, DataType::TDigest | DataType::Tsid | DataType::Source | DataType::DatePeriod | DataType::TimeDuration | DataType::Unsupported),
            ),
            (ToVersion, 0) => ("string or version", |t| t == DataType::Version || t.is_string()),
            (ToGeoPoint, 0) => ("geo_point or string", |t| t == DataType::GeoPoint || t.is_string()),
            (ToGeohash, 0) => ("geohash or long or string", |t| matches!(t, DataType::Geohash | DataType::Long) || t.is_string()),
            (ToGeotile, 0) => ("geotile or long or string", |t| matches!(t, DataType::Geotile | DataType::Long) || t.is_string()),
            (ToGeohex, 0) => ("geohex or long or string", |t| matches!(t, DataType::Geohex | DataType::Long) || t.is_string()),
            (ToCartesianPoint, 0) => ("cartesian_point or string", |t| t == DataType::CartesianPoint || t.is_string()),
            (ToGeoShape, 0) => ("geo_point or geo_shape or string", |t| matches!(t, DataType::GeoPoint | DataType::GeoShape) || t.is_string()),
            (ToCartesianShape, 0) => ("cartesian_point or cartesian_shape or string", |t| matches!(t, DataType::CartesianPoint | DataType::CartesianShape) || t.is_string()),
            (StX | StY, 0) => ("geo_point or cartesian_point", |t| matches!(t, DataType::GeoPoint | DataType::CartesianPoint)),
            (StGeometryType | StDimension | StIsEmpty, 0) => ("geo_point, cartesian_point, geo_shape or cartesian_shape", super::spatial::is_spatial),
            (ToInteger | ToLong, 0) if n == 2 => STRING,
            (ToInteger | ToLong, 1) => ("integer", |t| matches!(t, DataType::Integer | DataType::Long | DataType::UnsignedLong)),
            // Arguments that must have the first argument's type.
            (Coalesce | Greatest | Least | MvAppend | MvContains | MvDifference | MvIntersection | MvUnion | Clamp | ClampMin | ClampMax, _) if i > 0 => {
                return if first == DataType::Null { None } else { Some((type_name_static(first), same_as_first(first))) };
            }
            _ => ANY,
        })
    }

    /// Whether type errors name the argument's position ("first argument of").
    fn positional(self) -> bool {
        !matches!(self.arity(), Arity::Exact(0 | 1)) && self != F::Concat
    }

    /// Whether multi-values are this function's input (others answer `null` with a warning).
    fn takes_multi(self) -> bool {
        use F::*;
        matches!(
            self,
            Coalesce
                | Greatest
                | Least
                | CidrMatch
                | ToLower
                | ToUpper
                | ToBoolean
                | ToDatetime
                | ToDateNanos
                | ToDouble
                | ToInteger
                | ToIp
                | ToLong
                | ToString
                | ToText
                | ToAggregateMetricDouble
                | ToUnsignedLong
                | ToVersion
                | ToGeoPoint
                | ToCartesianPoint
                | ToGeoShape
                | ToCartesianShape
                | ToGeohash
                | ToGeotile
                | ToGeohex
                | StX
                | StY
                | StGeometryType
                | StDimension
                | StIsEmpty
                | ToDegrees
                | ToRadians
                | MvAppend
                | MvAvg
                | MvConcat
                | MvContains
                | MvCount
                | MvDedupe
                | MvDifference
                | MvFirst
                | MvIntersection
                | MvLast
                | MvMax
                | MvMedian
                | MvMedianAbsoluteDeviation
                | MvMin
                | MvPercentile
                | MvPseriesWeightedSum
                | MvSlice
                | MvSort
                | MvSum
                | MvUnion
                | MvZip
        )
    }

    /// Whether `null` arguments make the result `null` without evaluating.
    fn null_in_null_out(self) -> bool {
        !matches!(self, F::Coalesce | F::MvAppend | F::MvContains | F::MvZip | F::E | F::Pi | F::Tau | F::Now)
    }

    /// The result type for these argument types.
    pub fn result_type(self, args: &[DataType]) -> DataType {
        use DataType as T;
        use F::*;
        let a0 = args.first().copied().unwrap_or(T::Null);
        let widest = || args.iter().copied().fold(T::Null, |acc, t| if acc == T::Null { t } else if t.is_numeric() && acc.is_numeric() { T::widen(acc, t) } else { acc });
        match self {
            Abs | Ceil | MvDedupe | MvFirst | MvLast | MvMax | MvMin | MvMedian | MvMedianAbsoluteDeviation | MvPercentile | MvSort | MvSlice | MvSum | MvDifference | MvIntersection | MvUnion
            | CopySign => a0,
            Floor | Round => a0,
            Clamp | ClampMin | ClampMax | RoundTo => widest(),
            Coalesce | Greatest | Least | MvAppend => args.iter().copied().find(|t| *t != T::Null).unwrap_or(T::Null),
            Acos | Acosh | Asin | Asinh | Atan | Atanh | Atan2 | Cbrt | Cos | Cosh | E | Exp | Hypot | Log | Log10 | Pi | Pow | Scalb | Signum | Sin | Sinh | Sqrt | Tan | Tanh | Tau
            | ToDegrees | ToRadians | MvAvg | MvPseriesWeightedSum | ToDouble => T::Double,
            BitLength | ByteLength | Length | Locate | MvCount | DateDiff | ToInteger => T::Integer,
            Contains | EndsWith | StartsWith | CidrMatch | MvContains | ToBoolean => T::Boolean,
            DateExtract | ToLong => T::Long,
            DateParse | Now => T::Date,
            DateTrunc => {
                if args.get(1) == Some(&T::DateNanos) {
                    T::DateNanos
                } else {
                    T::Date
                }
            }
            ToDatetime => T::Date,
            ToDateNanos => T::DateNanos,
            IpPrefix | ToIp => T::Ip,
            ToText => T::Text,
            ToAggregateMetricDouble => T::AggregateMetricDouble,
            ToUnsignedLong => T::UnsignedLong,
            ToVersion => T::Version,
            ToGeoPoint => T::GeoPoint,
            ToCartesianPoint => T::CartesianPoint,
            ToGeoShape => T::GeoShape,
            ToCartesianShape => T::CartesianShape,
            ToGeohash => T::Geohash,
            ToGeotile => T::Geotile,
            ToGeohex => T::Geohex,
            StX | StY => T::Double,
            StDimension => T::Integer,
            StIsEmpty => T::Boolean,
            _ => T::Keyword,
        }
    }
}

/// Whether `f` is a type conversion (`TO_LONG`, `TO_IP`, ...): what resolves a union type.
pub fn is_conversion(f: F) -> bool {
    use F::*;
    matches!(
        f,
        ToBoolean | ToDatetime | ToDateNanos | ToDouble | ToInteger | ToIp | ToLong | ToString | ToText | ToUnsignedLong | ToVersion | ToGeoPoint | ToCartesianPoint | ToGeoShape | ToCartesianShape | ToAggregateMetricDouble | ToGeohash | ToGeotile | ToGeohex
    )
}

/// Whether conversion `f` takes a value of mapping type `t` (the conversions' signatures:
/// `TO_LONG` takes booleans, dates, strings and numbers but not IPs, ...).
pub fn conversion_accepts(f: F, t: &str) -> bool {
    use F::*;
    let numeric = matches!(t, "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float" | "scaled_float" | "unsigned_long");
    let string = matches!(t, "keyword" | "text" | "constant_keyword" | "wildcard" | "match_only_text");
    let date = matches!(t, "date" | "date_nanos");
    match f {
        ToLong | ToInteger | ToDouble | ToUnsignedLong => numeric || string || date || t == "boolean",
        ToBoolean => numeric || string || t == "boolean",
        ToDatetime | ToDateNanos => numeric || string || date,
        ToIp => string || t == "ip",
        ToVersion => string || t == "version",
        ToString | ToText => !matches!(t, "object" | "nested" | "binary" | "completion"),
        ToGeoPoint => string || t == "geo_point",
        ToCartesianPoint => string || t == "point",
        ToGeoShape => string || matches!(t, "geo_point" | "geo_shape"),
        ToCartesianShape => string || matches!(t, "point" | "shape"),
        ToAggregateMetricDouble => numeric || t == "aggregate_metric_double",
        ToGeohash | ToGeotile | ToGeohex => string || t == "long",
        _ => false,
    }
}

/// The conversion function of a `::type` cast.
pub fn conversion_to(t: DataType) -> Option<F> {
    use DataType as T;
    Some(match t {
        T::Boolean => F::ToBoolean,
        T::Integer => F::ToInteger,
        T::Long => F::ToLong,
        T::UnsignedLong => F::ToUnsignedLong,
        T::Double => F::ToDouble,
        T::Keyword => F::ToString,
        T::Text => F::ToText,
        T::Date => F::ToDatetime,
        T::DateNanos => F::ToDateNanos,
        T::Ip => F::ToIp,
        T::Version => F::ToVersion,
        T::GeoPoint => F::ToGeoPoint,
        T::CartesianPoint => F::ToCartesianPoint,
        T::GeoShape => F::ToGeoShape,
        T::CartesianShape => F::ToCartesianShape,
        _ => return None,
    })
}

/// A type as Elasticsearch's messages name it (`DataType.typeName`: `datetime`, where columns
/// say `date`).
pub fn type_name_static(t: DataType) -> &'static str {
    match t {
        DataType::Text => "text",
        DataType::Date => "datetime",
        _ => t.name(),
    }
}

fn same_as_first(first: DataType) -> fn(DataType) -> bool {
    use DataType as T;
    // `fn` pointers can't capture, so one per type.
    match first {
        T::Boolean => |t| same(t, T::Boolean),
        T::Integer => |t| same(t, T::Integer),
        T::Long => |t| same(t, T::Long),
        T::UnsignedLong => |t| same(t, T::UnsignedLong),
        T::Double => |t| same(t, T::Double),
        T::Keyword | T::Text => |t| t.is_string(),
        T::Date => |t| same(t, T::Date),
        T::DateNanos => |t| same(t, T::DateNanos),
        T::Ip => |t| same(t, T::Ip),
        T::Version => |t| same(t, T::Version),
        T::GeoPoint => |t| same(t, T::GeoPoint),
        _ => |_| true,
    }
}

const ORDINALS: [&str; 6] = ["first", "second", "third", "fourth", "fifth", "sixth"];

/// Whether argument `i` of `n` may have type `ty` (`true` when the function takes anything).
pub fn accepts(f: F, n: usize, i: usize, types: &[DataType], ty: DataType) -> bool {
    f.check(n, i, types).is_none_or(|(_, ok)| ok(ty))
}

/// An argument as the checks see it: its type, source text and literal value.
pub struct Arg<'a> {
    pub ty: DataType,
    pub text: &'a str,
    pub lit: Option<&'a V>,
}

/// Elasticsearch's checks of a call (`call` is its source text, `written` the name as written):
/// the result type, or the error.
pub fn check(f: F, written: &str, call: &str, at: (usize, usize), args: &[Arg<'_>]) -> Result<DataType, EsError> {
    let ar = f.arity();
    if !ar.accepts(args.len()) {
        let msg = ar.message();
        let cause = EsError::java(400, "ql_illegal_argument_exception", msg);
        return Err(EsError::new(400, "parsing_exception", format!("line {}:{}: error building [{written}]: {msg}", at.0, at.1)).caused_by(cause.clone()).with_root_causes(vec![cause]));
    }
    let verification = |msg: String| EsError::new(400, "verification_exception", format!("Found 1 problem\nline {}:{}: {msg}", at.0, at.1));
    let types: Vec<DataType> = args.iter().map(|a| a.ty).collect();
    if f == F::RoundTo {
        for (i, a) in args.iter().enumerate().skip(1) {
            if a.lit.is_none() {
                return Err(verification(format!("{} argument of [{call}] must be a constant, received [{}]", ORDINALS[i.min(5)], a.text)));
            }
        }
        if !types.iter().all(|t| t.is_numeric() || is_date(*t) || *t == DataType::Null) {
            return Err(verification("all arguments must be numeric, date, or data_nanos".into()));
        }
    }
    for (i, a) in args.iter().enumerate() {
        if a.ty == DataType::Null {
            continue;
        }
        let Some((expected, ok)) = f.check(args.len(), i, &types) else { continue };
        if !ok(a.ty) {
            let ord = if f.positional() { format!("{} argument", ORDINALS[i.min(5)]) } else { "argument".into() };
            return Err(verification(format!("{ord} of [{call}] must be [{expected}], found value [{}] type [{}]", a.text, type_name_static(a.ty))));
        }
    }
    // Constant arguments Elasticsearch validates while planning.
    let lit_str = |i: usize| args.get(i).and_then(|a| a.lit).and_then(V::as_str);
    let invalid = |reason: String| EsError::new(400, "invalid_argument_exception", reason);
    match f {
        F::Split => {
            if let Some(d) = lit_str(1)
                && d.len() != 1
            {
                return Err(invalid("delimiter must be single byte for now".into()));
            }
        }
        F::DateDiff => {
            if let Some(u) = lit_str(0)
                && diff_unit(u).is_none()
            {
                return Err(invalid(format!(
                    "invalid unit format for [{call}]: A value of [YEAR, QUARTER, MONTH, DAYOFYEAR, DAY, WEEK, WEEKDAY, HOUR, MINUTE, SECOND, MILLISECOND, MICROSECOND, NANOSECOND] or their aliases is required; received [{u}]"
                )));
            }
        }
        F::DateExtract => {
            if let Some(p) = lit_str(0)
                && chrono_field(p).is_none()
            {
                return Err(invalid(format!("invalid date field for [{call}]: {p}")));
            }
        }
        F::MvSort => {
            if let Some(o) = lit_str(1)
                && !o.eq_ignore_ascii_case("asc")
                && !o.eq_ignore_ascii_case("desc")
            {
                return Err(EsError::illegal_argument(format!("Invalid order value in [{call}], expected one of [ASC, DESC] but got [{o}]")));
            }
        }
        F::Hash => {
            if let Some(alg) = lit_str(0)
                && digest(alg, b"").is_none()
            {
                let cause = EsError::java(400, "no_such_algorithm_exception", format!("{alg} MessageDigest not available"));
                return Err(invalid(format!("invalid algorithm for [{call}]: {alg} MessageDigest not available")).caused_by(cause));
            }
        }
        F::Space | F::Repeat => {
            let at = if f == F::Space { 0 } else { 1 };
            if let Some(n) = args.get(at).and_then(|a| a.lit).and_then(V::as_i64)
                && n < 0
            {
                return Err(EsError::illegal_argument(format!("Number parameter cannot be negative, found [{n}]")));
            }
        }
        _ => {}
    }
    Ok(f.result_type(&types))
}

/// Evaluates a call.
pub fn eval(f: F, args: &[V], types: &[DataType]) -> Result<V, Fail> {
    if !f.takes_multi() && args.iter().any(|a| matches!(a, V::Multi(_))) {
        return Err(warn(MULTI));
    }
    if f.null_in_null_out() && args.iter().any(V::is_null) {
        // CIDR_MATCH's and COALESCE-like functions' later nulls are handled inside.
        if !(f == F::CidrMatch && !args[0].is_null()) {
            return Ok(V::Null);
        }
    }
    let ty0 = types.first().copied().unwrap_or(DataType::Null);
    let a0 = args.first().unwrap_or(&V::Null);
    let num = |i: usize| args.get(i).and_then(V::as_f64);
    let int = |i: usize| args.get(i).and_then(V::as_i64);
    let text = |i: usize| args.get(i).map(V::to_text).unwrap_or_default();
    use F::*;
    Ok(match f {
        Abs => match a0 {
            V::Int(i) => V::Int(i.checked_abs().ok_or_else(|| arith_err("Overflow to represent absolute value of Integer.MIN_VALUE"))?),
            V::Long(l) if ty0 == DataType::UnsignedLong => V::Long(*l),
            V::Long(l) => V::Long(l.checked_abs().ok_or_else(|| arith_err("Overflow to represent absolute value of Long.MIN_VALUE"))?),
            V::Double(d) => V::Double(d.abs()),
            _ => V::Null,
        },
        Ceil | Floor => match a0 {
            V::Double(d) => V::Double(if f == Ceil { d.ceil() } else { d.floor() }),
            other => other.clone(),
        },
        Acos | Asin => {
            let x = num(0).unwrap_or(0.0);
            if !(-1.0..=1.0).contains(&x) {
                return Err(arith_err(if f == Acos { "Acos input out of range" } else { "Asin input out of range" }));
            }
            V::Double(if f == Acos { x.acos() } else { x.asin() })
        }
        Acosh => {
            let x = num(0).unwrap_or(0.0);
            if x < 1.0 {
                return Err(arith_err("Acosh input out of range"));
            }
            V::Double(x.acosh())
        }
        Atanh => {
            let x = num(0).unwrap_or(0.0);
            if x <= -1.0 || x >= 1.0 {
                return Err(arith_err("Atanh input out of range"));
            }
            V::Double(x.atanh())
        }
        Sqrt => {
            let x = num(0).unwrap_or(0.0);
            if x < 0.0 {
                return Err(arith_err("Square root of negative"));
            }
            V::Double(x.sqrt())
        }
        Log10 => {
            let x = num(0).unwrap_or(0.0);
            if x <= 0.0 {
                return Err(arith_err("Log of non-positive number"));
            }
            V::Double(x.log10())
        }
        Log => {
            let (base, x) = if args.len() == 2 { (num(0), num(1).unwrap_or(0.0)) } else { (None, num(0).unwrap_or(0.0)) };
            if x <= 0.0 || base.is_some_and(|b| b <= 0.0) {
                return Err(arith_err("Log of non-positive number"));
            }
            match base {
                Some(1.0) => return Err(arith_err("Log of base 1")),
                Some(b) => V::Double(x.log10() / b.log10()),
                None => V::Double(x.ln()),
            }
        }
        Exp | Sin | Cos | Tan | Atan | Sinh | Cosh | Tanh | Cbrt | Asinh | ToDegrees | ToRadians => {
            let x = num(0).unwrap_or(0.0);
            V::Double(match f {
                Exp => x.exp(),
                Sin => x.sin(),
                Cos => x.cos(),
                Tan => x.tan(),
                Atan => x.atan(),
                Sinh => x.sinh(),
                Cosh => x.cosh(),
                Tanh => x.tanh(),
                Cbrt => x.cbrt(),
                Asinh => x.asinh(),
                ToDegrees => x.to_degrees(),
                _ => x.to_radians(),
            })
        }
        Signum => {
            let x = num(0).unwrap_or(0.0);
            V::Double(if x > 0.0 {
                1.0
            } else if x < 0.0 {
                -1.0
            } else {
                x
            })
        }
        Atan2 => V::Double(num(0).unwrap_or(0.0).atan2(num(1).unwrap_or(0.0))),
        Hypot => finite(num(0).unwrap_or(0.0).hypot(num(1).unwrap_or(0.0)))?,
        Pow => finite(num(0).unwrap_or(0.0).powf(num(1).unwrap_or(0.0)))?,
        Scalb => finite(num(0).unwrap_or(0.0) * 2f64.powi(int(1).unwrap_or(0).clamp(-2000, 2000) as i32))?,
        CopySign => {
            let negative = match &args[1] {
                V::Double(d) => d.is_sign_negative(),
                other => other.as_i64().is_some_and(|i| i < 0),
            };
            match a0 {
                V::Int(i) => V::Int(if negative { -i.abs() } else { i.abs() }),
                V::Long(l) => V::Long(if negative { -l.abs() } else { l.abs() }),
                V::Double(d) => V::Double(if negative { -d.abs() } else { d.abs() }),
                other => other.clone(),
            }
        }
        E => V::Double(std::f64::consts::E),
        Pi => V::Double(std::f64::consts::PI),
        Tau => V::Double(std::f64::consts::TAU),
        Round => {
            let digits = int(1).unwrap_or(0);
            match a0 {
                V::Double(d) => V::Double(round_double(*d, digits)),
                V::Int(i) => V::Int(round_whole(*i as i64, digits) as i32),
                V::Long(l) => V::Long(round_whole(*l, digits)),
                other => other.clone(),
            }
        }
        RoundTo => {
            let ty = f.result_type(types);
            let x = a0;
            let mut points: Vec<V> = args[1..].iter().map(|p| value::convert(p, ty)).collect();
            points.sort_by(value::cmp);
            let v = value::convert(x, ty);
            let pick = points.iter().rev().find(|p| value::cmp(p, &v) != Ordering::Greater).or(points.first());
            pick.cloned().unwrap_or(V::Null)
        }
        Clamp | ClampMin | ClampMax => {
            let ty = f.result_type(types);
            let v = value::convert(a0, ty);
            let (lo, hi) = match f {
                Clamp => (Some(value::convert(&args[1], ty)), Some(value::convert(&args[2], ty))),
                ClampMin => (Some(value::convert(&args[1], ty)), None),
                _ => (None, Some(value::convert(&args[1], ty))),
            };
            let mut out = v;
            if let Some(l) = lo
                && value::cmp(&out, &l) == Ordering::Less
            {
                out = l;
            }
            if let Some(h) = hi
                && value::cmp(&out, &h) == Ordering::Greater
            {
                out = h;
            }
            out
        }
        Greatest | Least => {
            let mut all: Vec<&V> = args.iter().flat_map(V::values).collect();
            if all.is_empty() || args.iter().any(V::is_null) {
                return Ok(V::Null);
            }
            all.sort_by(|a, b| cmp_typed(a, b, ty0));
            (if f == Greatest { all.last() } else { all.first() }).map(|v| (*v).clone()).unwrap_or(V::Null)
        }
        Coalesce => args.iter().find(|v| !v.is_null()).cloned().unwrap_or(V::Null),
        // strings
        Length => V::Int(text(0).chars().count() as i32),
        BitLength => V::Int((text(0).len() * 8) as i32),
        ByteLength => V::Int(text(0).len() as i32),
        Concat => V::Str(args.iter().map(V::to_text).collect()),
        Contains => V::Bool(text(0).contains(text(1).as_str())),
        StartsWith => V::Bool(text(0).starts_with(text(1).as_str())),
        EndsWith => V::Bool(text(0).ends_with(text(1).as_str())),
        LTrim => V::Str(text(0).trim_start_matches(java_space).to_string()),
        RTrim => V::Str(text(0).trim_end_matches(java_space).to_string()),
        Trim => V::Str(text(0).trim_matches(java_space).to_string()),
        Reverse => V::Str(text(0).chars().rev().collect()),
        ToLower | ToUpper => map_values(a0, |v| V::Str(if f == ToLower { v.to_text().to_lowercase() } else { v.to_text().to_uppercase() })),
        Left | Right => {
            let s = text(0);
            let n = int(1).unwrap_or(0).max(0) as usize;
            let chars: Vec<char> = s.chars().collect();
            V::Str(if f == Left { chars.iter().take(n).collect() } else { chars[chars.len().saturating_sub(n)..].iter().collect() })
        }
        Substring => {
            let chars: Vec<char> = text(0).chars().collect();
            let start = int(1).unwrap_or(1);
            let start = if start < 0 { (chars.len() as i64 + start).max(0) } else { (start - 1).max(0) } as usize;
            let len = int(2).map(|l| l.max(0) as usize).unwrap_or(chars.len());
            V::Str(chars.iter().skip(start).take(len).collect())
        }
        Locate => {
            let (s, p) = (text(0), text(1));
            let from = int(2).unwrap_or(1).max(1) as usize - 1;
            let hay: String = s.chars().skip(from).collect();
            V::Int(hay.find(&p).map(|b| hay[..b].chars().count() + from + 1).unwrap_or(0) as i32)
        }
        Repeat | Space => {
            let (s, n) = if f == Space { (" ".to_string(), int(0).unwrap_or(0)) } else { (text(0), int(1).unwrap_or(0)) };
            if n < 0 {
                return Err(warn(format!("java.lang.IllegalArgumentException: Number parameter cannot be negative, found [{n}]")));
            }
            if (s.len() as u128) * (n as u128) > 1 << 20 {
                return Err(warn("org.elasticsearch.xpack.esql.core.InvalidArgumentException: Creating strings longer than [1048576] bytes is not supported"));
            }
            V::Str(s.repeat(n as usize))
        }
        Replace => {
            let re = search_analysis::jregex::compile(&text(1), 0).map_err(|e| warn(format!("java.util.regex.PatternSyntaxException: {}", e.reason)))?;
            let rep = search_analysis::jregex::Replacement::parse(&text(2), &re);
            let mut out = String::new();
            let mut caps = re.captures();
            rep.replace(&re, &text(0), true, &mut caps, &mut out).map_err(|e| warn(format!("java.lang.IllegalArgumentException: {}", e.reason)))?;
            V::Str(out)
        }
        Split => {
            let s = text(0);
            let d = text(1);
            let parts: Vec<V> = s.split(d.as_str()).map(|p| V::Str(p.to_string())).collect();
            multi(parts)
        }
        Md5 | Sha1 | Sha256 => V::Str(hex(&digest(if f == Md5 { "MD5" } else if f == Sha1 { "SHA-1" } else { "SHA-256" }, text(0).as_bytes()).unwrap_or_default())),
        Hash => match digest(&text(0), text(1).as_bytes()) {
            Some(d) => V::Str(hex(&d)),
            None => return Err(warn(format!("java.security.NoSuchAlgorithmException: {} MessageDigest not available", text(0)))),
        },
        ToBase64 => {
            use base64::Engine;
            V::Str(base64::engine::general_purpose::STANDARD.encode(text(0).as_bytes()))
        }
        FromBase64 => {
            use base64::Engine;
            let s = text(0);
            if let Some(bad) = s.bytes().find(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))) {
                return Err(warn(format!("java.lang.IllegalArgumentException: Illegal base64 character {bad:x}")));
            }
            match base64::engine::general_purpose::STANDARD.decode(s.as_bytes()) {
                Ok(b) => V::Str(String::from_utf8_lossy(&b).into_owned()),
                Err(_) => return Err(warn("java.lang.IllegalArgumentException: Input byte array has wrong 4-byte ending unit")),
            }
        }
        UrlEncode | UrlEncodeComponent => V::Str(url_encode(&text(0), f == UrlEncodeComponent)),
        UrlDecode => V::Str(url_decode(&text(0)).map_err(warn)?),
        // dates
        DateDiff => {
            let unit = diff_unit(&text(0)).ok_or_else(|| {
                invalid_arg(format!(
                    "A value of [YEAR, QUARTER, MONTH, DAYOFYEAR, DAY, WEEK, WEEKDAY, HOUR, MINUTE, SECOND, MILLISECOND, MICROSECOND, NANOSECOND] or their aliases is required; received [{}]",
                    text(0)
                ))
            })?;
            let (a, b) = (int(1).unwrap_or(0), int(2).unwrap_or(0));
            let d = date_diff(unit, a, b);
            match i32::try_from(d) {
                Ok(v) => V::Int(v),
                Err(_) => return Err(invalid_arg(format!("[{d}] out of [integer] range"))),
            }
        }
        DateExtract => {
            let field = chrono_field(&text(0)).ok_or_else(|| warn(format!("java.lang.IllegalArgumentException: No enum constant java.time.temporal.ChronoField.{}", text(0).to_ascii_uppercase())))?;
            V::Long(extract(field, int(1).unwrap_or(0)))
        }
        DateFormat => {
            // The compiler appends the request's locale.
            let (args, locale) = match args.split_last() {
                Some((V::Str(l), rest)) if types.len() == args.len() && types.last() == Some(&DataType::Keyword) && !rest.is_empty() && rest.len() <= 2 && (rest.len() == 2 || !matches!(rest[0], V::Str(_))) => (rest, l.as_str()),
                _ => (args, "en"),
            };
            let text = |i: usize| args.get(i).map(V::to_text).unwrap_or_default();
            let int = |i: usize| args.get(i).and_then(V::as_i64);
            let (fmt, ms) = if args.len() == 2 { (text(0), int(1)) } else { ("strict_date_optional_time".to_string(), int(0)) };
            let Some(ms) = ms else { return Ok(V::Null) };
            match crate::time::DateFormatter::new(&fmt) {
                Ok(fm) => V::Str(fm.with_locale(crate::time::DateLocale::parse(locale)).format_millis(ms)),
                Err(e) => return Err(warn(format!("java.lang.IllegalArgumentException: {e}"))),
            }
        }
        DateParse => {
            let (fmt, s) = if args.len() >= 2 { (text(0), text(1)) } else { ("strict_date_optional_time".to_string(), text(0)) };
            match crate::time::DateFormatter::new(&fmt).ok().and_then(|fm| fm.parse_millis(&s).ok()) {
                Some(ms) => V::Date(ms),
                None => return Err(warn(format!("java.lang.IllegalArgumentException: failed to parse date field [{s}] with format [{fmt}]"))),
            }
        }
        DateTrunc => match value::date_trunc(&text(0), int(1).unwrap_or(0)) {
            Some(t) => V::Date(t),
            None => return Err(Fail::Error(EsError::illegal_argument(format!("Unsupported interval [{}]", text(0))))),
        },
        DayName | MonthName => {
            use chrono::Datelike;
            let Some(d) = chrono::DateTime::from_timestamp_millis(int(0).unwrap_or(0)) else { return Ok(V::Null) };
            V::Str(if f == DayName { d.format("%A").to_string() } else { [
                "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December",
            ][d.month0() as usize]
                .to_string() })
        }
        Now => V::Date(crate::time::now_millis()),
        // ip
        CidrMatch => {
            let Some(ip) = a0.as_str().and_then(|s| s.parse::<std::net::IpAddr>().ok()) else { return Ok(V::Null) };
            V::Bool(args[1..].iter().flat_map(V::values).any(|c| value::cidr_contains(&c.to_text(), ip)))
        }
        IpPrefix => {
            let Some(ip) = a0.as_str().and_then(|s| s.parse::<std::net::IpAddr>().ok()) else { return Ok(V::Null) };
            let (v4, v6) = (int(1).unwrap_or(32), int(2).unwrap_or(128));
            match ip {
                std::net::IpAddr::V4(a) => {
                    if !(0..=32).contains(&v4) {
                        return Err(warn(format!("java.lang.IllegalArgumentException: Prefix length v4 must be in range [0, 32], found {v4}")));
                    }
                    let mask = if v4 == 0 { 0 } else { u32::MAX << (32 - v4) };
                    V::Ip(std::net::Ipv4Addr::from(u32::from(a) & mask).to_string())
                }
                std::net::IpAddr::V6(a) => {
                    if !(0..=128).contains(&v6) {
                        return Err(warn(format!("java.lang.IllegalArgumentException: Prefix length v6 must be in range [0, 128], found {v6}")));
                    }
                    let mask = if v6 == 0 { 0 } else { u128::MAX << (128 - v6) };
                    V::Ip(std::net::Ipv6Addr::from(u128::from(a) & mask).to_string())
                }
            }
        }
        // conversions
        ToBoolean => map_values(a0, |v| value::convert(v, DataType::Boolean)),
        ToDouble => map_try(a0, |v| match v {
            V::Str(s) => s.trim().parse::<f64>().map(V::Double).map_err(|_| invalid_arg(format!("Cannot parse number [{s}]"))),
            other => Ok(value::convert(other, DataType::Double)),
        })?,
        ToInteger | ToLong => {
            let target = if f == ToInteger { DataType::Integer } else { DataType::Long };
            let radix = int(1);
            map_try(a0, |v| {
                // Base 10 is the plain conversion; other bases parse the digits as they are
                // (`0x` dropped for base 16), within the target type.
                if let (Some(r), V::Str(s)) = (radix, v)
                    && r != 10
                {
                    let digits = if r == 16 { s.strip_prefix("0x").unwrap_or(s) } else { s.as_str() };
                    let unable = || invalid_arg(format!("Unable to convert [{digits}] to number of base [{r}]"));
                    if !(2..=36).contains(&r) {
                        return Err(unable());
                    }
                    return i64::from_str_radix(digits, r as u32)
                        .ok()
                        .and_then(|x| if target == DataType::Integer { i32::try_from(x).ok().map(V::Int) } else { Some(V::Long(x)) })
                        .ok_or_else(unable);
                }
                let out = value::convert(v, target);
                if out.is_null() {
                    return Err(match v {
                        V::Str(s) if s.trim().parse::<f64>().is_ok() => invalid_arg(format!("[{}] out of [{}] range", s.trim(), target.name())),
                        V::Str(s) => invalid_arg(format!("Cannot parse number [{s}]")),
                        other => invalid_arg(format!("[{}] out of [{}] range", other.to_text(), target.name())),
                    });
                }
                Ok(out)
            })?
        }
        ToUnsignedLong => map_try(a0, |v| {
            let out = value::convert(v, DataType::Long);
            match out {
                V::Long(l) if l >= 0 => Ok(out),
                V::Null => Err(invalid_arg(format!("Cannot parse number [{}]", v.to_text()))),
                _ => Err(invalid_arg(format!("[{}] out of [unsigned_long] range", v.to_text()))),
            }
        })?,
        ToDatetime | ToDateNanos => map_try(a0, |v| match v {
            V::Str(s) => value::parse_date(s).map(V::Date).ok_or_else(|| warn(format!("java.lang.IllegalArgumentException: failed to parse date field [{s}] with format [strict_date_optional_time]"))),
            V::Double(d) => Ok(V::Date(*d as i64)),
            other => Ok(other.as_i64().map(V::Date).unwrap_or(V::Null)),
        })?,
        ToIp => map_try(a0, |v| match v {
            V::Ip(_) => Ok(v.clone()),
            other => {
                let s = other.to_text();
                s.parse::<std::net::IpAddr>().map(|ip| V::Ip(ip.to_string())).map_err(|_| warn(format!("java.lang.IllegalArgumentException: '{s}' is not an IP string literal.")))
            }
        })?,
        ToString | ToText if ty0.is_grid() => map_values(a0, |v| v.as_i64().map_or(V::Null, |l| V::Str(super::spatial::grid_address(l, ty0)))),
        ToString | ToText | ToVersion => map_values(a0, |v| V::Str(v.to_text())),
        // Grid cells are their long encodings; strings are addresses.
        ToGeohash | ToGeotile | ToGeohex => map_try(a0, |v| match v {
            V::Str(s) => super::spatial::grid_parse(s, f.result_type(types)).map(V::Long).map_err(warn),
            other => Ok(other.as_i64().map_or(V::Null, V::Long)),
        })?,
        // Every value of the (multi-)value folds into one metric; metrics stay as they are.
        ToAggregateMetricDouble => match a0 {
            V::Null => V::Null,
            V::Str(s) => V::Str(s.clone()),
            v => {
                let xs: Vec<f64> = v.values().into_iter().filter_map(V::as_f64).collect();
                let (min, max) = xs.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
                let sum: f64 = xs.iter().sum();
                super::value::agg_metric_text(Some(min), Some(max), Some(sum), Some(xs.len() as i64)).map_or(V::Null, V::Str)
            }
        },
        ToGeoPoint | ToCartesianPoint | ToGeoShape | ToCartesianShape => {
            let to = f.result_type(types);
            // Spatial inputs already hold normalized text; strings that don't read are null
            // with a warning per failed value.
            map_try(a0, |v| if super::spatial::is_spatial(ty0) { Ok(v.clone()) } else { super::spatial::convert_checked(&v.to_text(), to).map(V::Str).map_err(Fail::Warn) })?
        }
        StX | StY => map_try(a0, |v| super::spatial::st_coordinate(&v.to_text(), ty0, f == StY).map(|c| c.map_or(V::Null, V::Double)).map_err(Fail::Warn))?,
        StGeometryType | StDimension | StIsEmpty => {
            // Multi-valued points read as their first point; shapes as their collection.
            let vals = a0.values();
            let vals = if matches!(ty0, DataType::GeoPoint | DataType::CartesianPoint) { &vals[..vals.len().min(1)] } else { &vals[..] };
            let texts: Vec<String> = vals.iter().map(|v| v.to_text()).collect();
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            match f {
                StGeometryType => super::spatial::geometry_type(&refs).map_or(V::Null, V::Str),
                StDimension => super::spatial::st_dimension(&refs).map_or(V::Null, V::Int),
                _ => super::spatial::st_is_empty(&refs).map_or(V::Null, V::Bool),
            }
        }
        // multi-values
        MvCount => V::Int(a0.values().len() as i32),
        MvFirst => a0.values().first().map(|v| (*v).clone()).unwrap_or(V::Null),
        MvLast => a0.values().last().map(|v| (*v).clone()).unwrap_or(V::Null),
        MvMin | MvMax => {
            let vals = a0.values();
            let pick = vals.iter().copied().reduce(|a, b| {
                let o = cmp_typed(a, b, ty0);
                if (f == MvMin) == (o == Ordering::Greater) { b } else { a }
            });
            pick.cloned().unwrap_or(V::Null)
        }
        MvDedupe => {
            let mut out: Vec<V> = Vec::new();
            // `MultivalueDedupeDouble` compares with the primitive `==`: `-0.0` and `0.0` are
            // one value, the first kept (unlike group keys and `VALUES`, which hash the bits).
            for v in a0.values() {
                if !out.contains(v) {
                    out.push(v.clone());
                }
            }
            multi(out)
        }
        MvSort => {
            let desc = args.get(1).is_some_and(|o| o.to_text().eq_ignore_ascii_case("desc"));
            let mut vals: Vec<V> = a0.values().into_iter().cloned().collect();
            vals.sort_by(|a, b| cmp_typed(a, b, ty0));
            if desc {
                vals.reverse();
            }
            multi(vals)
        }
        MvSlice => {
            let vals = a0.values();
            let n = vals.len() as i64;
            let norm = |i: i64| if i < 0 { i + n } else { i };
            let (s, e) = (norm(int(1).unwrap_or(0)), norm(int(2).unwrap_or(int(1).unwrap_or(0))));
            if s < 0 || e < s || s >= n {
                return Ok(V::Null);
            }
            multi(vals[s as usize..=(e.min(n - 1) as usize)].iter().map(|v| (*v).clone()).collect())
        }
        MvAppend => {
            if args.iter().any(V::is_null) {
                return Ok(V::Null);
            }
            multi(args.iter().flat_map(V::values).cloned().collect())
        }
        MvContains => {
            let sup = a0.values();
            V::Bool(args[1].values().iter().all(|x| sup.contains(x)))
        }
        MvDifference | MvIntersection | MvUnion => {
            let (a, b) = (a0.values(), args[1].values());
            let mut out: Vec<V> = Vec::new();
            let push = |v: &V, out: &mut Vec<V>| {
                if !out.contains(v) {
                    out.push(v.clone());
                }
            };
            match f {
                MvDifference => a.iter().filter(|x| !b.contains(x)).for_each(|x| push(x, &mut out)),
                MvIntersection => a.iter().filter(|x| b.contains(x)).for_each(|x| push(x, &mut out)),
                _ => a.iter().chain(b.iter()).for_each(|x| push(x, &mut out)),
            }
            multi(out)
        }
        MvConcat => V::Str(a0.values().iter().map(|v| v.to_text()).collect::<Vec<_>>().join(&text(1))),
        MvZip => {
            if a0.is_null() && args[1].is_null() {
                return Ok(V::Null);
            }
            let delim = if args.len() == 3 { if args[2].is_null() { return Ok(V::Null) } else { text(2) } } else { ",".to_string() };
            let (a, b) = (a0.values(), args[1].values());
            let out: Vec<V> = (0..a.len().max(b.len()))
                .map(|i| match (a.get(i), b.get(i)) {
                    (Some(x), Some(y)) => V::Str(format!("{}{delim}{}", x.to_text(), y.to_text())),
                    (Some(x), None) | (None, Some(x)) => V::Str(x.to_text()),
                    _ => V::Null,
                })
                .collect();
            multi(out)
        }
        MvSum => {
            let vals = a0.values();
            match ty0 {
                DataType::Double => V::Double(vals.iter().filter_map(|v| v.as_f64()).sum()),
                DataType::Integer => {
                    let mut s: i32 = 0;
                    for v in vals {
                        s = s.checked_add(v.as_i64().unwrap_or(0) as i32).ok_or_else(|| arith_err("integer overflow"))?;
                    }
                    V::Int(s)
                }
                _ => {
                    let mut s: i64 = 0;
                    for v in vals {
                        s = s.checked_add(v.as_i64().unwrap_or(0)).ok_or_else(|| arith_err("long overflow"))?;
                    }
                    V::Long(s)
                }
            }
        }
        MvAvg => {
            let vals: Vec<f64> = a0.values().iter().filter_map(|v| v.as_f64()).collect();
            V::Double(vals.iter().sum::<f64>() / vals.len() as f64)
        }
        MvMedian | MvMedianAbsoluteDeviation | MvPercentile => {
            let mut xs: Vec<f64> = a0.values().iter().filter_map(|v| v.as_f64()).collect();
            xs.sort_by(f64::total_cmp);
            let whole = ty0 != DataType::Double;
            let median = |xs: &[f64]| -> f64 {
                let m = xs.len() / 2;
                if xs.len() % 2 == 1 {
                    xs[m]
                } else if whole {
                    // Integer medians average with floor division, like `(a + b) >> 1`.
                    ((xs[m - 1] + xs[m]) / 2.0).floor()
                } else {
                    (xs[m - 1] + xs[m]) / 2.0
                }
            };
            let r = match f {
                MvMedian => median(&xs),
                MvMedianAbsoluteDeviation => {
                    let m = median(&xs);
                    let mut dev: Vec<f64> = xs.iter().map(|x| (x - m).abs()).collect();
                    dev.sort_by(f64::total_cmp);
                    median(&dev)
                }
                _ => {
                    let p = num(1).unwrap_or(50.0);
                    if !(0.0..=100.0).contains(&p) {
                        return Err(warn(format!("java.lang.IllegalArgumentException: Percentile parameter must be a number between 0 and 100, found [{}]", value::V::Double(p).to_text())));
                    }
                    let rank = p / 100.0 * (xs.len() - 1) as f64;
                    let (lo, hi) = (rank.floor() as usize, rank.ceil() as usize);
                    let v = xs[lo] + (xs[hi] - xs[lo]) * (rank - lo as f64);
                    if whole { v.floor() } else { v }
                }
            };
            match ty0 {
                DataType::Integer => V::Int(r as i32),
                DataType::Long | DataType::UnsignedLong => V::Long(r as i64),
                _ => V::Double(r),
            }
        }
        MvPseriesWeightedSum => {
            let p = num(1).unwrap_or(1.0);
            let s: f64 = a0.values().iter().filter_map(|v| v.as_f64()).enumerate().map(|(i, x)| x / ((i + 1) as f64).powf(p)).sum();
            finite(s)?
        }
    })
}

fn finite(x: f64) -> Result<V, Fail> {
    if x.is_finite() {
        Ok(V::Double(x))
    } else {
        Err(arith_err(&format!("not a finite double number: {}", if x.is_nan() { "NaN" } else if x > 0.0 { "Infinity" } else { "-Infinity" })))
    }
}

fn multi(mut vals: Vec<V>) -> V {
    match vals.len() {
        0 => V::Null,
        1 => vals.pop().expect("one"),
        _ => V::Multi(vals),
    }
}

fn map_values(v: &V, f: impl Fn(&V) -> V) -> V {
    match v {
        V::Null => V::Null,
        V::Multi(vs) => multi(vs.iter().map(f).filter(|x| !x.is_null()).collect()),
        other => f(other),
    }
}

/// Converts every value; a failed single value is a warning. The failed values of a
/// multi-value are dropped, each with its own warning (also when others converted), like
/// Elasticsearch's converters.
fn map_try(v: &V, f: impl Fn(&V) -> Result<V, Fail>) -> Result<V, Fail> {
    match v {
        V::Null => Ok(V::Null),
        V::Multi(vs) => {
            let mut out = Vec::with_capacity(vs.len());
            let mut failed = Vec::new();
            for x in vs {
                match f(x) {
                    Ok(y) if !y.is_null() => out.push(y),
                    Ok(_) => {}
                    Err(Fail::Warn(m)) => failed.push(m),
                    Err(Fail::Partial(y, ms)) => {
                        out.push(y);
                        failed.extend(ms);
                    }
                    Err(e @ Fail::Error(_)) => return Err(e),
                }
            }
            if failed.is_empty() { Ok(multi(out)) } else { Err(Fail::Partial(multi(out), failed)) }
        }
        other => f(other),
    }
}

/// Orders two values of a column of type `ty` (versions by version precedence).
pub fn cmp_typed(a: &V, b: &V, ty: DataType) -> Ordering {
    if ty == DataType::Version
        && let (V::Str(x), V::Str(y)) = (a, b)
    {
        return version_cmp(x, y);
    }
    value::cmp(a, b)
}

/// `version` field precedence (`VersionEncoder`): semantic versions compare by numeric parts,
/// then a pre-release sorts before its release; strings that aren't versions sort after every
/// valid version, by their bytes.
pub fn version_cmp(a: &str, b: &str) -> Ordering {
    type Parts<'a> = Vec<(u8, u64, &'a str)>;
    fn parts(t: &str) -> Parts<'_> {
        t.split('.').map(|p| if let Ok(n) = p.parse::<u64>() { (0u8, n, "") } else { (1u8, 0, p) }).collect()
    }
    fn key(s: &str) -> (bool, Parts<'_>, Option<Parts<'_>>) {
        let main_end = s.find(['-', '+']).unwrap_or(s.len());
        let (main, rest) = s.split_at(main_end);
        let valid = !main.is_empty() && main.split('.').all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()));
        let pre = rest.strip_prefix('-').map(|p| p.split('+').next().unwrap_or(""));
        (valid, parts(main), pre.map(parts))
    }
    let (va, ma, pa) = key(a);
    let (vb, mb, pb) = key(b);
    match (va, vb) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        (false, false) => return a.cmp(b),
        _ => {}
    }
    let o = ma.cmp(&mb);
    if o != Ordering::Equal {
        return o;
    }
    match (pa, pb) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(&y),
    }
}

/// `Character.isWhitespace` (what `TRIM` strips): no-break spaces aren't whitespace.
fn java_space(c: char) -> bool {
    (c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)) && !matches!(c, '\u{a0}' | '\u{2007}' | '\u{202f}')
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// `MessageDigest.getInstance(algorithm)` (names are case-insensitive).
fn digest(alg: &str, data: &[u8]) -> Option<Vec<u8>> {
    use sha2::Digest;
    Some(match alg.to_ascii_uppercase().as_str() {
        "MD5" => crate::ingest::pipeline::hash("MD5", data),
        "SHA-1" | "SHA1" | "SHA" => sha1::Sha1::digest(data).to_vec(),
        "SHA-224" => sha2::Sha224::digest(data).to_vec(),
        "SHA-256" => sha2::Sha256::digest(data).to_vec(),
        "SHA-384" => sha2::Sha384::digest(data).to_vec(),
        "SHA-512" => sha2::Sha512::digest(data).to_vec(),
        _ => return None,
    })
}

/// `URLEncoder` (`+` for spaces), or RFC 3986 percent-encoding for `URL_ENCODE_COMPONENT`.
/// Both leave `~` and encode `*`, as Elasticsearch's do.
fn url_encode(s: &str, component: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' if !component => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `URLDecoder.decode(s, UTF_8)` with its error texts.
fn url_decode(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                if i + 2 >= b.len() {
                    return Err("java.lang.IllegalArgumentException: URLDecoder: Incomplete trailing escape (%) pattern".into());
                }
                let hexval = |c: u8| (c as char).to_digit(16);
                for &c in &b[i + 1..i + 3] {
                    if hexval(c).is_none() {
                        return Err(format!(
                            "java.lang.IllegalArgumentException: URLDecoder: Illegal hex characters in escape (%) pattern - not a hexadecimal digit: \"{}\" = {}",
                            c as char, c
                        ));
                    }
                }
                out.push((hexval(b[i + 1]).unwrap_or(0) * 16 + hexval(b[i + 2]).unwrap_or(0)) as u8);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DiffUnit {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
    Milli,
    Micro,
    Nano,
}

/// `DateDiff.Part` names and aliases.
fn diff_unit(u: &str) -> Option<DiffUnit> {
    Some(match u.to_ascii_lowercase().as_str() {
        "year" | "years" | "yy" | "yyyy" => DiffUnit::Year,
        "quarter" | "quarters" | "qq" | "q" => DiffUnit::Quarter,
        "month" | "months" | "mm" | "m" => DiffUnit::Month,
        "dayofyear" | "dy" | "y" | "day" | "days" | "dd" | "d" | "weekday" | "weekdays" | "dw" => DiffUnit::Day,
        "week" | "weeks" | "wk" | "ww" => DiffUnit::Week,
        "hour" | "hours" | "hh" => DiffUnit::Hour,
        "minute" | "minutes" | "mi" | "n" => DiffUnit::Minute,
        "second" | "seconds" | "ss" | "s" => DiffUnit::Second,
        "millisecond" | "milliseconds" | "ms" => DiffUnit::Milli,
        "microsecond" | "microseconds" | "mcs" => DiffUnit::Micro,
        "nanosecond" | "nanoseconds" | "ns" => DiffUnit::Nano,
        _ => return None,
    })
}

/// `ChronoUnit.between` on UTC date-times: calendar units count whole units.
fn date_diff(unit: DiffUnit, a: i64, b: i64) -> i64 {
    use chrono::{Datelike, Timelike};
    let months = |a: i64, b: i64| -> i64 {
        let (Some(x), Some(y)) = (chrono::DateTime::from_timestamp_millis(a), chrono::DateTime::from_timestamp_millis(b)) else { return 0 };
        let (x, y) = (x.naive_utc(), y.naive_utc());
        let mut m = (y.year() as i64 - x.year() as i64) * 12 + (y.month() as i64 - x.month() as i64);
        // Whole months only: the end's day-of-month and time must reach the start's.
        let rest = |d: chrono::NaiveDateTime| (d.day(), d.num_seconds_from_midnight(), d.nanosecond());
        if m > 0 && rest(y) < rest(x) {
            m -= 1;
        } else if m < 0 && rest(y) > rest(x) {
            m += 1;
        }
        m
    };
    let d = b - a;
    match unit {
        DiffUnit::Year => months(a, b) / 12,
        DiffUnit::Quarter => months(a, b) / 3,
        DiffUnit::Month => months(a, b),
        DiffUnit::Week => d / 604_800_000,
        DiffUnit::Day => d / 86_400_000,
        DiffUnit::Hour => d / 3_600_000,
        DiffUnit::Minute => d / 60_000,
        DiffUnit::Second => d / 1000,
        DiffUnit::Milli => d,
        DiffUnit::Micro => d.saturating_mul(1000),
        DiffUnit::Nano => d.saturating_mul(1_000_000),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Field {
    NanoOfSecond,
    NanoOfDay,
    MicroOfSecond,
    MicroOfDay,
    MilliOfSecond,
    MilliOfDay,
    SecondOfMinute,
    SecondOfDay,
    MinuteOfHour,
    MinuteOfDay,
    HourOfAmpm,
    ClockHourOfAmpm,
    HourOfDay,
    ClockHourOfDay,
    AmpmOfDay,
    DayOfWeek,
    AlignedDayOfWeekInMonth,
    AlignedDayOfWeekInYear,
    DayOfMonth,
    DayOfYear,
    EpochDay,
    AlignedWeekOfMonth,
    AlignedWeekOfYear,
    MonthOfYear,
    ProlepticMonth,
    YearOfEra,
    Year,
    Era,
    InstantSeconds,
    OffsetSeconds,
}

/// `ChronoField.valueOf(name.toUpperCase())`.
fn chrono_field(name: &str) -> Option<Field> {
    use Field::*;
    Some(match name.to_ascii_uppercase().as_str() {
        "NANO_OF_SECOND" => NanoOfSecond,
        "NANO_OF_DAY" => NanoOfDay,
        "MICRO_OF_SECOND" => MicroOfSecond,
        "MICRO_OF_DAY" => MicroOfDay,
        "MILLI_OF_SECOND" => MilliOfSecond,
        "MILLI_OF_DAY" => MilliOfDay,
        "SECOND_OF_MINUTE" => SecondOfMinute,
        "SECOND_OF_DAY" => SecondOfDay,
        "MINUTE_OF_HOUR" => MinuteOfHour,
        "MINUTE_OF_DAY" => MinuteOfDay,
        "HOUR_OF_AMPM" => HourOfAmpm,
        "CLOCK_HOUR_OF_AMPM" => ClockHourOfAmpm,
        "HOUR_OF_DAY" => HourOfDay,
        "CLOCK_HOUR_OF_DAY" => ClockHourOfDay,
        "AMPM_OF_DAY" => AmpmOfDay,
        "DAY_OF_WEEK" => DayOfWeek,
        "ALIGNED_DAY_OF_WEEK_IN_MONTH" => AlignedDayOfWeekInMonth,
        "ALIGNED_DAY_OF_WEEK_IN_YEAR" => AlignedDayOfWeekInYear,
        "DAY_OF_MONTH" => DayOfMonth,
        "DAY_OF_YEAR" => DayOfYear,
        "EPOCH_DAY" => EpochDay,
        "ALIGNED_WEEK_OF_MONTH" => AlignedWeekOfMonth,
        "ALIGNED_WEEK_OF_YEAR" => AlignedWeekOfYear,
        "MONTH_OF_YEAR" => MonthOfYear,
        "PROLEPTIC_MONTH" => ProlepticMonth,
        "YEAR_OF_ERA" => YearOfEra,
        "YEAR" => Year,
        "ERA" => Era,
        "INSTANT_SECONDS" => InstantSeconds,
        "OFFSET_SECONDS" => OffsetSeconds,
        _ => return None,
    })
}

fn extract(f: Field, ms: i64) -> i64 {
    use Field::*;
    use chrono::{Datelike, Timelike};
    let Some(dt) = chrono::DateTime::from_timestamp_millis(ms) else { return 0 };
    let dt = dt.naive_utc();
    let nanos_of_day = dt.num_seconds_from_midnight() as i64 * 1_000_000_000 + dt.nanosecond() as i64;
    match f {
        NanoOfSecond => dt.nanosecond() as i64,
        NanoOfDay => nanos_of_day,
        MicroOfSecond => dt.nanosecond() as i64 / 1000,
        MicroOfDay => nanos_of_day / 1000,
        MilliOfSecond => dt.nanosecond() as i64 / 1_000_000,
        MilliOfDay => nanos_of_day / 1_000_000,
        SecondOfMinute => dt.second() as i64,
        SecondOfDay => dt.num_seconds_from_midnight() as i64,
        MinuteOfHour => dt.minute() as i64,
        MinuteOfDay => dt.num_seconds_from_midnight() as i64 / 60,
        HourOfAmpm => dt.hour() as i64 % 12,
        ClockHourOfAmpm => {
            let h = dt.hour() as i64 % 12;
            if h == 0 { 12 } else { h }
        }
        HourOfDay => dt.hour() as i64,
        ClockHourOfDay => {
            let h = dt.hour() as i64;
            if h == 0 { 24 } else { h }
        }
        AmpmOfDay => (dt.hour() >= 12) as i64,
        DayOfWeek => dt.weekday().number_from_monday() as i64,
        AlignedDayOfWeekInMonth => (dt.day() as i64 - 1) % 7 + 1,
        AlignedDayOfWeekInYear => (dt.ordinal() as i64 - 1) % 7 + 1,
        DayOfMonth => dt.day() as i64,
        DayOfYear => dt.ordinal() as i64,
        EpochDay => ms.div_euclid(86_400_000),
        AlignedWeekOfMonth => (dt.day() as i64 - 1) / 7 + 1,
        AlignedWeekOfYear => (dt.ordinal() as i64 - 1) / 7 + 1,
        MonthOfYear => dt.month() as i64,
        ProlepticMonth => dt.year() as i64 * 12 + dt.month0() as i64,
        YearOfEra => {
            let y = dt.year() as i64;
            if y >= 1 { y } else { 1 - y }
        }
        Year => dt.year() as i64,
        Era => (dt.year() >= 1) as i64,
        InstantSeconds => ms.div_euclid(1000),
        OffsetSeconds => 0,
    }
}

/// `Math.round`-style rounding of a double to `digits` decimals (negative: tens, hundreds...),
/// half away from zero like Elasticsearch's `Maths.round`.
fn round_double(x: f64, digits: i64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let factor = 10f64.powi(digits.clamp(-308, 308) as i32);
    let scaled = x * factor;
    if !scaled.is_finite() {
        return x;
    }
    let r = scaled.abs().round() * scaled.signum();
    let out = r / factor;
    if out.is_finite() { out } else { x }
}

fn round_whole(x: i64, digits: i64) -> i64 {
    if digits >= 0 {
        return x;
    }
    let Some(p) = 10i64.checked_pow((-digits).min(18) as u32) else { return 0 };
    let r = (x.unsigned_abs() + (p as u64) / 2) / p as u64 * p as u64;
    if x < 0 { -(r as i64) } else { r as i64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(f: F, args: &[V], types: &[DataType]) -> V {
        eval(f, args, types).unwrap_or(V::Null)
    }

    #[test]
    fn names_resolve_with_aliases() {
        assert_eq!(resolve("to_dbl"), Some(F::ToDouble));
        assert_eq!(resolve("to_ul"), Some(F::ToUnsignedLong));
        assert_eq!(resolve("mv_sort"), Some(F::MvSort));
        assert_eq!(resolve("nope"), None);
        assert_eq!(F::ToDouble.name(), "to_double");
    }

    #[test]
    fn arity_errors_match_elasticsearch() {
        let e = check(F::Length, "length", "LENGTH()", (1, 22), &[]).unwrap_err();
        assert_eq!(e.reason(), "line 1:22: error building [length]: expects exactly one argument");
        let e = check(F::Substring, "substring", "SUBSTRING(\"abc\")", (1, 22), &[Arg { ty: DataType::Keyword, text: "\"abc\"", lit: None }]).unwrap_err();
        assert_eq!(e.reason(), "line 1:22: error building [substring]: expects two or three arguments");
    }

    #[test]
    fn type_errors_name_the_argument() {
        let a = [Arg { ty: DataType::Integer, text: "a", lit: None }];
        let e = check(F::Length, "LENGTH", "LENGTH(a)", (1, 22), &a).unwrap_err();
        assert_eq!(e.reason(), "Found 1 problem\nline 1:22: argument of [LENGTH(a)] must be [string], found value [a] type [integer]");
        let a = [Arg { ty: DataType::Integer, text: "i", lit: None }, Arg { ty: DataType::Boolean, text: "b", lit: None }];
        let e = check(F::Round, "round", "round(i, b)", (1, 1), &a).unwrap_err();
        assert_eq!(e.reason(), "Found 1 problem\nline 1:1: second argument of [round(i, b)] must be [whole number except unsigned_long or counter types], found value [b] type [boolean]");
        let a = [Arg { ty: DataType::Integer, text: "i", lit: None }, Arg { ty: DataType::Keyword, text: "s", lit: None }];
        let e = check(F::Coalesce, "coalesce", "coalesce(i, s)", (1, 1), &a).unwrap_err();
        assert_eq!(e.reason(), "Found 1 problem\nline 1:1: second argument of [coalesce(i, s)] must be [integer], found value [s] type [keyword]");
    }

    #[test]
    fn multi_values_warn_for_single_value_functions() {
        let m = V::Multi(vec![V::Str("ab".into()), V::Str("c".into())]);
        assert!(matches!(eval(F::Length, std::slice::from_ref(&m), &[DataType::Keyword]), Err(Fail::Warn(w)) if w == MULTI));
        assert_eq!(ev(F::ToUpper, &[m], &[DataType::Keyword]), V::Multi(vec![V::Str("AB".into()), V::Str("C".into())]));
    }

    #[test]
    fn mv_functions() {
        let a = V::Multi(vec![V::Int(3), V::Int(1), V::Int(2), V::Int(3)]);
        let t = [DataType::Integer];
        assert_eq!(ev(F::MvSort, std::slice::from_ref(&a), &t), V::Multi(vec![V::Int(1), V::Int(2), V::Int(3), V::Int(3)]));
        assert_eq!(ev(F::MvSort, &[a.clone(), V::Str("DESC".into())], &[DataType::Integer, DataType::Keyword]), V::Multi(vec![V::Int(3), V::Int(3), V::Int(2), V::Int(1)]));
        assert_eq!(ev(F::MvSlice, &[a.clone(), V::Int(1), V::Int(2)], &[DataType::Integer; 3]), V::Multi(vec![V::Int(1), V::Int(2)]));
        assert_eq!(ev(F::MvDedupe, std::slice::from_ref(&a), &t), V::Multi(vec![V::Int(3), V::Int(1), V::Int(2)]));
        assert_eq!(ev(F::MvMedian, std::slice::from_ref(&a), &t), V::Int(2));
        assert_eq!(ev(F::MvMedian, &[V::Multi(vec![V::Int(1), V::Int(2)])], &t), V::Int(1));
        assert_eq!(ev(F::MvSum, std::slice::from_ref(&a), &t), V::Int(9));
        assert_eq!(ev(F::MvAvg, std::slice::from_ref(&a), &t), V::Double(2.25));
        assert!(matches!(eval(F::MvSum, &[V::Multi(vec![V::Int(i32::MAX), V::Int(1)])], &t), Err(Fail::Warn(_))));
        let z = ev(F::MvZip, &[V::Multi(vec![V::Str("x".into()), V::Str("y".into())]), V::Multi(vec![V::Str("1".into()), V::Str("2".into()), V::Str("3".into())]), V::Str("-".into())], &[DataType::Keyword; 3]);
        assert_eq!(z, V::Multi(vec![V::Str("x-1".into()), V::Str("y-2".into()), V::Str("3".into())]));
        let w = ev(F::MvPseriesWeightedSum, &[V::Multi(vec![V::Double(1.5), V::Double(2.5)]), V::Double(1.5)], &[DataType::Double; 2]);
        assert_eq!(w, V::Double(2.3838834764831844));
    }

    #[test]
    fn math_edges() {
        assert!(matches!(eval(F::Sqrt, &[V::Int(-1)], &[DataType::Integer]), Err(Fail::Warn(w)) if w == "java.lang.ArithmeticException: Square root of negative"));
        assert!(matches!(eval(F::Abs, &[V::Int(i32::MIN)], &[DataType::Integer]), Err(Fail::Warn(_))));
        assert_eq!(ev(F::Round, &[V::Double(-2.5)], &[DataType::Double]), V::Double(-3.0));
        assert_eq!(ev(F::Round, &[V::Double(1.255), V::Int(2)], &[DataType::Double, DataType::Integer]), V::Double(1.25));
        assert_eq!(ev(F::Round, &[V::Int(1234), V::Int(-2)], &[DataType::Integer, DataType::Integer]), V::Int(1200));
        assert_eq!(ev(F::RoundTo, &[V::Int(7), V::Int(2), V::Int(6)], &[DataType::Integer; 3]), V::Int(6));
        assert_eq!(ev(F::RoundTo, &[V::Int(0), V::Int(2), V::Int(6)], &[DataType::Integer; 3]), V::Int(2));
        assert_eq!(ev(F::Clamp, &[V::Int(7), V::Double(1.0), V::Int(5)], &[DataType::Integer, DataType::Double, DataType::Integer]), V::Double(5.0));
        assert_eq!(ev(F::CopySign, &[V::Int(3), V::Double(-1.0)], &[DataType::Integer, DataType::Double]), V::Int(-3));
        assert_eq!(ev(F::Scalb, &[V::Int(3), V::Int(2)], &[DataType::Integer; 2]), V::Double(12.0));
        assert_eq!(ev(F::Signum, &[V::Int(-5)], &[DataType::Integer]), V::Double(-1.0));
    }

    #[test]
    fn strings() {
        let s = |x: &str| V::Str(x.into());
        let k = [DataType::Keyword; 3];
        assert_eq!(ev(F::UrlEncode, &[s("a b&c/é~*")], &k), s("a+b%26c%2F%C3%A9~%2A"));
        assert_eq!(ev(F::UrlEncodeComponent, &[s("a b&c/é~*")], &k), s("a%20b%26c%2F%C3%A9~%2A"));
        assert_eq!(ev(F::UrlDecode, &[s("a%20b+c")], &k), s("a b c"));
        assert_eq!(ev(F::Md5, &[s("Hello World")], &k), s("b10a8db164e0754105b7a99be72e3fe5"));
        assert_eq!(ev(F::Sha1, &[s("Hello World")], &k), s("0a4d55a8d778e5022fab701977c5d840bbc486d0"));
        assert_eq!(ev(F::Replace, &[s("abc"), s("(b)"), s("[$1]")], &k), s("a[b]c"));
        assert_eq!(ev(F::Trim, &[s("\u{a0} x \t")], &k), s("\u{a0} x"));
        assert_eq!(ev(F::Reverse, &[s("ab🙂c")], &k), s("c🙂ba"));
        assert_eq!(ev(F::Substring, &[s("abc"), V::Int(0), V::Int(2)], &k), s("ab"));
        assert_eq!(ev(F::Substring, &[s("abc"), V::Int(-2)], &k), s("bc"));
        assert_eq!(ev(F::Locate, &[s("abcabc"), s("c"), V::Int(4)], &k), V::Int(6));
        assert_eq!(ev(F::ToBase64, &[s("a")], &k), s("YQ=="));
        assert_eq!(ev(F::FromBase64, &[s("YQ==")], &k), s("a"));
    }

    #[test]
    fn dates() {
        let d = V::Date(1710498030123); // 2024-03-15T10:20:30.123Z, a Friday
        let e = V::Date(1675123200000); // 2023-01-31T00:00:00Z
        let t = [DataType::Keyword, DataType::Date, DataType::Date];
        assert_eq!(ev(F::DateDiff, &[V::Str("month".into()), e.clone(), d.clone()], &t), V::Int(13));
        assert_eq!(ev(F::DateDiff, &[V::Str("year".into()), e.clone(), d.clone()], &t), V::Int(1));
        assert_eq!(ev(F::DateDiff, &[V::Str("quarter".into()), e.clone(), d.clone()], &t), V::Int(4));
        assert_eq!(ev(F::DateDiff, &[V::Str("dy".into()), e.clone(), d.clone()], &t), V::Int(409));
        assert!(matches!(eval(F::DateDiff, &[V::Str("ns".into()), e, d.clone()], &t), Err(Fail::Warn(_))));
        assert_eq!(ev(F::DayName, std::slice::from_ref(&d), &[DataType::Date]), V::Str("Friday".into()));
        assert_eq!(ev(F::MonthName, std::slice::from_ref(&d), &[DataType::Date]), V::Str("March".into()));
        assert_eq!(ev(F::DateExtract, &[V::Str("aligned_week_of_year".into()), d.clone()], &t), V::Long(11));
        assert_eq!(ev(F::DateExtract, &[V::Str("epoch_day".into()), d], &t), V::Long(19797));
    }

    #[test]
    fn ips_and_versions() {
        assert_eq!(ev(F::IpPrefix, &[V::Ip("2001:db8::ff00:42:8329".into()), V::Int(24), V::Int(32)], &[DataType::Ip, DataType::Integer, DataType::Integer]), V::Ip("2001:db8::".into()));
        assert_eq!(ev(F::IpPrefix, &[V::Ip("192.168.1.5".into()), V::Int(24), V::Int(0)], &[DataType::Ip, DataType::Integer, DataType::Integer]), V::Ip("192.168.1.0".into()));
        let mut v = vec!["2.0.0", "1.10.0", "0.9", "1.2.3", "1.2.3-alpha", "nope"];
        v.sort_by(|a, b| version_cmp(a, b));
        assert_eq!(v, ["0.9", "1.2.3-alpha", "1.2.3", "1.10.0", "2.0.0", "nope"]);
    }
}
