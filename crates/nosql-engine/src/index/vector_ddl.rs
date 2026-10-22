//! ScyllaDB's `vector_index` DDL: targets with a local partition (`((p1, p2), v)`) and filtering
//! columns (`(v, f1, f2)`), their stored form, and the index options with their validation, as
//! the reference image answers them (learned by probing; ScyllaDB's docs describe the options:
//! `docs/cql/secondary-indexes.rst`, `docs/dev/vector_index.md`).
//!
//! # Stored target
//!
//! `system_schema.indexes.options.target` is the column name for a plain index, and JSON
//! otherwise: `{"tc":"v","fc":["f1","f2"]}`, `{"tc":"v","pk":["p1","p2"]}` or both (`tc` the
//! vector column, `pk` the local partition, `fc` the filtering columns).
//!
//! # Validation order
//!
//! The vector column (a `vector<float, n>`), the local partition and filtering columns (types,
//! statics, duplicates), the primary key size the Vector Store takes (255 columns), then the
//! options, in the iteration order of the reference's `std::unordered_map` of them
//! ([`crate::cpp::unordered_map_order`]).

use std::collections::BTreeMap;

use nosql_cql::ast::{self, IndexTarget as AstTarget, IndexTargetKind};
use nosql_types::CqlType;

use crate::error::{Error, Result};
use crate::schema::Table;

/// A vector index target: the vector column, the local partition columns and the filtering
/// columns (names).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VectorTarget {
    pub column: String,
    pub pk: Vec<String>,
    pub fc: Vec<String>,
}

impl VectorTarget {
    /// The stored form: the (CQL-quoted) column, or the JSON object.
    pub fn text(&self) -> String {
        if self.pk.is_empty() && self.fc.is_empty() {
            return nosql_types::quote_identifier(&self.column, nosql_types::Dialect::Scylla)
                .into_owned();
        }
        let list = |v: &[String]| -> String {
            v.iter()
                .map(|s| json_string(s))
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut s = format!("{{\"tc\":{}", json_string(&self.column));
        if !self.pk.is_empty() {
            s.push_str(&format!(",\"pk\":[{}]", list(&self.pk)));
        }
        if !self.fc.is_empty() {
            s.push_str(&format!(",\"fc\":[{}]", list(&self.fc)));
        }
        s.push('}');
        s
    }

    /// Parses the JSON form (`None` when it isn't one).
    pub fn parse(s: &str) -> Option<VectorTarget> {
        let s = s.trim();
        if !s.starts_with('{') || !s.contains("\"tc\"") {
            return None;
        }
        let mut p = Json {
            s: s.as_bytes(),
            i: 1,
        };
        let mut out = VectorTarget::default();
        loop {
            p.ws();
            if p.eat(b'}') {
                break;
            }
            let key = p.string()?;
            p.ws();
            if !p.eat(b':') {
                return None;
            }
            p.ws();
            match key.as_str() {
                "tc" => out.column = p.string()?,
                "pk" => out.pk = p.strings()?,
                "fc" => out.fc = p.strings()?,
                _ => return None,
            }
            p.ws();
            p.eat(b',');
        }
        Some(out)
    }

    /// How `DESCRIBE` shows the target: `(p1, p2), v, f1`.
    pub fn describe(&self) -> String {
        let q = |s: &String| -> String {
            nosql_types::quote_identifier(s, nosql_types::Dialect::Scylla).into_owned()
        };
        let mut parts = Vec::new();
        if !self.pk.is_empty() {
            parts.push(format!(
                "({})",
                self.pk.iter().map(q).collect::<Vec<_>>().join(", ")
            ));
        }
        parts.push(q(&self.column));
        parts.extend(self.fc.iter().map(q));
        parts.join(", ")
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A minimal reader of the target JSON (strings and arrays of strings).
struct Json<'a> {
    s: &'a [u8],
    i: usize,
}

impl Json<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(|b| b.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.s.get(self.i) == Some(&b) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat(b'"') {
            return None;
        }
        let mut out = Vec::new();
        loop {
            let b = *self.s.get(self.i)?;
            self.i += 1;
            match b {
                b'"' => break,
                b'\\' => {
                    out.push(*self.s.get(self.i)?);
                    self.i += 1;
                }
                b => out.push(b),
            }
        }
        String::from_utf8(out).ok()
    }

    fn strings(&mut self) -> Option<Vec<String>> {
        if !self.eat(b'[') {
            return None;
        }
        let mut out = Vec::new();
        loop {
            self.ws();
            if self.eat(b']') {
                return Some(out);
            }
            out.push(self.string()?);
            self.ws();
            self.eat(b',');
        }
    }
}

/// The types a local partition or filtering column may have (native types but counter and
/// duration).
fn filtering_type(ty: &CqlType) -> bool {
    matches!(
        ty,
        CqlType::Ascii
            | CqlType::Bigint
            | CqlType::Blob
            | CqlType::Boolean
            | CqlType::Date
            | CqlType::Decimal
            | CqlType::Double
            | CqlType::Float
            | CqlType::Inet
            | CqlType::Int
            | CqlType::Smallint
            | CqlType::Text
            | CqlType::Time
            | CqlType::Timestamp
            | CqlType::Timeuuid
            | CqlType::Tinyint
            | CqlType::Uuid
            | CqlType::Varint
    )
}

/// The Vector Store keys rows by at most this many primary key columns.
const MAX_KEY_COLUMNS: usize = 255;

/// `CREATE CUSTOM INDEX ... USING 'vector_index'` (or an SAI class rewritten to it): the
/// targets, validated. Returns the target and the vector column's index.
pub fn targets(table: &Table, c: &ast::CreateIndex, sai: bool) -> Result<(VectorTarget, usize)> {
    let mut pk: Vec<String> = Vec::new();
    let mut cols: Vec<(IndexTargetKind, String)> = Vec::new();
    for t in &c.targets {
        match t {
            AstTarget::PartitionKey(ids) => pk.extend(ids.iter().map(|i| i.name().to_string())),
            AstTarget::Column(k, id) => cols.push((*k, id.name().to_string())),
        }
    }
    let Some((kind, name)) = cols.first().cloned() else {
        return Err(Error::invalid(
            "Only CUSTOM indexes can be created without specifying a target column",
        ));
    };
    let vector_float = |ci: usize| matches!(&table.columns[ci].ty, CqlType::Vector { elem, .. } if **elem == CqlType::Float);
    if sai {
        if kind == IndexTargetKind::Entries {
            return Err(Error::invalid(
                if c.properties.iter().any(|p| p.name.name() == "options") {
                    "StorageAttachedIndex (SAI) on map entries cannot be rewritten to a regular secondary index when WITH OPTIONS are specified; remove the options or use a secondary index directly"
                } else {
                    "StorageAttachedIndex (SAI) on ENTRIES of a map column is not supported; if this is a CassIO/LangChain workload, enable the enable_cassio_compatibility configuration option to rewrite this index as a regular secondary index"
                },
            ));
        }
        if !table
            .column_index(&name)
            .is_some_and(|ci| table.columns[ci].ty.is_vector())
        {
            return Err(Error::invalid(
                "StorageAttachedIndex (SAI) is only supported on vector columns; use a secondary index for non-vector columns",
            ));
        }
    }
    // The local partition: existing, non-static, distinct columns.
    let mut local: Vec<usize> = Vec::new();
    for n in &pk {
        let Some(pi) = table
            .column_index(n)
            .filter(|&i| !table.columns[i].is_static())
        else {
            return Err(Error::invalid(format!(
                "Local vector index definition must contain primary key or regular columns. Redundant column: {n}"
            )));
        };
        if local.contains(&pi) {
            return Err(Error::invalid(format!(
                "Duplicate column definition in local vector index: {n}"
            )));
        }
        local.push(pi);
    }
    let Some(ci) = table.column_index(&name) else {
        return Err(Error::invalid(format!("Column {name} not found in schema")));
    };
    if !vector_float(ci) {
        return Err(Error::invalid(
            "Vector indexes are only supported on columns of vectors of floats",
        ));
    }
    let static_error =
        || Error::invalid("Local indexes containing static columns are not supported.");
    let type_error = |n: &str| {
        Error::invalid(format!(
            "Unsupported vector index filtering column {n} type"
        ))
    };
    for (&pi, n) in local.iter().zip(&pk) {
        if !filtering_type(&table.columns[pi].ty) {
            return Err(type_error(n));
        }
    }
    let mut fc: Vec<String> = Vec::new();
    for (k, n) in &cols[1..] {
        if fc.contains(n) {
            return Err(Error::invalid(format!(
                "Duplicate column {n} in index target list"
            )));
        }
        let Some(fi) = table.column_index(n) else {
            return Err(Error::invalid(format!("Column {n} not found in schema")));
        };
        let col = &table.columns[fi];
        if col.kind == crate::schema::ColumnKind::PartitionKey && table.partition_key.len() == 1 {
            return Err(Error::invalid(format!(
                "Cannot create secondary index on partition key column {n}"
            )));
        }
        if col.is_static() {
            return Err(static_error());
        }
        if *k != IndexTargetKind::Simple || !filtering_type(&col.ty) {
            return Err(type_error(n));
        }
        fc.push(n.clone());
    }
    let key_columns = table.partition_key.len() + table.clustering.len();
    if key_columns > MAX_KEY_COLUMNS {
        return Err(Error::invalid(format!(
            "Vector index requires at most {MAX_KEY_COLUMNS} primary key columns, but {key_columns} were found"
        )));
    }
    let name = table.columns[ci].name.clone();
    Ok((
        VectorTarget {
            column: name,
            pk,
            fc,
        },
        ci,
    ))
}

/// `strtol`-style integer text (leading whitespace and a sign allowed, the whole text consumed,
/// within `int`), as the reference reads integer options.
fn c_int(s: &str) -> Option<i32> {
    let t = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    t.parse::<i32>().ok()
}

/// `strtof`-style float text (leading whitespace, decimal or hexadecimal, `inf` / `nan`, the
/// whole text consumed); a value out of the float range is not a float.
fn c_float(s: &str) -> Option<f32> {
    let t = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let lower = body.to_ascii_lowercase();
    let v: f64 = if matches!(lower.as_str(), "inf" | "infinity") {
        f64::INFINITY
    } else if lower == "nan" {
        f64::NAN
    } else if let Some(hex) = lower.strip_prefix("0x") {
        hex_float(hex)?
    } else {
        if body.is_empty()
            || !body
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        {
            return None;
        }
        let v = body.parse::<f64>().ok()?;
        let f = v as f32;
        // ERANGE: overflow, or a non-zero value below the normal range.
        if f.is_infinite() || (v != 0.0 && f.abs() < f32::MIN_POSITIVE) {
            return None;
        }
        v
    };
    let f = v as f32;
    if v.is_finite() && f.is_infinite() {
        return None;
    }
    Some(if neg { -f } else { f })
}

/// The digits of a hexadecimal float after `0x` (`1.8p3`).
fn hex_float(s: &str) -> Option<f64> {
    let (mantissa, exp) = match s.split_once('p') {
        Some((m, e)) => (m, e.parse::<i32>().ok()?),
        None => (s, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    let mut v = 0f64;
    for c in int.chars() {
        v = v * 16.0 + c.to_digit(16)? as f64;
    }
    let mut scale = 1.0 / 16.0;
    for c in frac.chars() {
        v += c.to_digit(16)? as f64 * scale;
        scale /= 16.0;
    }
    Some(v * 2f64.powi(exp))
}

/// The options of a vector index (with `index_version` and `class_name` refused before).
pub fn validate_options(opts: &BTreeMap<String, String>) -> Result<()> {
    let bad = |k: &str, v: &str, why: String| {
        Err(Error::invalid(format!(
            "Invalid value in option '{k}' for vector index: '{v}' {why}"
        )))
    };
    let int_range = |k: &str, v: &str, max: i32| -> Result<()> {
        match c_int(v) {
            None => bad(k, v, "is not an integer".into()),
            Some(n) if !(1..=max).contains(&n) => {
                bad(k, v, format!("is out of valid range [1 - {max}]"))
            }
            Some(_) => Ok(()),
        }
    };
    let choice = |k: &str, v: &str, allowed: &[&str]| -> Result<()> {
        if allowed.iter().any(|a| a.eq_ignore_ascii_case(v)) {
            return Ok(());
        }
        Err(Error::invalid(format!(
            "Invalid value in option '{k}' for vector index: '{v}'. Supported are case-insensitive: {}",
            allowed.join(", ")
        )))
    };
    // The reference walks its `std::unordered_map` of the options.
    let keys: Vec<&str> = opts.keys().map(String::as_str).collect();
    for k in crate::cpp::unordered_map_order(&keys) {
        let v = &opts[k];
        match k {
            "similarity_function" => choice(k, v, &["cosine", "euclidean", "dot_product"])?,
            "rescoring" => choice(k, v, &["false", "true"])?,
            "quantization" => choice(k, v, &["f32", "f16", "bf16", "i8", "b1"])?,
            "search_beam_width" | "construction_beam_width" => int_range(k, v, 4096)?,
            "maximum_node_connections" => int_range(k, v, 512)?,
            "oversampling" => match c_float(v) {
                None => bad(k, v, "is not a float".into())?,
                Some(f) if !(1.0..=100.0).contains(&f) => {
                    bad(k, v, "is out of valid range [1 - 100]".into())?
                }
                Some(_) => {}
            },
            "source_model" => {}
            _ => {
                return Err(Error::invalid(format!(
                    "Unsupported option {k} for vector index"
                )));
            }
        }
    }
    Ok(())
}

/// The options of a full-text index: `analyzer` and `positions`, checked in the reference's
/// map order like a vector index's.
pub fn validate_fulltext_options(opts: &BTreeMap<String, String>) -> Result<()> {
    let keys: Vec<&str> = opts.keys().map(String::as_str).collect();
    for k in crate::cpp::unordered_map_order(&keys) {
        let v = &opts[k];
        let allowed: &[&str] = match k {
            "analyzer" => &[
                "standard",
                "english",
                "german",
                "french",
                "spanish",
                "italian",
                "portuguese",
                "russian",
                "simple",
                "whitespace",
            ],
            "positions" => &["false", "true"],
            _ => {
                return Err(Error::invalid(format!(
                    "Unsupported option {k} for fulltext index"
                )));
            }
        };
        if !allowed.iter().any(|a| a.eq_ignore_ascii_case(v)) {
            return Err(Error::invalid(format!(
                "Invalid value in option '{k}' for fulltext index: '{v}'. Supported are case-insensitive: {}",
                allowed.join(", ")
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_round_trip() {
        let t = VectorTarget {
            column: "v".into(),
            pk: vec!["p1".into(), "p\"2".into()],
            fc: vec!["f".into()],
        };
        assert_eq!(t.text(), r#"{"tc":"v","pk":["p1","p\"2"],"fc":["f"]}"#);
        assert_eq!(VectorTarget::parse(&t.text()), Some(t.clone()));
        assert_eq!(t.describe(), "(p1, \"p\"\"2\"), v, f");
        let plain = VectorTarget {
            column: "V".into(),
            ..Default::default()
        };
        assert_eq!(plain.text(), "\"V\"");
        assert_eq!(VectorTarget::parse("v"), None);
        assert_eq!(VectorTarget::parse(r#"{"pk":["p"],"ck":["v"]}"#), None);
    }

    #[test]
    fn c_numbers() {
        assert_eq!(c_int("+5"), Some(5));
        assert_eq!(c_int(" 7"), Some(7));
        assert_eq!(c_int("-0"), Some(0));
        for bad in [" 7 ", "1.5", "1e2", "0x10", "", "3000000000", "+"] {
            assert_eq!(c_int(bad), None, "{bad}");
        }
        assert_eq!(c_float("0x10"), Some(16.0));
        assert_eq!(c_float("0x1p3"), Some(8.0));
        assert_eq!(c_float("  9"), Some(9.0));
        assert_eq!(c_float("1."), Some(1.0));
        assert_eq!(c_float(".5e1"), Some(5.0));
        assert!(c_float("INF").is_some_and(f32::is_infinite));
        assert!(c_float("nan").is_some_and(f32::is_nan));
        for bad in ["  9  ", "abc", "1e39", "1e-50", "5f", ""] {
            assert_eq!(c_float(bad), None, "{bad}");
        }
    }
}
