//! OpenSearch's k-NN plugin (bundled with every OpenSearch distribution): the `knn_vector`
//! field type and the `knn` query of the opensearch dialect.
//!
//! A `knn_vector` is roll's vector field under OpenSearch's rules:
//! - **Mapping** ([`normalize_mapping`]): `dimension`, `data_type` (`float`, `byte`, `binary`,
//!   `half_float`), `space_type` (`l2`, `cosinesimil`, `innerproduct`, `hamming`; `l1`/`linf`
//!   only on the retired nmslib engine), `method` (`hnsw` on `faiss` — the default — or
//!   `lucene`), `mode` and `compression_level`, validated in `KNNVectorFieldMapper.TypeParser`'s
//!   order with its messages, and kept in the form OpenSearch prints (`data_type` upper case,
//!   `method` resolved to `{engine, space_type, name, parameters}`). Without `index.knn` the
//!   field is flat: stored for scripts, not searchable by `knn`.
//! - **Scores** ([`Space::score`]): `l2` is `1 / (1 + d²)`, `cosinesimil` `(1 + cos) / 2`,
//!   `innerproduct` `ip + 1` (or `1 / (1 - ip)` below 0), `hamming` `1 / (1 + bits)`. The first
//!   three are the codec's own similarities; hamming is the codec's bit similarity, whose
//!   `(bits - h) / bits` score is mapped back to OpenSearch's (`OsKnnQuery`). `l1` and `linf`
//!   (only reachable through a top-level `engine: nmslib`) are ranked by L2.
//! - **Search** ([`to_query`]): the native engines search segment by segment and keep each
//!   segment's best `k` (a shard answers up to `k` hits per segment); the lucene engine and
//!   `rescore` keep the best `k` overall. `max_distance` / `min_score` are radial searches: every
//!   vector within the threshold, scored exactly.
//! - **Source** ([`derive_source`]): with `index.knn` (and `index.knn.derived_source.enabled`,
//!   on by default) `_source` vectors come back as the plugin stores them: floats (normalized
//!   for `cosinesimil`, rounded to half precision for `half_float`), or integers for byte and
//!   binary vectors.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::mapping::{FieldMapper, MappingCtx};
use crate::query::xparser::{Field, Tokens};
use crate::query::{Common, Query, Token, XParser, check_boost, parse_inner};
use crate::search::query::QueryCtx;
use search_codec::{HnswParams, VectorElement, VectorQuantization, VectorSimilarity, VectorSpec};
use search_index::query::{KnnQuery as IndexKnn, QueryVector};
use search_index::{QueryRef, Searcher};
use std::any::Any;
use std::sync::Arc;

/// The plugin's largest `k` (`KNNQueryBuilder.K_MAX`).
const K_MAX: i32 = 10_000;
/// Largest dimension the faiss and lucene engines index.
const MAX_DIMENSION: i64 = 16_000;

/// `SpaceType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    L2,
    Cosine,
    InnerProduct,
    L1,
    Linf,
    Hamming,
}

impl Space {
    const NAMES: &'static str = "[l2, cosinesimil, l1, linf, innerproduct, hamming]";

    pub fn parse(s: &str) -> Option<Space> {
        Some(match s {
            "l2" => Space::L2,
            "cosinesimil" => Space::Cosine,
            "innerproduct" => Space::InnerProduct,
            "l1" => Space::L1,
            "linf" => Space::Linf,
            "hamming" => Space::Hamming,
            _ => return None,
        })
    }

    /// `SpaceType.getSpace`.
    fn parse_or_err(s: &str) -> EsResult<Space> {
        Space::parse(s).ok_or_else(|| EsError::illegal_argument(format!("Unable to find space: {s} . Valid values are: {}", Space::NAMES)))
    }

    pub fn name(self) -> &'static str {
        match self {
            Space::L2 => "l2",
            Space::Cosine => "cosinesimil",
            Space::InnerProduct => "innerproduct",
            Space::L1 => "l1",
            Space::Linf => "linf",
            Space::Hamming => "hamming",
        }
    }

    /// The enum constant's name (messages print `space type: L2`).
    fn constant(self) -> &'static str {
        match self {
            Space::L2 => "L2",
            Space::Cosine => "COSINESIMIL",
            Space::InnerProduct => "INNER_PRODUCT",
            Space::L1 => "L1",
            Space::Linf => "LINF",
            Space::Hamming => "HAMMING",
        }
    }

    /// The score OpenSearch gives a raw engine distance (`SpaceType.scoreTranslation`): `raw` is
    /// the squared L2 distance, `1 - cos`, `-ip` or the bit distance.
    pub fn score(self, raw: f32) -> f32 {
        match self {
            Space::Cosine => ((2.0 - raw) / 2.0).max(0.0),
            Space::InnerProduct if raw < 0.0 => -raw + 1.0,
            _ => 1.0 / (1.0 + raw),
        }
    }
}

/// `VectorDataType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    Float,
    Byte,
    Binary,
    HalfFloat,
}

impl DataType {
    fn parse(s: &str) -> EsResult<DataType> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "float" => DataType::Float,
            "byte" => DataType::Byte,
            "binary" => DataType::Binary,
            "half_float" => DataType::HalfFloat,
            _ => return Err(EsError::illegal_argument("Invalid value provided for [data_type] field. Supported values are [binary,byte,float,half_float]")),
        })
    }

    fn constant(self) -> &'static str {
        match self {
            DataType::Float => "FLOAT",
            DataType::Byte => "BYTE",
            DataType::Binary => "BINARY",
            DataType::HalfFloat => "HALF_FLOAT",
        }
    }

    fn value(self) -> &'static str {
        match self {
            DataType::Float => "float",
            DataType::Byte => "byte",
            DataType::Binary => "binary",
            DataType::HalfFloat => "half_float",
        }
    }
}

/// The engines (`KNNEngine`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Faiss,
    Lucene,
    Nmslib,
}

impl Engine {
    fn parse(s: &str) -> Option<Engine> {
        Some(match s.to_ascii_lowercase().as_str() {
            "faiss" => Engine::Faiss,
            "lucene" => Engine::Lucene,
            "nmslib" => Engine::Nmslib,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Engine::Faiss => "faiss",
            Engine::Lucene => "lucene",
            Engine::Nmslib => "nmslib",
        }
    }

    fn supports(self, space: Space) -> bool {
        match self {
            Engine::Faiss => matches!(space, Space::L2 | Space::Cosine | Space::InnerProduct | Space::Hamming),
            Engine::Lucene => matches!(space, Space::L2 | Space::Cosine | Space::InnerProduct | Space::Hamming),
            Engine::Nmslib => matches!(space, Space::L2 | Space::Cosine | Space::InnerProduct | Space::L1 | Space::Linf),
        }
    }

    /// The `hnsw` method parameters the engine takes.
    fn parameters(self) -> &'static [&'static str] {
        match self {
            Engine::Lucene => &["m", "ef_construction", "encoder"],
            _ => &["m", "ef_construction", "ef_search", "encoder"],
        }
    }
}

/// A `knn_vector` field's resolved configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Resolved {
    pub dims: u32,
    pub data: DataType,
    pub space: Space,
    pub engine: Engine,
    /// `index.knn`: an approximate (graph) index exists.
    pub ann: bool,
    pub m: u16,
    pub ef_construction: u16,
}

fn text(v: &Value) -> String {
    v.to_es_string()
}

fn validation(errors: &[String]) -> EsError {
    let mut msg = String::from("Validation Failed: ");
    for (i, e) in errors.iter().enumerate() {
        msg.push_str(&format!("{}: {e};", i + 1));
    }
    // `ValidationException` is an `IllegalArgumentException`: the mapping error stays the root.
    EsError::java(400, "validation_exception", msg)
}

/// `XContentMapValues.nodeIntegerValue` for `dimension`: integers and integer strings.
fn dimension_of(v: &Value, field: &str) -> EsResult<i64> {
    let bad = || EsError::illegal_argument(format!("Unable to parse [dimension] from provided value [{}] for vector [{field}]", text(v)));
    let n = match v {
        Value::Num(n) => {
            let d = n.as_f64();
            if d.fract() != 0.0 || !d.is_finite() {
                return Err(bad());
            }
            d as i64
        }
        Value::Str(s) => s.trim().parse::<i64>().map_err(|_| bad())?,
        _ => return Err(bad()),
    };
    if n > i32::MAX as i64 || n < i32::MIN as i64 {
        return Err(bad());
    }
    Ok(n)
}

/// `KNNMethodContext.parse` plus the resolution `resolveKNNMethodComponents` does: the method
/// as OpenSearch prints it, or the parse error.
struct Method {
    name: String,
    engine: Option<Engine>,
    space: Option<Space>,
    /// As given, in the order of the `HashMap` they were parsed into.
    parameters: Map,
}

fn parse_method(v: &Value) -> EsResult<Method> {
    let Value::Obj(m) = v else {
        return Err(EsError::mapper_parsing("Unable to parse mapping into KNNMethodContext. Object not of type \"Map\""));
    };
    let mut out = Method { name: String::new(), engine: None, space: None, parameters: Map::new() };
    let mut name = None;
    for (k, v) in m {
        match k.as_str() {
            "engine" => {
                let s = text(v);
                out.engine = Some(Engine::parse(&s).ok_or_else(|| EsError::mapper_parsing(format!("Invalid engine: {s}")))?);
            }
            "space_type" => {
                let s = text(v);
                out.space = Some(Space::parse(&s).ok_or_else(|| EsError::mapper_parsing(format!("Invalid space_type: {s}")))?);
            }
            "name" => name = Some(text(v)),
            "parameters" => match v {
                Value::Obj(p) => {
                    let keys: Vec<&str> = p.keys().map(String::as_str).collect();
                    for k in super::java_hashmap_order(&keys, p.len()) {
                        if let Some(v) = p.get(k) {
                            out.parameters.insert(k.to_string(), v.clone());
                        }
                    }
                }
                Value::Null => {}
                _ => return Err(EsError::mapper_parsing("Unable to parse parameters for main method component")),
            },
            other => return Err(EsError::mapper_parsing(format!("Invalid parameter: {other}"))),
        }
    }
    out.name = name.ok_or_else(|| EsError::mapper_parsing("name needs to be set"))?;
    if !matches!(out.name.as_str(), "hnsw" | "ivf") {
        return Err(EsError::illegal_argument(format!("Invalid method name: {}", out.name)));
    }
    Ok(out)
}

/// Parses, validates and normalizes a `knn_vector` mapping (`KNNVectorFieldMapper.TypeParser`).
pub fn normalize_mapping(f: &mut FieldMapper, ctx: &MappingCtx) -> EsResult<()> {
    let field = f.name.clone();
    // Parameter parsing (`Parameter.parse` and validators), in parameter order.
    let dims = match f.raw("dimension") {
        Some(v) => {
            let n = dimension_of(v, &field)?;
            if n <= 0 {
                return Err(EsError::illegal_argument(format!("Dimension value must be greater than 0 for vector: {field}")));
            }
            f.set("dimension", Value::from(n));
            Some(n)
        }
        None => None,
    };
    let data = match f.raw("data_type").filter(|v| !v.is_null()) {
        Some(v) => DataType::parse(&text(v))?,
        None => DataType::Float,
    };
    // The default `float` isn't printed; the others print as the enum constant.
    f.set("data_type", if data == DataType::Float { Value::Null } else { Value::from(data.constant()) });
    let method = match f.raw("method").filter(|v| !v.is_null()) {
        Some(v) => Some(parse_method(v)?),
        None => None,
    };
    let model = f.raw("model_id").filter(|v| !v.is_null()).map(text);
    let restricted = |name: &str, options: &[&str], shown: &str| -> EsResult<Option<String>> {
        let Some(v) = f.raw(name).filter(|v| !v.is_null()) else { return Ok(None) };
        let s = text(v);
        if !options.contains(&s.as_str()) {
            return Err(EsError::mapper_parsing(format!("Unknown value [{s}] for field [{name}] - accepted values are [{shown}]")));
        }
        Ok(Some(s))
    };
    let mode = restricted("mode", &["in_memory", "on_disk"], "null, in_memory, on_disk")?;
    let compression = restricted("compression_level", &["1x", "2x", "4x", "8x", "16x", "32x"], ", 1x, 2x, 4x, 8x, 16x, 32x")?;
    let top_space = match f.raw("space_type").filter(|v| !v.is_null()) {
        Some(v) => Some(Space::parse_or_err(&text(v))?),
        None => None,
    };
    let top_engine = match f.raw("engine").filter(|v| !v.is_null()) {
        Some(v) => {
            let s = text(v);
            Some(Engine::parse(&s).ok_or_else(|| {
                EsError::illegal_argument(format!(
                    "Invalid engine type: {s}. If an engine definition for this name exists, it may have failed to load; check startup warnings."
                ))
            })?)
        }
        None => None,
    };
    // Build-time validation.
    if method.is_some() && model.is_some() {
        return Err(EsError::illegal_argument(format!("Method and model can not be both specified in the mapping: {field}")));
    }
    if dims.is_some() && model.is_some() {
        return Err(EsError::illegal_argument(format!("Cannot specify both a modelId and dimension in the mapping: {field}")));
    }
    let missing_dims = || EsError::illegal_argument(format!("Dimension value missing for vector: {field}"));
    if !ctx.knn {
        if model.is_some() || method.is_some() {
            return Err(EsError::illegal_argument("Cannot set modelId or method parameters when index.knn setting is false"));
        }
        if dims.is_none() {
            return Err(missing_dims());
        }
        if mode.is_some() || compression.is_some() {
            return Err(EsError::mapper_parsing(format!("Compression and mode can not be specified in a flat mapping configuration for field: {field}")));
        }
        // `FlatVectorFieldMapper` always keeps (and prints) doc values.
        f.set("doc_values", Value::Bool(true));
        return Ok(());
    }
    if model.is_some() {
        if top_space.is_some() {
            return Err(EsError::illegal_argument("TopLevel Space type and model can not be both specified in the mapping"));
        }
        if mode.is_some() || compression.is_some() {
            return Err(EsError::mapper_parsing(format!("Compression and mode can not be specified in a model mapping configuration for field: {field}")));
        }
        return Ok(());
    }
    let float = matches!(data, DataType::Float | DataType::HalfFloat);
    if mode.is_some() && !float {
        return Err(EsError::mapper_parsing(format!("Mode cannot be used for non-float data type for field {field}")));
    }
    if compression.is_some() && !float {
        return Err(EsError::mapper_parsing(format!("Compression cannot be used for non-float data type for field {field}")));
    }
    // `SpaceTypeResolver.resolveSpaceType`.
    let space = match (method.as_ref().and_then(|m| m.space), top_space) {
        (None, None) if data == DataType::Binary => Space::Hamming,
        (None, None) => Space::L2,
        (Some(s), None) | (None, Some(s)) => s,
        (Some(a), Some(b)) if a == b => a,
        (Some(a), Some(b)) => {
            return Err(EsError::mapper_parsing(format!("Cannot specify conflicting space types: \"[{}]\" \"[{}]\"", a.name(), b.name())));
        }
    };
    // `SpaceType.validateVectorDataType`.
    if (space == Space::Hamming) != (data == DataType::Binary) {
        return Err(EsError::illegal_argument(format!("Space type [{}] is not supported with [{}] data type", space.name(), data.value())));
    }
    let engine = match (method.as_ref().and_then(|m| m.engine), top_engine) {
        (Some(a), Some(b)) if a != b => {
            return Err(EsError::mapper_parsing(format!("Cannot specify conflicting engines: [{}] and [{}]", a.name(), b.name())));
        }
        (Some(e), _) | (None, Some(e)) => e,
        (None, None) => Engine::Faiss,
    };
    if let Some(m) = &method {
        if m.engine == Some(Engine::Nmslib) {
            return Err(EsError::illegal_argument(
                "nmslib engine is deprecated in OpenSearch  and cannot be used for new index creation in OpenSearch from  3.0.0.",
            ));
        }
        if m.name == "ivf" {
            return Err(validation(&["\"method\" requires training.".into()]));
        }
    }
    let mut errors = Vec::new();
    if let Some(d) = dims {
        if d > MAX_DIMENSION {
            errors.push(format!("Dimension value cannot be greater than {MAX_DIMENSION} for vector with engine: {}", engine.name()));
        } else if data == DataType::Binary && d % 8 != 0 {
            errors.push("Dimension should be multiply of 8 for binary vector data type".into());
        }
    }
    if !engine.supports(space) {
        errors.push(format!("\"hnsw\" with \"{}\" configuration does not support space type: \"{}\".", engine.name(), space.name()));
    }
    if let Some(m) = &method {
        for (k, v) in &m.parameters {
            if !engine.parameters().contains(&k.as_str()) {
                errors.push(format!("Unknown parameter '{k}' found"));
            } else if k != "encoder" {
                match v.as_i64().filter(|_| matches!(v, Value::Num(n) if n.as_f64().fract() == 0.0)) {
                    None => errors.push(format!("value is not an instance of Integer for Integer parameter [{k}].")),
                    Some(n) if n <= 0 => errors.push(format!("parameter validation failed for Integer parameter [{k}].")),
                    _ => {}
                }
            }
        }
    }
    if !errors.is_empty() {
        return Err(validation(&errors));
    }
    if dims.is_none() {
        return Err(missing_dims());
    }
    if let Some(m) = method {
        let mut out = Map::new();
        out.insert("engine".into(), Value::from(engine.name()));
        out.insert("space_type".into(), Value::from(space.name()));
        out.insert("name".into(), Value::from(m.name));
        out.insert("parameters".into(), Value::Obj(m.parameters));
        f.set("method", Value::Obj(out));
    }
    Ok(())
}

/// A `knn_vector` mapping's resolved configuration (after [`normalize_mapping`]).
pub fn resolve(f: &FieldMapper, ctx: &MappingCtx) -> Resolved {
    let dims = f.raw("dimension").and_then(Value::as_i64).unwrap_or(0).max(0) as u32;
    let data = f.raw("data_type").filter(|v| !v.is_null()).and_then(|v| DataType::parse(&text(v)).ok()).unwrap_or(DataType::Float);
    let method = f.raw("method").filter(|v| !v.is_null());
    let space = method
        .and_then(|m| m.get("space_type"))
        .or_else(|| f.raw("space_type"))
        .and_then(|v| Space::parse(&text(v)))
        .unwrap_or(if data == DataType::Binary { Space::Hamming } else { Space::L2 });
    let engine = method.and_then(|m| m.get("engine")).or_else(|| f.raw("engine")).and_then(|v| Engine::parse(&text(v))).unwrap_or(Engine::Faiss);
    let param = |k: &str, d: u16| method.and_then(|m| m.get("parameters")).and_then(|p| p.get(k)).and_then(Value::as_i64).map_or(d, |n| n.clamp(1, u16::MAX as i64) as u16);
    Resolved { dims, data, space, engine, ann: ctx.knn, m: param("m", 16), ef_construction: param("ef_construction", 100) }
}

/// The codec layout of a `knn_vector`: the plugin's spaces are the codec's similarities
/// (hamming is the bit similarity; `l1` / `linf` rank by L2). Without `index.knn` the field
/// has no graph.
pub fn vector_spec(f: &FieldMapper, ctx: &MappingCtx) -> VectorSpec {
    let r = resolve(f, ctx);
    let element = match r.data {
        DataType::Byte => VectorElement::Byte,
        DataType::Binary => VectorElement::Bit,
        DataType::Float | DataType::HalfFloat => VectorElement::Float32,
    };
    // Cosine vectors are stored normalized (and queries normalized), so the score is the dot
    // product's `(1 + ip) / 2`, in the plugin's float arithmetic.
    let similarity = match r.space {
        Space::Cosine if r.data != DataType::Byte => VectorSimilarity::DotProduct,
        Space::Cosine => VectorSimilarity::Cosine,
        Space::InnerProduct => VectorSimilarity::MaxInnerProduct,
        _ => VectorSimilarity::L2Norm,
    };
    let hnsw = r.ann.then_some(HnswParams { m: r.m, ef_construction: r.ef_construction });
    VectorSpec { dims: r.dims, element, similarity, hnsw, quantization: VectorQuantization::None }
}

/// The Elasticsearch similarity name of a `knn_vector`'s space (what roll's vector code keys on).
pub fn similarity_name(f: &FieldMapper, ctx: &MappingCtx) -> Option<String> {
    Some(
        match resolve(f, ctx).space {
            Space::Cosine => "cosine",
            Space::InnerProduct => "max_inner_product",
            _ => "l2_norm",
        }
        .to_string(),
    )
}

/// One document value of a `knn_vector`, checked as `KNNVectorFieldMapper.parseCreateField`
/// does. `values` are the array's numbers (already parsed); returns the vector the codec
/// stores (floats, or signed bytes for byte and binary vectors).
pub enum DocVector {
    Floats(Vec<f32>),
    Bytes(Vec<i8>),
}

/// Validates the numbers of one vector. Errors carry the index of the offending element (for
/// the preview), or `None` when the vector as a whole is wrong.
pub fn check_doc_vector(r: &Resolved, values: &[f64]) -> Result<DocVector, (Option<usize>, EsError)> {
    let arg = |m: String| EsError::illegal_argument(m);
    match r.data {
        DataType::Byte | DataType::Binary => {
            let mut out = Vec::with_capacity(values.len());
            for (i, v) in values.iter().enumerate() {
                if v.is_nan() {
                    return Err((Some(i), arg("KNN vector values cannot be NaN".into())));
                }
                if v.is_infinite() {
                    return Err((Some(i), arg("KNN vector values cannot be infinity".into())));
                }
                let name = r.data.value();
                if v.fract() != 0.0 {
                    return Err((Some(i), arg(format!("[data_type] field was set as [{name}] in index mapping. But, KNN vector values are floats instead of byte integers"))));
                }
                if !(-128.0..=127.0).contains(v) {
                    return Err((
                        Some(i),
                        arg(format!("[data_type] field was set as [{name}] in index mapping. But, KNN vector values are not within in the byte range [-128, 127]")),
                    ));
                }
                out.push(*v as i8);
            }
            let given = if r.data == DataType::Binary { out.len() * 8 } else { out.len() };
            if given != r.dims as usize {
                return Err((
                    None,
                    arg(if r.data == DataType::Binary {
                        format!("The dimension of the binary vector must be 8 times the length of the provided vector. Expected: {}, Given: {given}", r.dims)
                    } else {
                        format!("Vector dimension mismatch. Expected: {}, Given: {given}", r.dims)
                    }),
                ));
            }
            Ok(DocVector::Bytes(out))
        }
        DataType::Float | DataType::HalfFloat => {
            let mut out = Vec::with_capacity(values.len());
            for (i, v) in values.iter().enumerate() {
                let f = *v as f32;
                if f.is_nan() {
                    return Err((Some(i), arg("KNN vector values cannot be NaN".into())));
                }
                if f.is_infinite() {
                    return Err((Some(i), arg("KNN vector values cannot be infinity".into())));
                }
                if r.data == DataType::HalfFloat && f.abs() > 65504.0 {
                    return Err((
                        Some(i),
                        arg("[data_type] field was set as HALF_FLOAT in index mapping. But, KNN vector values are not within in the half_float range [-65504.000000, 65504.000000]".into()),
                    ));
                }
                out.push(if r.data == DataType::HalfFloat { half(f) } else { f });
            }
            if out.len() != r.dims as usize {
                return Err((None, arg(format!("Vector dimension mismatch. Expected: {}, Given: {}", r.dims, out.len()))));
            }
            if r.space == Space::Cosine {
                if out.iter().all(|x| *x == 0.0) {
                    return Err((None, arg("zero vector is not supported when space type is [cosinesimil]".into())));
                }
                normalize(&mut out);
            }
            Ok(DocVector::Floats(out))
        }
    }
}

/// `f` rounded to the nearest half-precision float (ties to even), as a float.
pub fn half(f: f32) -> f32 {
    let bits = f.to_bits();
    let sign = bits & 0x8000_0000;
    let abs = f32::from_bits(bits & 0x7fff_ffff);
    if abs.is_nan() || abs.is_infinite() {
        return f;
    }
    // Half precision keeps 10 mantissa bits; below 2^-14 the spacing is fixed at 2^-24.
    let exp = ((bits >> 23) & 0xff) as i32 - 127;
    let r = if exp < -14 {
        let q = 2f32.powi(-24);
        (abs / q).round_ties_even() * q
    } else {
        let shift = 13u32;
        let m = bits & 0x7fff_ffff;
        let rem = m & ((1 << shift) - 1);
        let half_ulp = 1u32 << (shift - 1);
        let mut t = m >> shift;
        if rem > half_ulp || (rem == half_ulp && t & 1 == 1) {
            t += 1;
        }
        f32::from_bits(t << shift)
    };
    f32::from_bits(r.to_bits() | sign)
}

/// L2-normalizes in place (`NormalizeVectorTransformer`), in float arithmetic.
/// The squared L2 distance as faiss's `fvec_L2sqr` computes it: differences accumulated in
/// order, each with one fused multiply-add.
fn l2_squared(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).fold(0.0f32, |acc, (x, y)| {
        let t = x - y;
        t.mul_add(t, acc)
    })
}

/// Lucene's `VectorUtil.l2normalize` (what the plugin normalizes with): a vector already of
/// unit length (within 1e-5) is left as it is.
fn normalize(v: &mut [f32]) {
    let sum = v.iter().map(|x| x * x).sum::<f32>();
    if (f64::from(sum) - 1.0).abs() <= 1e-5 {
        return;
    }
    let norm = sum.sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// A Java `float` as the JSON number Jackson prints for it.
fn float_json(f: f32) -> Value {
    Value::from(crate::json::write::java_float(f).parse::<f64>().unwrap_or(f as f64))
}

/// Whether an index derives `knn_vector` values in `_source` from the stored vectors.
pub fn derives_source(idx: &crate::meta::IndexMeta) -> bool {
    idx.dialect == crate::cluster::Dialect::OpenSearch
        && idx.settings.get("index.knn").is_some_and(|v| v.eq_ignore_ascii_case("true"))
        && idx.settings.get("index.knn.derived_source.enabled").is_none_or(|v| !v.eq_ignore_ascii_case("false"))
}

/// Rewrites the `knn_vector` values of a document's `_source` the way OpenSearch derives them
/// (see the module docs). Returns whether anything changed.
pub fn derive_source(idx: &crate::meta::IndexMeta, doc: &mut Map) -> bool {
    let ctx = idx.mapping_ctx();
    let mut changed = false;
    derive_in(idx, &ctx, doc, "", &mut changed);
    changed
}

fn derive_in(idx: &crate::meta::IndexMeta, ctx: &MappingCtx, m: &mut Map, prefix: &str, changed: &mut bool) {
    for (k, v) in m.iter_mut() {
        let path = format!("{prefix}{k}");
        match idx.mapping.lookup(&path) {
            Some(crate::mapping::MapperRef::Field(f)) if f.ty == "knn_vector" => {
                if let Some(nv) = derived_value(&resolve(f, ctx), v) {
                    *v = nv;
                    *changed = true;
                }
            }
            _ => match v {
                Value::Obj(inner) => derive_in(idx, ctx, inner, &format!("{path}."), changed),
                Value::Arr(items) => {
                    for it in items {
                        if let Value::Obj(inner) = it {
                            derive_in(idx, ctx, inner, &format!("{path}."), changed);
                        }
                    }
                }
                _ => {}
            },
        }
    }
}

fn derived_value(r: &Resolved, v: &Value) -> Option<Value> {
    let Value::Arr(items) = v else { return None };
    let mut nums = Vec::with_capacity(items.len());
    for it in items {
        nums.push(match it {
            Value::Num(n) => n.as_f64(),
            Value::Str(s) => s.trim().parse::<f64>().ok()?,
            _ => return None,
        });
    }
    match check_doc_vector(r, &nums).ok()? {
        DocVector::Floats(f) => Some(Value::Arr(f.into_iter().map(float_json).collect())),
        DocVector::Bytes(b) => Some(Value::Arr(b.into_iter().map(|x| Value::from(x as i64)).collect())),
    }
}

/// The `knn` query (`KNNQueryBuilder`): `{"knn": {"<field>": {...}}}`.
#[derive(Clone, Debug, PartialEq)]
pub struct OsKnn {
    pub field: String,
    pub vector: Vec<f32>,
    pub k: Option<i32>,
    pub max_distance: Option<f32>,
    pub min_score: Option<f32>,
    pub filter: Option<Query>,
    pub method_parameters: Option<Map>,
    pub rescore: Option<Value>,
    pub expand_nested: Option<bool>,
    pub ignore_unmapped: bool,
    pub common: Common,
}

impl OsKnn {
    /// `KNNQueryBuilderParser.fromXContent`.
    pub fn parse(p: &mut XParser<'_>) -> EsResult<OsKnn> {
        const FIELDS: &[Field] = &[
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
            Field::new("vector", Tokens::FLOAT_ARRAY),
            Field::new("k", Tokens::INT),
            Field::new("ignore_unmapped", Tokens::BOOLEAN),
            Field::new("max_distance", Tokens::FLOAT),
            Field::new("min_score", Tokens::FLOAT),
            Field::new("method_parameters", Tokens::OBJECT),
            Field::new("filter", Tokens::OBJECT),
            Field::new("rescore", Tokens::OBJECT_OR_BOOLEAN),
            Field::new("expand_nested_docs", Tokens::BOOLEAN),
        ];
        let mut q = OsKnn {
            field: String::new(),
            vector: Vec::new(),
            k: None,
            max_distance: None,
            min_score: None,
            filter: None,
            method_parameters: None,
            rescore: None,
            expand_nested: None,
            ignore_unmapped: false,
            common: Common::default(),
        };
        let mut field: Option<String> = None;
        let mut boost = 1.0f32;
        let mut current = String::new();
        let mut vector_given = false;
        while let Some(t) = p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => current = p.current_name().unwrap_or("").to_string(),
                Token::StartObject => {
                    if let Some(f) = &field {
                        return Err(p.parsing_error(format!("[knn] query doesn't support multiple fields, found [{f}] and [{current}]")));
                    }
                    field = Some(current.clone());
                    p.object("knn", FIELDS, |p, i| {
                        match i {
                            0 => boost = p.float_value()?,
                            1 => q.common.name = Some(p.text()?),
                            2 => {
                                q.vector = float_array(p)?;
                                vector_given = true;
                                if q.vector.is_empty() {
                                    return Err(EsError::illegal_argument("[knn] field 'vector' requires to be non-null and non-empty"));
                                }
                            }
                            3 => q.k = Some(p.int_value()?),
                            4 => q.ignore_unmapped = p.boolean_value()?,
                            5 => q.max_distance = Some(p.float_value()?),
                            6 => q.min_score = Some(p.float_value()?),
                            7 => {
                                let v = p.take_value();
                                let Value::Obj(m) = v else { return Ok(()) };
                                for k in m.keys() {
                                    if !matches!(k.as_str(), "ef_search" | "nprobes") {
                                        return Err(p.parsing_error(format!("[knn] unknown method parameter found [{k}]")));
                                    }
                                }
                                q.method_parameters = Some(m);
                            }
                            8 => q.filter = Some(parse_inner(p)?),
                            9 => q.rescore = Some(p.take_value()),
                            _ => q.expand_nested = Some(p.boolean_value()?),
                        }
                        Ok(())
                    })?;
                }
                _ => {
                    if let Some(f) = &field {
                        return Err(p.parsing_error(format!("[knn] query doesn't support multiple fields, found [{f}] and [{current}]")));
                    }
                    field = Some(current.clone());
                    q.vector = float_array(p)?;
                    vector_given = true;
                }
            }
        }
        // `Builder.validate`.
        let arg = |m: String| EsError::illegal_argument(m);
        q.field = field.unwrap_or_default();
        if q.field.is_empty() && !vector_given {
            return Err(arg("[knn] field 'vector' requires to be non-null and non-empty".into()));
        }
        if q.field.is_empty() {
            return Err(arg("[knn] requires fieldName".into()));
        }
        if !vector_given {
            return Err(arg("[knn] requires query vector".into()));
        }
        if q.vector.is_empty() {
            return Err(arg("[knn] query vector is empty".into()));
        }
        let set = [q.k.is_some(), q.max_distance.is_some(), q.min_score.is_some()].iter().filter(|x| **x).count();
        if set != 1 {
            return Err(arg("[knn] requires exactly one of k, distance or score to be set".into()));
        }
        if q.k.is_some_and(|k| k <= 0 || k > K_MAX) {
            return Err(arg(format!("[knn] requires k to be in the range (0, {K_MAX}]")));
        }
        if q.min_score.is_some_and(|s| s <= 0.0) {
            return Err(arg("[knn] requires minScore to be greater than 0".into()));
        }
        q.common.boost = check_boost(boost, || q.clone().into())?;
        Ok(q)
    }

    /// `KNNQueryBuilderParser.toXContent` (the body under `knn`).
    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("vector".into(), Value::Arr(self.vector.iter().map(|x| float_json(*x)).collect()));
        if let Some(k) = self.k {
            m.insert("k".into(), Value::from(k as i64));
        }
        if let Some(f) = &self.filter {
            m.insert("filter".into(), f.to_value());
        }
        if let Some(d) = self.max_distance {
            m.insert("max_distance".into(), float_json(d));
        }
        if self.ignore_unmapped {
            m.insert("ignore_unmapped".into(), Value::Bool(true));
        }
        if let Some(s) = self.min_score {
            m.insert("min_score".into(), float_json(s));
        }
        if let Some(mp) = &self.method_parameters {
            m.insert("method_parameters".into(), Value::Obj(mp.clone()));
        }
        if let Some(r) = &self.rescore {
            m.insert("rescore".into(), r.clone());
        }
        m.insert("boost".into(), float_json(self.common.boost));
        if let Some(n) = &self.common.name {
            m.insert("_name".into(), Value::from(n.clone()));
        }
        if let Some(e) = self.expand_nested {
            m.insert("expand_nested_docs".into(), Value::Bool(e));
        }
        let mut outer = Map::new();
        outer.insert(self.field.clone(), Value::Obj(m));
        Value::Obj(outer)
    }
}

/// `declareFloatArray`: numbers or numeric strings.
fn float_array(p: &mut XParser<'_>) -> EsResult<Vec<f32>> {
    p.array_items(|p| match p.token() {
        Token::String => crate::query::xparser::java_parse_float(&p.text()?),
        _ => p.float_value(),
    })
}

/// Builds the shard query of a `knn` query (`KNNQueryBuilder.doToQuery`).
pub fn to_query(ctx: &QueryCtx, q: &OsKnn) -> EsResult<QueryRef> {
    let arg = |m: String| EsError::illegal_argument(m);
    let Some(f) = ctx.field(&q.field).filter(|f| f.ty == "knn_vector") else {
        if q.ignore_unmapped && ctx.field(&q.field).is_none() {
            return Ok(Arc::new(search_index::query::MatchNoneQuery::new("")));
        }
        return Err(arg(format!("Field '{}' is not knn_vector type.", q.field)));
    };
    let mctx = ctx.idx.mapping_ctx();
    let r = resolve(&f.mapper, &mctx);
    if !r.ann {
        return Err(arg(format!("Field '{}' is not built for ANN search.", q.field)));
    }
    if q.expand_nested == Some(true) && f.nested_path.is_none() {
        return Err(arg("Invalid value provided for the [expand_nested_docs] field. [expand_nested_docs] is only supported with a nested field.".into()));
    }
    let space = r.space;
    // `ResolvedIndexSpec.supportsRadialSearch`: binary vectors have no radial search.
    if q.k.is_none() && r.data == DataType::Binary {
        return Err(EsError::java(
            400,
            "unsupported_operation_exception",
            format!(
                "Radial search is not supported for this configuration: engine={}, data_type=BINARY, method=hnsw, compression=1x",
                r.engine.name().to_ascii_uppercase()
            ),
        ));
    }
    if let Some(d) = q.max_distance
        && d < 0.0
        && space != Space::InnerProduct
    {
        return Err(arg(format!("[knn] requires distance to be non-negative for space type: {}", space.constant())));
    }
    if let Some(s) = q.min_score
        && s > 1.0
        && space != Space::InnerProduct
    {
        return Err(arg(format!("[knn] requires score to be in the range [0, 1] for space type: {}", space.constant())));
    }
    let given = if r.data == DataType::Binary { q.vector.len() * 8 } else { q.vector.len() };
    if given != r.dims as usize {
        return Err(arg(format!("Query vector has invalid dimension: {given}. Dimension should be: {}", r.dims)));
    }
    let vector = match r.data {
        DataType::Byte | DataType::Binary => {
            let mut out = Vec::with_capacity(q.vector.len());
            for v in &q.vector {
                if v.fract() != 0.0 {
                    return Err(arg(format!(
                        "[data_type] field was set as [{}] in index mapping. But, KNN vector values are floats instead of byte integers",
                        r.data.value()
                    )));
                }
                if !(-128.0..=127.0).contains(v) {
                    return Err(arg(format!(
                        "[data_type] field was set as [{}] in index mapping. But, KNN vector values are not within in the byte range [-128, 127]",
                        r.data.value()
                    )));
                }
                out.push(*v as i8);
            }
            if r.data == DataType::Binary {
                QueryVector::Bits(out.into_iter().map(|x| x as u8).collect())
            } else {
                QueryVector::Byte(out)
            }
        }
        DataType::Float | DataType::HalfFloat => {
            // `NormalizeVectorTransformer` on the query vector.
            if space == Space::Cosine && q.vector.iter().all(|x| *x == 0.0) {
                return Err(arg("Cannot normalize a zero-length vector".into()));
            }
            let mut v = q.vector.clone();
            if space == Space::Cosine {
                normalize(&mut v);
            }
            QueryVector::Float(v)
        }
    };
    let radial = q.k.is_none();
    let k = q.k.map_or(usize::MAX / 4, |k| k as usize);
    let ef = q.method_parameters.as_ref().and_then(|m| m.get("ef_search")).and_then(Value::as_i64).map_or(100, |n| n.max(1) as usize);
    let mut kq = IndexKnn::new(f.number, vector, k, if radial { k } else { ef.max(k) });
    // Radial searches are answered exactly.
    kq.exact = radial;
    // The native engines answer per segment; lucene (and rescoring) merges the best `k`.
    kq.per_segment = r.engine != Engine::Lucene && q.rescore.as_ref().is_none_or(|v| v == &Value::Bool(false));
    if let Some(filter) = &q.filter {
        kq.filter = Some(ctx.to_filter(filter)?);
    }
    // Thresholds are on OpenSearch's scores; the codec's hamming score is mapped first. A
    // distance is a raw engine score (for inner product `-ip`: the similarity is at least `-d`).
    let threshold = match (q.max_distance, q.min_score) {
        (Some(d), _) => Some(space.score(d)),
        (_, Some(s)) => Some(s),
        _ => None,
    };
    // The plugin's request counters (`KNNQueryBuilder.doToQuery`): radial searches count as
    // min_score / max_distance requests instead of knn requests.
    use super::knn_api::Counter;
    let rt = &ctx.cluster.knn;
    let filtered = q.filter.is_some();
    let (base, with_filter) = match (q.max_distance, q.min_score) {
        (Some(_), _) => (Counter::MaxDistanceQuery, Counter::MaxDistanceQueryWithFilter),
        (_, Some(_)) => (Counter::MinScoreQuery, Counter::MinScoreQueryWithFilter),
        _ => (Counter::KnnQuery, Counter::KnnQueryWithFilter),
    };
    rt.bump(base);
    if filtered {
        rt.bump(with_filter);
    }
    // Native graphs go through the plugin's cache; lucene's HNSW does not.
    let ledger = (r.engine != Engine::Lucene).then(|| Ledger {
        runtime: rt.clone(),
        index_uuid: ctx.idx.uuid.clone(),
        index_name: ctx.idx.name.clone(),
        shard: ctx.shard,
        m: r.m,
        exact_below: if filtered { MAX_EXACT_DISTANCE_COMPUTATIONS / u64::from(r.dims.max(1)) } else { 0 },
    });
    let explain = Arc::new(KnnExplain {
        lucene: r.engine == Engine::Lucene,
        radial,
        high: high_level_explanation(q, space, r.dims),
        data_type: r.data.constant(),
        space,
        raw: q.rescore.is_none(),
        field: f.number,
        k: q.k.map(|k| k as usize),
        dims: r.dims,
        filter: kq.filter.clone(),
        query: q.vector.clone(),
        lucene_k: q.k.map(|k| (k as usize).max(ef)).unwrap_or(0),
        field_name: q.field.clone(),
        bytes: matches!(r.data, DataType::Byte | DataType::Binary),
    });
    Ok(Arc::new(OsKnnQuery { inner: kq, space, dims: r.dims, threshold, ledger, explain }))
}

/// `KNNWeight.getHighLevelExplanation`.
fn high_level_explanation(q: &OsKnn, space: Space, dims: u32) -> String {
    use search_index::util::java::float_to_string as fs;
    let mut s = String::from("the type of knn search executed was ");
    if let Some(r) = &q.rescore {
        // `RescoreContext`: `true` is the default context, `false` an explicitly disabled
        // one, an object sets the oversample factor itself.
        let (factor, user, enabled) = match r {
            Value::Bool(b) => (1.0f32, !*b, *b),
            Value::Obj(m) => (m.get("oversample_factor").and_then(Value::as_f64).map_or(1.0, |f| f as f32), m.contains_key("oversample_factor"), true),
            _ => (1.0, false, true),
        };
        let effective = if user {
            factor
        } else if dims >= 1000 {
            1.0
        } else if dims >= 768 {
            2.0
        } else {
            3.0
        };
        let k = q.k.unwrap_or(0) as f32;
        let first_pass = ((k * effective).ceil() as i64).clamp(100, 10_000);
        s.push_str(&format!(
            "Disk-based and the first pass k was {first_pass} with vector dimension of {dims}, over sampling factor of {}, shard level rescoring {}",
            fs(factor),
            if enabled { "enabled" } else { "disabled" }
        ));
    } else if let Some(d) = q.max_distance {
        // Inner product radii are similarities: the distance is negated.
        s.push_str(&format!("Radial with the radius of {}", fs(if space == Space::InnerProduct { -d } else { d })));
    } else if let Some(m) = q.min_score {
        // `KNNEngine.scoreToRadialThreshold`.
        let radius = match space {
            Space::Cosine => 2.0 * m - 1.0,
            Space::InnerProduct if m > 1.0 => m - 1.0,
            Space::InnerProduct => 1.0 - 1.0 / m,
            _ => 1.0 / m - 1.0,
        };
        s.push_str(&format!("Radial with the radius of {}", fs(radius)));
    } else {
        s.push_str("Approximate-NN");
    }
    s
}

/// What a k-NN hit's explanation says (`KNNWeight.explain`), and how its profile reads.
#[derive(Debug)]
struct KnnExplain {
    /// The lucene engine: Lucene's own query (`within top k docs`, `Score above threshold`).
    lucene: bool,
    radial: bool,
    high: String,
    data_type: &'static str,
    space: Space,
    /// Raw engine scores are reported (not under rescoring).
    raw: bool,
    field: u32,
    k: Option<usize>,
    dims: u32,
    filter: Option<QueryRef>,
    query: Vec<f32>,
    /// The `k` the lucene engine searches with (`max(k, ef_search)`).
    lucene_k: usize,
    field_name: String,
    bytes: bool,
}

/// `KNNConstants.MAX_DISTANCE_COMPUTATIONS`.
const MAX_DISTANCE_COMPUTATIONS: u64 = 2_048_000;

impl KnnExplain {
    /// The engine's raw score of `doc` (the squared L2 distance, faiss's inner product of the
    /// normalized vectors for cosine, the negated inner product, the bit distance).
    fn raw_score(&self, seg: &search_index::search::SegmentContext, doc: u32) -> Option<f32> {
        let vr = seg.reader.vectors(self.field)?;
        let ord = vr.ord(doc)?;
        if self.space == Space::Hamming {
            let v = vr.vector(ord);
            let bits: u32 = v.iter().zip(&self.query).map(|(a, b)| (*a ^ (*b as i8 as u8)).count_ones()).sum();
            return Some(bits as f32);
        }
        let mut v = Vec::new();
        if self.bytes {
            v.extend(vr.vector(ord).iter().map(|b| f32::from(*b as i8)));
        } else {
            vr.vector_f32(ord, &mut v);
        }
        let q = &self.query;
        Some(match self.space {
            Space::Cosine => {
                let mut a = q.clone();
                normalize(&mut a);
                normalize(&mut v);
                a.iter().zip(&v).map(|(x, y)| x * y).sum()
            }
            Space::InnerProduct => -q.iter().zip(&v).map(|(x, y)| x * y).sum::<f32>(),
            Space::L1 => q.iter().zip(&v).map(|(x, y)| (x - y).abs()).sum(),
            Space::Linf => q.iter().zip(&v).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max),
            _ => l2_squared(q, &v),
        })
    }

    /// `SpaceType.explainScoreTranslation`.
    fn translation(&self, raw: f32) -> &'static str {
        match self.space {
            Space::Cosine => "`Math.max((2.0F - rawScore) / 2.0F, 0.0F)`",
            Space::InnerProduct if raw < 0.0 => "`-rawScore + 1`",
            _ => "`1 / (1 + rawScore)`",
        }
    }

    fn explain(&self, searcher: &Searcher, seg: &search_index::search::SegmentContext, doc: u32, score: f32) -> search_index::Result<search_index::Explanation> {
        use search_index::Explanation as E;
        let mut leaf = String::from("the type of knn search executed at leaf was ");
        let mut exact = false;
        if let (Some(filter), Some(k)) = (&self.filter, self.k) {
            // `KNNWeight.getLeafLevelExplanation`: the filtered docs of the segment decide.
            let w = searcher.filter_weight(filter, search_index::ScoreMode::CompleteNoScores, 1.0)?;
            let mut cardinality = 0u64;
            if let Some(mut sc) = w.scorer(seg)? {
                let mut d = sc.next();
                while d != search_codec::NO_MORE_DOCS {
                    if seg.is_live(d) {
                        cardinality += 1;
                    }
                    d = sc.next();
                }
            }
            if cardinality <= k as u64 {
                exact = true;
                leaf.push_str(&format!("Exact since filteredIds = {cardinality} is less than or equal to K = {k}"));
            } else if cardinality * u64::from(self.dims) <= MAX_DISTANCE_COMPUTATIONS {
                exact = true;
                leaf.push_str(&format!(
                    "Exact since max distance computation = {MAX_DISTANCE_COMPUTATIONS} is greater than or equal to estimated distance computations = {}",
                    cardinality * u64::from(self.dims)
                ));
            }
        }
        if !exact {
            leaf.push_str("Approximate-NN");
        }
        leaf.push_str(&format!(" with vectorDataType = {}, spaceType = {}", self.data_type, self.space.name()));
        let raw = if self.raw && !exact { self.raw_score(seg, doc) } else { None };
        let details = match raw {
            Some(raw) => {
                leaf.push_str(&format!(" where score is computed as {} from:", self.translation(raw)));
                vec![E::leaf(raw, "rawScore, returned from FAISS library")]
            }
            None => Vec::new(),
        };
        Ok(E::matched(score, self.high.clone(), vec![E::matched(score, leaf, details)]))
    }

    /// The query's `toString` (validate, profile descriptions).
    fn describe(&self) -> String {
        if !self.lucene {
            // `KNNQuery.toString(field)` prints the field it is given: none.
            return String::new();
        }
        let first = self.query.first().map(|f| if self.bytes { format!("{}", *f as i32) } else { search_index::util::java::float_to_string(*f) }).unwrap_or_default();
        let class = if self.bytes { "OSKnnByteVectorQuery" } else { "OSKnnFloatVectorQuery" };
        format!("{class}:{}[{first},...][{}]", self.field_name, self.lucene_k)
    }
}

/// The lucene engine's `OSKnnFloatVectorQuery` / `OSKnnByteVectorQuery` (profiles only).
#[derive(Debug)]
struct LuceneKnnLeaf {
    inner: QueryRef,
    explain: Arc<KnnExplain>,
}

impl search_index::Query for LuceneKnnLeaf {
    fn weight(&self, s: &Searcher, mode: search_index::ScoreMode, boost: f32) -> search_index::Result<Box<dyn search_index::Weight>> {
        self.inner.weight(s, mode, boost)
    }

    fn lucene_string(&self, _schema: &search_index::Schema) -> String {
        self.explain.describe()
    }

    fn profile_name(&self) -> String {
        if self.explain.bytes { "OSKnnByteVectorQuery".into() } else { "OSKnnFloatVectorQuery".into() }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A k-NN query's hits on a shard, explained and profiled the way the plugin's query is.
#[derive(Debug)]
struct KnnHits {
    inner: QueryRef,
    explain: Arc<KnnExplain>,
}

struct KnnHitsWeight {
    inner: Box<dyn search_index::Weight>,
    explain: Arc<KnnExplain>,
    searcher: Searcher,
}

impl search_index::Weight for KnnHitsWeight {
    fn scorer(&self, seg: &search_index::search::SegmentContext) -> search_index::Result<Option<Box<dyn search_index::search::Scorer>>> {
        self.inner.scorer(seg)
    }

    fn scorer_supplier(&self, seg: &search_index::search::SegmentContext) -> search_index::Result<Option<Box<dyn search_index::search::ScorerSupplier>>> {
        self.inner.scorer_supplier(seg)
    }

    fn bulk_scorer(&self, seg: &search_index::search::SegmentContext, mode: search_index::ScoreMode) -> search_index::Result<Option<Box<dyn search_index::search::BulkScorer>>> {
        self.inner.bulk_scorer(seg, mode)
    }

    fn explain(&self, seg: &search_index::search::SegmentContext, doc: u32) -> search_index::Result<search_index::Explanation> {
        let e = self.inner.explain(seg, doc)?;
        if !e.is_match() {
            return Ok(e);
        }
        if self.explain.lucene {
            if self.explain.radial {
                return Ok(search_index::Explanation::leaf(e.value(), "Score above threshold"));
            }
            return Ok(e);
        }
        self.explain.explain(&self.searcher, seg, doc, e.value())
    }

    fn count(&self, seg: &search_index::search::SegmentContext) -> search_index::Result<Option<u32>> {
        self.inner.count(seg)
    }
}

impl search_index::Query for KnnHits {
    fn weight(&self, s: &Searcher, mode: search_index::ScoreMode, boost: f32) -> search_index::Result<Box<dyn search_index::Weight>> {
        // The lucene engine's query wraps Lucene's own k-NN query (a child in profiles).
        let inner = if self.explain.lucene {
            let leaf: QueryRef = Arc::new(LuceneKnnLeaf { inner: self.inner.clone(), explain: self.explain.clone() });
            s.weight_of(&leaf, mode, boost)?
        } else {
            self.inner.weight(s, mode, boost)?
        };
        Ok(Box::new(KnnHitsWeight { inner, explain: self.explain.clone(), searcher: s.clone() }))
    }

    fn lucene_string(&self, _schema: &search_index::Schema) -> String {
        self.explain.describe()
    }

    fn profile_name(&self) -> String {
        if self.explain.lucene { "LuceneEngineKnnVectorQuery".into() } else { "KNNQuery".into() }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Where a native-engine search records the graphs it loads ([`super::knn_api`]).
#[derive(Debug)]
struct Ledger {
    runtime: Arc<super::knn_api::KnnRuntime>,
    index_uuid: String,
    index_name: String,
    shard: u32,
    m: u16,
    /// A filtered search of a segment with at most this many docs is exact (no graph).
    exact_below: u64,
}

/// `KNNWeight.MAX_DISTANCE_COMPUTATIONS`: below `this / dimension` filtered documents the
/// plugin scores them exactly instead of searching the graph.
const MAX_EXACT_DISTANCE_COMPUTATIONS: u64 = 2_048_000;

impl Ledger {
    fn record(&self, s: &Searcher, field: u32) {
        for seg in s.segments() {
            let Some(vr) = seg.reader.vectors(field) else { continue };
            if vr.is_empty() || u64::from(seg.num_docs) <= self.exact_below {
                continue;
            }
            self.runtime.bump(super::knn_api::Counter::GraphQuery);
            let bytes = super::knn_api::graph_bytes(vr.len(), vr.spec().vector_bytes(), self.m);
            self.runtime.touch(&self.index_uuid, &self.index_name, (self.shard, seg.core.uid, field), bytes);
        }
    }
}

/// A `knn` query on a shard: the codec's k-NN search with OpenSearch's scores and thresholds.
#[derive(Debug)]
struct OsKnnQuery {
    inner: IndexKnn,
    space: Space,
    dims: u32,
    threshold: Option<f32>,
    ledger: Option<Ledger>,
    explain: Arc<KnnExplain>,
}

impl OsKnnQuery {
    fn hits(&self, s: &Searcher) -> search_index::Result<Vec<(u32, f32)>> {
        if let Some(l) = &self.ledger {
            l.record(s, self.inner.field);
        }
        let mut hits = self.inner.search(s)?;
        // Faiss and Lucene accumulate squared differences in order with fused multiply-adds;
        // the codec's distance may round differently in the last place, so L2 scores are
        // recomputed the engines' way.
        if self.space == Space::L2 && !self.explain.bytes {
            let mut v = Vec::new();
            for h in &mut hits {
                let Some((seg, local)) = s.segment_of(h.0) else { continue };
                let Some(vr) = seg.reader.vectors(self.inner.field) else { continue };
                let Some(ord) = vr.ord(local) else { continue };
                v.clear();
                vr.vector_f32(ord, &mut v);
                h.1 = 1.0 / (1.0 + l2_squared(&self.explain.query, &v));
            }
        }
        if self.space == Space::Hamming {
            // The codec scores bits as `(bits - h) / bits`.
            let bits = self.dims as f32;
            for h in &mut hits {
                let distance = (bits - h.1 * bits).round();
                h.1 = Space::Hamming.score(distance);
            }
        }
        if let Some(t) = self.threshold {
            hits.retain(|h| h.1 >= t);
        }
        Ok(hits)
    }

}

impl search_index::Query for OsKnnQuery {
    fn weight(&self, s: &Searcher, mode: search_index::ScoreMode, boost: f32) -> search_index::Result<Box<dyn search_index::Weight>> {
        let hits = KnnHits { inner: Arc::new(search_index::query::ScoreDocQuery::lucene(self.hits(s)?)), explain: self.explain.clone() };
        hits.weight(s, mode, boost)
    }

    fn rewrite(&self, s: &Searcher) -> search_index::Result<Option<QueryRef>> {
        let r = self.hits(s)?;
        if r.is_empty() {
            return Ok(Some(Arc::new(search_index::query::MatchNoneQuery::new(""))));
        }
        Ok(Some(Arc::new(KnnHits { inner: Arc::new(search_index::query::ScoreDocQuery::lucene(r)), explain: self.explain.clone() })))
    }

    fn lucene_string(&self, _schema: &search_index::Schema) -> String {
        self.explain.describe()
    }

    fn profile_name(&self) -> String {
        if self.explain.lucene { "LuceneEngineKnnVectorQuery".into() } else { "KNNQuery".into() }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The k-NN plugin's scoring script (`{"source": "knn_score", "lang": "knn"}` in a
/// `script_score`): an exact score against `params.query_value` in `params.space_type`, 0 for
/// documents without a vector. Its spaces score as the plugin's `KNNScoringUtil`: `l2`
/// `1 / (1 + d²)`, `cosinesimil` `(1 + cos) / 2`, `innerproduct` like the query, `l1` and `linf`
/// `1 / (1 + d)`, `hamming` `1 / (1 + bits)`.
pub fn knn_score(ctx: &QueryCtx, s: &crate::query::compound::ScriptScore) -> EsResult<QueryRef> {
    use super::knn_api::Counter;
    let rt = &ctx.cluster.knn;
    // The script service compiles the plugin's one script once and caches it.
    rt.compile_once();
    rt.bump(Counter::ScriptQuery);
    knn_score_query(ctx, s).inspect_err(|_| rt.bump(Counter::ScriptQueryErrors))
}

fn knn_score_query(ctx: &QueryCtx, s: &crate::query::compound::ScriptScore) -> EsResult<QueryRef> {
    let arg = |m: String| EsError::illegal_argument(m);
    let params = &s.script.params;
    let param = |k: &str| params.get(k).ok_or_else(|| arg(format!("Missing parameter [{k}]")));
    let field = text(param("field")?);
    let query = param("query_value")?.clone();
    let space = text(param("space_type")?);
    let space = match space.as_str() {
        "l2" | "cosinesimil" | "innerproduct" | "l1" | "linf" | "hamming" => Space::parse(&space).expect("listed"),
        "hammingbit" => {
            return Err(arg("Incompatible field_type for hammingbit space. The field type must of type long or binary.".into()));
        }
        _ => return Err(arg("Invalid space type. Please refer to the available space types.".into())),
    };
    let Some(f) = ctx.field(&field).filter(|f| f.ty == "knn_vector") else {
        return Err(arg(format!("Incompatible field_type for {} space. The field type must be knn_vector.", space.name())));
    };
    let r = resolve(&f.mapper, &ctx.idx.mapping_ctx());
    let values: Vec<f32> = match &query {
        Value::Arr(a) => a.iter().map(|v| match v {
            Value::Num(n) => Ok(n.as_f64() as f32),
            other => Err(arg(format!("Invalid query vector value [{}]", text(other)))),
        }).collect::<EsResult<_>>()?,
        other => return Err(arg(format!("Invalid query vector value [{}]", text(other)))),
    };
    let expected = if r.data == DataType::Binary { r.dims as usize / 8 } else { r.dims as usize };
    if values.len() != expected {
        return Err(EsError::java(400, "illegal_state_exception", format!("Object's length={} does not match the expected length={expected}.", values.len())));
    }
    let inner = ctx.to_query(&s.query)?;
    let script = KnnScore(Arc::new(KnnScoreSpec { field: f.number, space, data: r.data, query: values, describe: s.script.id_or_code.clone() }));
    Ok(Arc::new(search_index::query::ScriptScoreQuery { query: inner, script: Arc::new(script), min_score: s.min_score }))
}

struct KnnScore(Arc<KnnScoreSpec>);

struct KnnScoreSpec {
    field: u32,
    space: Space,
    data: DataType,
    query: Vec<f32>,
    describe: String,
}

impl KnnScoreSpec {
    fn score(&self, stored: &[f32]) -> f32 {
        let q = &self.query;
        match self.space {
            Space::L2 => 1.0 / (1.0 + q.iter().zip(stored).map(|(a, b)| (a - b) * (a - b)).sum::<f32>()),
            Space::L1 => 1.0 / (1.0 + q.iter().zip(stored).map(|(a, b)| (a - b).abs()).sum::<f32>()),
            Space::Linf => 1.0 / (1.0 + q.iter().zip(stored).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max)),
            Space::InnerProduct => {
                let ip: f32 = q.iter().zip(stored).map(|(a, b)| a * b).sum();
                Space::InnerProduct.score(-ip)
            }
            Space::Cosine => {
                let (mut dot, mut qq, mut ss) = (0.0f32, 0.0f32, 0.0f32);
                for (a, b) in q.iter().zip(stored) {
                    dot += a * b;
                    qq += a * a;
                    ss += b * b;
                }
                let n = (qq * ss).sqrt();
                if n == 0.0 { 0.0 } else { (1.0 + dot / n) / 2.0 }
            }
            Space::Hamming => {
                let bits: u32 = q.iter().zip(stored).map(|(a, b)| ((*a as i32 as u8) ^ (*b as i32 as u8)).count_ones()).sum();
                Space::Hamming.score(bits as f32)
            }
        }
    }
}

impl search_index::query::ScoreScript for KnnScore {
    fn needs_score(&self) -> bool {
        false
    }

    fn leaf(&self, seg: &search_index::SegmentContext) -> search_index::Result<Box<dyn search_index::query::function_score::LeafScoreScript>> {
        Ok(Box::new(KnnScoreLeaf { script: self.0.clone(), seg: seg.clone(), buf: Vec::new() }))
    }

    fn describe(&self) -> String {
        format!("Script{{type=inline, lang='knn', idOrCode='{}', options={{}}, params={{}}}}", self.0.describe)
    }
}

/// A segment's [`KnnScore`]: the stored vector of each document, decoded into a scratch buffer.
struct KnnScoreLeaf {
    script: Arc<KnnScoreSpec>,
    seg: search_index::SegmentContext,
    buf: Vec<f32>,
}

impl search_index::query::function_score::LeafScoreScript for KnnScoreLeaf {
    fn score(&mut self, doc: u32, _sub: f32) -> search_index::Result<f64> {
        let script = &*self.script;
        let Some(vr) = self.seg.reader.vectors(script.field) else { return Ok(0.0) };
        let Some(ord) = vr.ord(doc) else { return Ok(0.0) };
        self.buf.clear();
        match script.data {
            DataType::Float | DataType::HalfFloat => vr.vector_f32(ord, &mut self.buf),
            DataType::Byte | DataType::Binary => self.buf.extend(vr.vector(ord).iter().map(|b| *b as i8 as f32)),
        }
        Ok(script.score(&self.buf) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_follow_the_space_translations() {
        assert_eq!(Space::L2.score(1.0), 0.5);
        assert_eq!(Space::Cosine.score(0.0), 1.0);
        assert_eq!(Space::Cosine.score(2.0), 0.0);
        // `-ip` raw scores: ip 3 scores 4, ip -1 scores 1/2.
        assert_eq!(Space::InnerProduct.score(-3.0), 4.0);
        assert_eq!(Space::InnerProduct.score(1.0), 0.5);
        assert_eq!(Space::Hamming.score(3.0), 0.25);
    }

    #[test]
    fn half_rounds_like_float16() {
        // Values OpenSearch 3.9.0 returned for a half_float vector [1.1, 2.2].
        assert_eq!(half(1.1), 1.0996094);
        assert_eq!(half(2.2), 2.1992188);
        assert_eq!(half(1.0), 1.0);
        assert_eq!(half(-65504.0), -65504.0);
        assert_eq!(half(0.0), 0.0);
        // Subnormal halves are multiples of 2^-24.
        assert_eq!(half(3e-8), 2f32.powi(-24) * 0.0f32.max((3e-8f32 / 2f32.powi(-24)).round_ties_even()));
    }

    fn resolved(data: DataType, space: Space, dims: u32) -> Resolved {
        Resolved { dims, data, space, engine: Engine::Faiss, ann: true, m: 16, ef_construction: 100 }
    }

    #[test]
    fn doc_vectors_are_checked_like_the_plugin() {
        let r = resolved(DataType::Float, Space::L2, 3);
        assert!(matches!(check_doc_vector(&r, &[1.0, 2.0, 3.0]), Ok(DocVector::Floats(v)) if v == vec![1.0, 2.0, 3.0]));
        let e = check_doc_vector(&r, &[1.0, 2.0]).err().unwrap();
        assert_eq!((e.0, e.1.reason()), (None, "Vector dimension mismatch. Expected: 3, Given: 2"));
        let e = check_doc_vector(&r, &[1.0, 2.0, 3.5e40]).err().unwrap();
        assert_eq!((e.0, e.1.reason()), (Some(2), "KNN vector values cannot be infinity"));
        let b = resolved(DataType::Byte, Space::L2, 3);
        assert!(check_doc_vector(&b, &[1.0, 2.0, 3.5]).err().unwrap().1.reason().ends_with("are floats instead of byte integers"));
        assert!(check_doc_vector(&b, &[1.0, 2.0, 300.0]).err().unwrap().1.reason().ends_with("byte range [-128, 127]"));
        let x = resolved(DataType::Binary, Space::Hamming, 8);
        assert!(matches!(check_doc_vector(&x, &[1.0]), Ok(DocVector::Bytes(v)) if v == vec![1]));
        assert_eq!(
            check_doc_vector(&x, &[1.0, 2.0]).err().unwrap().1.reason(),
            "The dimension of the binary vector must be 8 times the length of the provided vector. Expected: 8, Given: 16"
        );
        let c = resolved(DataType::Float, Space::Cosine, 2);
        assert!(matches!(check_doc_vector(&c, &[3.0, 4.0]), Ok(DocVector::Floats(v)) if v == vec![0.6, 0.8]));
        assert_eq!(check_doc_vector(&c, &[0.0, 0.0]).err().unwrap().1.reason(), "zero vector is not supported when space type is [cosinesimil]");
    }

    #[test]
    fn derived_values_print_as_java_floats() {
        let r = resolved(DataType::Float, Space::L2, 3);
        let v = derived_value(&r, &Value::Arr(vec![Value::from(0.1), Value::from("2"), Value::from(3i64)])).unwrap();
        assert_eq!(v.to_json(), "[0.1,2.0,3.0]");
        let h = resolved(DataType::HalfFloat, Space::L2, 2);
        assert_eq!(derived_value(&h, &Value::Arr(vec![Value::from(1.1), Value::from(2.2)])).unwrap().to_json(), "[1.0996094,2.1992188]");
    }
}
