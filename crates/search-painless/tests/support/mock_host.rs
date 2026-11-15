//! A [`Host`] over one JSON document and an Elasticsearch mapping, producing doc values the way
//! Elasticsearch's field types do (sorted, quantized, deduplicated), for fixture tests of
//! document-based contexts.

use search_painless::{DocKind, DocValues, Host, HostError, Value};
use serde_json::Value as Json;

pub struct MockHost {
    pub index: String,
    pub mapping: Json,
    pub doc: Json,
    /// `_score` of the document.
    pub score: f64,
    /// `_seq_no` (and Lucene doc id) of the document.
    pub seq_no: i64,
    /// Whether the script runs in a search (the `_execute` API cannot look its index up).
    pub search: bool,
}

fn field_type<'a>(mapping: &'a Json, path: &str) -> Option<&'a Json> {
    let mut props = mapping.get("properties")?;
    let parts: Vec<&str> = path.split('.').collect();
    for (i, p) in parts.iter().enumerate() {
        let f = props.get(*p)?;
        if i + 1 == parts.len() {
            return if f.get("type").is_some() { Some(f) } else { None };
        }
        props = f.get("properties")?;
    }
    None
}

fn values_at<'a>(doc: &'a Json, path: &str) -> Vec<&'a Json> {
    let mut cur = vec![doc];
    for p in path.split('.') {
        let mut next = Vec::new();
        for c in cur {
            match c {
                Json::Object(o) => {
                    if let Some(v) = o.get(p) {
                        next.push(v);
                    }
                }
                Json::Array(a) => {
                    for x in a {
                        if let Some(v) = x.get(p) {
                            next.push(v);
                        }
                    }
                }
                _ => {}
            }
        }
        cur = next;
    }
    let mut out = Vec::new();
    for c in cur {
        match c {
            Json::Array(a) => out.extend(a.iter().filter(|x| !x.is_null())),
            Json::Null => {}
            v => out.push(v),
        }
    }
    out
}

fn half(f: f32) -> f32 {
    // round to IEEE half precision (nearest even) and back
    let bits = f.to_bits();
    let sign = bits & 0x8000_0000;
    let abs = f32::from_bits(bits & 0x7fff_ffff);
    if abs.is_nan() {
        return f;
    }
    if abs >= 65520.0 {
        return f32::from_bits(sign | 0x7f80_0000);
    }
    let e = abs.log2().floor() as i32;
    let e = e.max(-14);
    let quantum = 2f32.powi(e - 10);
    let q = abs / quantum;
    let r = q.round();
    let r = if (q - q.floor() - 0.5).abs() == 0.0 && (r as i64) % 2 == 1 { r - 1.0 } else { r };
    f32::from_bits(sign | (r * quantum).to_bits())
}

fn num(v: &Json) -> f64 {
    match v {
        Json::Number(n) => n.as_f64().unwrap_or(0.0),
        Json::String(s) => s.parse().unwrap_or(0.0),
        Json::Bool(b) => *b as i64 as f64,
        _ => 0.0,
    }
}

fn parse_millis(s: &str) -> i64 {
    search_painless::es::score::parse_es_date(s).unwrap_or(0)
}

fn parse_nanos(s: &str) -> i64 {
    match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(d) => d.timestamp_nanos_opt().unwrap_or(0),
        Err(_) => parse_millis(s) * 1_000_000,
    }
}

const LAT_DECODE: f64 = 180.0 / 4294967296.0;
const LON_DECODE: f64 = 360.0 / 4294967296.0;

fn geo(v: &Json) -> Option<(f64, f64)> {
    let (lat, lon) = match v {
        Json::String(s) => {
            let p: Vec<&str> = s.split(',').collect();
            (p[0].trim().parse().ok()?, p[1].trim().parse().ok()?)
        }
        Json::Object(o) => (num(o.get("lat")?), num(o.get("lon")?)),
        Json::Array(a) => (num(&a[1]), num(&a[0])),
        _ => return None,
    };
    let la = (lat / LAT_DECODE).floor() as i32;
    let lo = (lon / LON_DECODE).floor() as i32;
    Some((la as f64 * LAT_DECODE, lo as f64 * LON_DECODE))
}

impl Host for MockHost {
    fn field_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        let is_text = field_type(&self.mapping, field).is_some_and(|ft| ft["type"] == "text");
        if !is_text {
            return self.doc_values(field, out);
        }
        // the field API reads text fields from `_source`
        let mut vals: Vec<String> = values_at(&self.doc, field).iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect();
        vals.sort();
        out.clear(DocKind::String);
        for v in &vals {
            out.push_str(v);
        }
        Ok(true)
    }

    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        if field == "_seq_no" {
            out.clear(DocKind::Long);
            out.longs.push(self.seq_no);
            return Ok(true);
        }
        let Some(ft) = field_type(&self.mapping, field) else { return Ok(false) };
        let ty = ft["type"].as_str().unwrap_or("");
        let vals = values_at(&self.doc, field);
        let kind = match ty {
            "long" => DocKind::Long,
            "integer" => DocKind::Integer,
            "short" => DocKind::Short,
            "byte" => DocKind::Byte,
            "unsigned_long" => DocKind::UnsignedLong,
            "double" => DocKind::Double,
            "float" => DocKind::Float,
            "half_float" => DocKind::HalfFloat,
            "scaled_float" => DocKind::ScaledFloat,
            "keyword" => DocKind::String,
            "boolean" => DocKind::Boolean,
            "date" => DocKind::Date,
            "date_nanos" => DocKind::DateNanos,
            "geo_point" => DocKind::GeoPoint,
            "ip" => DocKind::Ip,
            "binary" => DocKind::Binary,
            "version" => DocKind::Version,
            "dense_vector" => DocKind::DenseVector,
            "text" => {
                return Err(HostError {
                    kind: "illegal_argument_exception",
                    reason: format!("Fielddata is disabled on [{field}] in [{}]. Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{field}] in order to load field data by uninverting the inverted index. Note that this can use significant memory.", self.index),
                    frames: vec![
                        "org.elasticsearch.server@9.5.5/org.elasticsearch.index.mapper.TextFieldMapper$TextFieldType.fielddataBuilder(TextFieldMapper.java:1550)".into(),
                        "org.elasticsearch.server@9.5.5/org.elasticsearch.index.fielddata.IndexFieldDataService.getForField(IndexFieldDataService.java:92)".into(),
                        "org.elasticsearch.server@9.5.5/org.elasticsearch.index.IndexService.loadFielddata(IndexService.java:1451)".into(),
                        "org.elasticsearch.server@9.5.5/org.elasticsearch.index.query.SearchExecutionContext.lambda$setLookupProviders$5(SearchExecutionContext.java:570)".into(),
                    ],
                })
            }
            _ => return Ok(false),
        };
        out.clear(kind);
        match kind {
            DocKind::Long | DocKind::Integer | DocKind::Short | DocKind::Byte => {
                out.longs.extend(vals.iter().map(|v| num(v) as i64));
                out.longs.sort();
            }
            DocKind::UnsignedLong => {
                let mut u: Vec<u64> = vals.iter().map(|v| v.as_u64().unwrap_or(num(v) as u64)).collect();
                u.sort();
                out.longs.extend(u.iter().map(|x| *x as i64));
            }
            DocKind::Boolean => {
                out.longs.extend(vals.iter().map(|v| match v {
                    Json::Bool(b) => *b as i64,
                    Json::String(s) => (s == "true") as i64,
                    _ => 0,
                }));
                out.longs.sort();
            }
            DocKind::Date => {
                out.longs.extend(vals.iter().map(|v| match v {
                    Json::String(s) => parse_millis(s),
                    other => num(other) as i64,
                }));
                out.longs.sort();
            }
            DocKind::DateNanos => {
                out.longs.extend(vals.iter().map(|v| match v {
                    Json::String(s) => parse_nanos(s),
                    other => num(other) as i64 * 1_000_000,
                }));
                out.longs.sort();
            }
            DocKind::Double => {
                out.doubles.extend(vals.iter().map(|v| num(v)));
                out.doubles.sort_by(|a, b| a.total_cmp(b));
            }
            DocKind::Float => {
                out.doubles.extend(vals.iter().map(|v| num(v) as f32 as f64));
                out.doubles.sort_by(|a, b| a.total_cmp(b));
            }
            DocKind::HalfFloat => {
                out.doubles.extend(vals.iter().map(|v| half(num(v) as f32) as f64));
                out.doubles.sort_by(|a, b| a.total_cmp(b));
            }
            DocKind::ScaledFloat => {
                let f = ft["scaling_factor"].as_f64().unwrap_or(1.0);
                out.doubles.extend(vals.iter().map(|v| (num(v) * f + 0.5).floor() / f));
                out.doubles.sort_by(|a, b| a.total_cmp(b));
            }
            DocKind::String | DocKind::Version => {
                let mut s: Vec<String> = vals.iter().map(|v| match v {
                    Json::String(s) => s.clone(),
                    other => other.to_string(),
                }).collect();
                s.sort();
                s.dedup();
                for x in s {
                    out.push_str(&x);
                }
            }
            DocKind::Ip => {
                let mut ips: Vec<std::net::IpAddr> = vals.iter().filter_map(|v| v.as_str()?.parse().ok()).collect();
                ips.sort_by_key(|ip| match ip {
                    std::net::IpAddr::V4(a) => a.to_ipv6_mapped().octets(),
                    std::net::IpAddr::V6(a) => a.octets(),
                });
                for ip in ips {
                    out.push_str(&ip.to_string());
                }
            }
            DocKind::GeoPoint => {
                let single = matches!(self.doc_value(field), Some(Json::Array(a)) if a.len() == 2 && a.iter().all(|x| x.is_number()));
                let pts: Vec<(f64, f64)> = if single { geo(self.doc_value(field).unwrap()).into_iter().collect() } else { vals.iter().filter_map(|v| geo(v)).collect() };
                let mut pts = pts;
                pts.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
                out.geo.extend(pts.iter().map(|(a, b)| [*a, *b]));
            }
            DocKind::Binary => {
                use base64::Engine;
                for v in vals {
                    if let Some(s) = v.as_str() {
                        out.bytes.push(base64::engine::general_purpose::STANDARD.decode(s).unwrap_or_default());
                    }
                }
            }
            DocKind::RankVectors | DocKind::GeoShape => {}
            DocKind::DenseVector => {
                use search_painless::VectorElement;
                out.element = match ft["element_type"].as_str() {
                    Some("byte") => VectorElement::Byte,
                    Some("bit") => VectorElement::Bit,
                    Some("bfloat16") => VectorElement::BFloat16,
                    _ => VectorElement::Float,
                };
                out.vector_binary = ft["index"] == Json::Bool(false);
                match self.doc_value(field) {
                    Some(Json::Array(a)) if matches!(out.element, VectorElement::Byte | VectorElement::Bit) => out.vector_bytes.extend(a.iter().map(|x| num(x) as i8)),
                    Some(Json::String(h)) if matches!(out.element, VectorElement::Byte | VectorElement::Bit) => {
                        out.vector_bytes.extend((0..h.len() / 2).map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap_or(0) as i8))
                    }
                    Some(Json::Array(a)) => out.vector.extend(a.iter().map(|x| num(x) as f32)),
                    _ => {}
                }
                // doc-values-only vectors store their magnitude
                if out.vector_binary && !(out.vector.is_empty() && out.vector_bytes.is_empty()) {
                    out.magnitude = Some(match out.element {
                        VectorElement::Byte => (out.vector_bytes.iter().map(|&b| b as i32 * b as i32).sum::<i32>() as f64).sqrt() as f32,
                        VectorElement::Bit => (out.vector_bytes.iter().map(|&b| (b as u8).count_ones()).sum::<u32>() as f64).sqrt() as f32,
                        _ => (out.vector.iter().map(|&x| x * x).sum::<f32>() as f64).sqrt() as f32,
                    });
                }
            }
        }
        Ok(true)
    }

    fn source(&mut self) -> Option<Value> {
        Some(Value::from_json_map(&self.doc))
    }

    fn score(&mut self) -> f64 {
        self.score
    }

    fn doc_id(&mut self) -> i32 {
        self.seq_no as i32
    }

    fn shard(&mut self) -> (i32, String) {
        (0, if self.search { self.index.clone() } else { String::new() })
    }
}

impl MockHost {
    fn doc_value(&self, field: &str) -> Option<&Json> {
        let mut cur = &self.doc;
        for p in field.split('.') {
            cur = cur.get(p)?;
        }
        Some(cur)
    }
}
