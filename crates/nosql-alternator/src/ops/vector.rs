//! Vector indexes (`VectorIndexes`, `VectorIndexUpdates`, Query's `VectorSearch`) and the
//! `FLOAT32VECTOR` attribute type, as ScyllaDB 2026.3.3 implements them.
//!
//! This is the preview API that image ships: `VectorAttribute: {AttributeName, Dimensions}`,
//! `SimilarityFunction` (`COSINE` by default), `Projection: KEYS_ONLY` only, and searches as a
//! Query with `VectorSearch: {QueryVector}` and `Limit`. Indexes need tablets and a CDC log
//! (created, disabled, with the first vector index). Without a vector store, indexes stay
//! `CREATING` and a valid search fails with `Vector Store is disabled`.
//!
//! An indexed attribute, when written, must be an `L` of exactly `Dimensions` numbers that fit
//! a 32-bit float, or a `FLOAT32VECTOR` of that length.

use super::*;
use crate::schema::{IndexSchema, KeySchema, VectorIndex};

pub const KEYS_ONLY_STREAM_ERROR: &str = "Internal server error: exceptions::invalid_request_exception (Vector Search is enabled on this table.\nThe CDC log must meet the minimal requirements for external indexes.\nThis means that the CDC's TTL must be at least 86400 seconds (24 hours), and the CDC's delta mode must be set to 'full' or postimage must be enabled.)";

const FUNCTIONS: [&str; 3] = ["EUCLIDEAN", "COSINE", "DOT_PRODUCT"];

/// The initial tablet count a table gets from its `system:initial_tablets` tag (`Some(0)`
/// without the tag: the server picks), parsed as ScyllaDB does: like `strtol` (leading
/// whitespace and sign accepted, junk after the digits ignored), negative values as 0, then
/// truncated to 32 bits. `None` (no digits, or overflowing 64 bits) means vnodes.
pub fn initial_tablets(tags: &[(String, String)]) -> Option<i32> {
    let Some((_, v)) = tags.iter().find(|(k, _)| k == "system:initial_tablets") else {
        return Some(0);
    };
    let s = v.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let n: String = digits.chars().take_while(char::is_ascii_digit).collect();
    if n.is_empty() {
        return None;
    }
    let v: i64 = n.parse().ok()?;
    Some(if neg { 0 } else { v as i32 })
}

/// Validates one vector index definition. `what` prefixes messages (`VectorIndexes` or
/// `VectorIndexUpdates`).
fn parse_definition(
    v: &Json,
    what: &str,
    update: bool,
    key_of: &dyn Fn(&str) -> Option<String>,
) -> Result<VectorIndex> {
    if !v.is_object() {
        return Err(json_assert("IsObject()"));
    }
    let name = match v.get("IndexName").and_then(Json::as_str) {
        Some(n) => n,
        None if update => {
            return Err(Error::validation(format!(
                "{what} operation must have IndexName"
            )));
        }
        None => {
            return Err(Error::validation(format!(
                "{what} IndexName must be a string."
            )));
        }
    };
    if name.len() < 3 || name.len() > 192 {
        return Err(Error::validation(format!(
            "{what} IndexName must be at least 3 characters long and at most 192 characters long"
        )));
    }
    if !valid_name_chars(name) {
        return Err(Error::validation(format!(
            "{what} IndexName must satisfy regular expression pattern: [a-zA-Z0-9_.-]+"
        )));
    }
    let sub = if update {
        format!("{what} Create")
    } else {
        what.to_string()
    };
    let Some(va) = v.get("VectorAttribute").filter(|x| x.is_object()) else {
        return Err(Error::validation(format!(
            "{sub} VectorAttribute must be an object."
        )));
    };
    let Some(attr) = va.get("AttributeName").and_then(Json::as_str) else {
        return Err(Error::validation(format!(
            "{sub} AttributeName must be a string."
        )));
    };
    if let Err(e) = crate::expr::resolve::check_attr_name(attr) {
        return Err(Error::validation(format!("AttributeName in {what} - {e}")));
    }
    if let Some(msg) = key_of(attr) {
        return Err(Error::validation(format!(
            "{what} AttributeName '{attr}' {msg}"
        )));
    }
    let dims = va
        .get("Dimensions")
        .and_then(Json::as_number)
        .filter(|n| !n.is_double())
        .and_then(|n| n.as_i64())
        .filter(|d| (1..=16000).contains(d));
    let Some(dims) = dims else {
        return Err(Error::validation(format!(
            "{what} Dimensions must be an integer between 1 and 16000."
        )));
    };
    let similarity = match v.get("SimilarityFunction") {
        None => "COSINE".to_string(),
        Some(Json::String(f)) if FUNCTIONS.contains(&f.as_str()) => f.clone(),
        Some(Json::String(f)) => {
            return Err(Error::validation(format!(
                "{what} SimilarityFunction '{f}' is not valid. Valid values are: EUCLIDEAN, COSINE, DOT_PRODUCT."
            )));
        }
        Some(_) => {
            return Err(Error::validation(format!(
                "Expected string value for attribute SimilarityFunction, got: {}",
                v.to_json_string()
            )));
        }
    };
    if let Some(p) = v.get("Projection") {
        if !p.is_object() {
            return Err(Error::validation(format!(
                "{what} Projection must be an object."
            )));
        }
        if p.get("ProjectionType").and_then(Json::as_str) != Some("KEYS_ONLY") {
            return Err(Error::validation(format!(
                "{what} Projection: only ProjectionType=KEYS_ONLY is currently supported."
            )));
        }
    }
    Ok(VectorIndex {
        name: name.to_string(),
        attribute: attr.to_string(),
        dimensions: dims as u32,
        similarity,
    })
}

/// CreateTable's `VectorIndexes` (`None` when absent or empty).
pub fn parse_create(
    body: &Json,
    key: &KeySchema,
    gsis: &[IndexSchema],
    lsis: &[IndexSchema],
    tags: &[(String, String)],
) -> Result<Option<Vec<VectorIndex>>> {
    let Some(v) = body.get("VectorIndexes") else {
        return Ok(None);
    };
    let Some(list) = v.as_array() else {
        return Err(Error::validation("VectorIndexes must be an array."));
    };
    if list.is_empty() {
        return Ok(None);
    }
    let key_of = |a: &str| {
        let k = key
            .attr(a)
            .or_else(|| gsis.iter().chain(lsis).find_map(|i| i.key.attr(a)))?;
        Some(format!(
            "is a key column of type {} so cannot be used as a vector index target.",
            k.ty.name()
        ))
    };
    // A name an LSI or GSI already has is refused before the definitions are checked.
    for d in list {
        if let Some(name) = d.get("IndexName").and_then(Json::as_str)
            && gsis.iter().chain(lsis).any(|g| g.name == name)
        {
            return Err(Error::validation(format!("Duplicate IndexName '{name}', ")));
        }
    }
    let mut out: Vec<VectorIndex> = Vec::with_capacity(list.len());
    for d in list {
        out.push(parse_definition(d, "VectorIndexes", false, &key_of)?);
    }
    for (i, x) in out.iter().enumerate() {
        let clash = out[..i].iter().any(|y| y.name == x.name)
            || gsis.iter().chain(lsis).any(|g| g.name == x.name);
        if clash {
            return Err(Error::validation(format!(
                "Duplicate IndexName '{}', ",
                x.name
            )));
        }
        if out[..i].iter().any(|y| y.attribute == x.attribute) {
            return Err(Error::validation(format!(
                "Duplicate vector index on the same AttributeName '{}'",
                x.attribute
            )));
        }
    }
    if initial_tablets(tags).is_none() {
        return Err(Error::validation(
            "Vector indexes are not supported on tables using vnodes.",
        ));
    }
    Ok(Some(out))
}

/// DescribeTable's `VectorIndexes`, in ScyllaDB's order ([`scylla_order`]).
pub fn describe(list: &[VectorIndex]) -> Json {
    let names: Vec<&str> = list.iter().map(|v| v.name.as_str()).collect();
    Json::Array(
        scylla_order(&names)
            .into_iter()
            .map(|i| &list[i])
            .map(|v| {
                Json::object()
                    .with("IndexName", Json::str(v.name.clone()))
                    .with(
                        "VectorAttribute",
                        Json::object()
                            .with("AttributeName", Json::str(v.attribute.clone()))
                            .with("Dimensions", Json::u64(v.dimensions as u64)),
                    )
                    .with(
                        "Projection",
                        Json::object().with("ProjectionType", Json::str("KEYS_ONLY")),
                    )
                    .with("SimilarityFunction", Json::str(v.similarity.clone()))
                    .with("IndexStatus", Json::str("CREATING"))
            })
            .collect(),
    )
}

/// The order ScyllaDB lists a table's vector indexes in (positions in `names`): its schema keeps
/// them in a `std::unordered_map` keyed by name, filled in name order when the schema loads, so
/// they come out in libstdc++'s iteration order for `std::hash<std::string_view>`. A key into
/// an empty bucket goes to the front of the list, otherwise to the front of its bucket's run;
/// the table has 13 buckets after the first insert and grows at load factor 1 (to 29, 59, 127,
/// …), relinking in list order. Measured on scylla 2026.3.3 for 2 to 30 indexes, created
/// together or one by one, and checked against libstdc++ itself.
pub fn scylla_order(names: &[&str]) -> Vec<usize> {
    const BUCKETS: [u64; 15] = [
        13, 29, 59, 127, 257, 541, 1109, 2357, 5087, 10273, 20753, 42043, 85229, 172933, 351061,
    ];
    let mut sorted: Vec<usize> = (0..names.len()).collect();
    sorted.sort_by(|&a, &b| names[a].as_bytes().cmp(names[b].as_bytes()));
    let hashes: Vec<u64> = names.iter().map(|n| hash_bytes(n.as_bytes())).collect();
    let insert = |list: &mut Vec<usize>, i: usize, nb: u64| {
        let b = hashes[i] % nb;
        let at = list.iter().position(|&j| hashes[j] % nb == b).unwrap_or(0);
        list.insert(at, i);
    };
    let (mut list, mut size) = (Vec::with_capacity(names.len()), 0);
    for i in sorted {
        if list.len() as u64 >= BUCKETS[size] && size + 1 < BUCKETS.len() {
            size += 1;
            let old = std::mem::take(&mut list);
            for j in old {
                insert(&mut list, j, BUCKETS[size]);
            }
        }
        insert(&mut list, i, BUCKETS[size]);
    }
    list
}

/// libstdc++'s `std::_Hash_bytes` with `std::hash<std::string_view>`'s seed (64-bit).
fn hash_bytes(b: &[u8]) -> u64 {
    const MUL: u64 = (0xc6a4_a793 << 32) + 0x5bd1_e995;
    let shift_mix = |v: u64| v ^ (v >> 47);
    let mut hash = 0xc70f_6907u64 ^ (b.len() as u64).wrapping_mul(MUL);
    let mut chunks = b.chunks_exact(8);
    for c in &mut chunks {
        let d = u64::from_le_bytes(c.try_into().unwrap_or_default());
        hash ^= shift_mix(d.wrapping_mul(MUL)).wrapping_mul(MUL);
        hash = hash.wrapping_mul(MUL);
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let d = rest.iter().rev().fold(0u64, |acc, &x| (acc << 8) | x as u64);
        hash ^= d;
        hash = hash.wrapping_mul(MUL);
    }
    shift_mix(shift_mix(hash).wrapping_mul(MUL))
}

/// UpdateTable's `VectorIndexUpdates`.
pub fn update(s: &mut TableSchema, u: &Json, with_gsi_updates: bool, now: i64) -> Result<()> {
    const W: &str = "VectorIndexUpdates";
    let Some(list) = u.as_array() else {
        return Err(Error::validation(format!("{W} must be an array")));
    };
    if list.len() > 1 {
        return Err(Error::new(
            ErrorKind::LimitExceeded,
            format!("{W} only allows one index creation or deletion"),
        ));
    }
    let one = || {
        Error::validation(format!(
            "{W} array must contain one object with a Create or Delete operation"
        ))
    };
    let Some(op) = list.first() else {
        return Err(one());
    };
    let Some([(kind, v)]) = op.as_object() else {
        return Err(one());
    };
    if !matches!(kind.as_str(), "Create" | "Delete") {
        return Err(Error::validation(format!(
            "{W} supports a Create or Delete operation, saw '{kind}'"
        )));
    }
    if !v.is_object() {
        return Err(Error::validation(format!("{W} entries must be objects")));
    }
    if kind == "Delete" {
        let Some(name) = v.get("IndexName").and_then(Json::as_str) else {
            return Err(Error::validation(format!(
                "{W} operation must have IndexName"
            )));
        };
        let Some(pos) = s.vector_indexes.iter().position(|x| x.name == name) else {
            return Err(Error::not_found(format!(
                "No vector index {name} in table {}",
                s.name
            )));
        };
        if with_gsi_updates {
            return Err(Error::new(
                ErrorKind::LimitExceeded,
                "UpdateTable cannot have both VectorIndexUpdates and GlobalSecondaryIndexUpdates in the same request",
            ));
        }
        s.vector_indexes.remove(pos);
        if s.vector_indexes.is_empty()
            && let Some(st) = s.stream.as_mut()
        {
            st.reported_disabled = false;
        }
        return Ok(());
    }
    let key = s.key.clone();
    let others: Vec<KeySchema> = s
        .gsis
        .iter()
        .chain(&s.lsis)
        .map(|i| i.key.clone())
        .collect();
    let key_of = |a: &str| {
        if key.contains(a) {
            return Some(
                "is a key column and cannot be used as a vector index target.".to_string(),
            );
        }
        others.iter().any(|k| k.contains(a)).then(|| {
            "is a key column of a GSI or LSI and cannot be used as a vector index target."
                .to_string()
        })
    };
    let d = parse_definition(v, W, true, &key_of)?;
    if s.gsis.iter().chain(&s.lsis).any(|g| g.name == d.name) {
        return Err(Error::validation(format!(
            "GSI or LSI {} already exists in table {}, cannot reuse the name for a vector index",
            d.name, s.name
        )));
    }
    if s.vector_indexes.iter().any(|x| x.name == d.name) {
        return Err(Error::validation(format!(
            "Vector index {} already exists in table {}",
            d.name, s.name
        )));
    }
    if s.vector_indexes.iter().any(|x| x.attribute == d.attribute) {
        return Err(Error::validation(format!(
            "{W} AttributeName '{}' is already the target of an existing vector index.",
            d.attribute
        )));
    }
    if with_gsi_updates {
        return Err(Error::new(
            ErrorKind::LimitExceeded,
            "UpdateTable cannot have both VectorIndexUpdates and GlobalSecondaryIndexUpdates in the same request",
        ));
    }
    if !s.tablets {
        return Err(Error::validation(
            "Vector indexes are not supported on tables using vnodes.",
        ));
    }
    if s.stream.is_none() {
        s.stream = Some(crate::schema::StreamSpec {
            view_type: "KEYS_ONLY".into(),
            created_at_ms: now,
            enabled: false,
            visible_at_ms: now,
            reported_disabled: false,
        });
    }
    s.vector_indexes.insert(0, d);
    Ok(())
}

/// A value as a list of 32-bit floats, for an indexed attribute. `Err` carries the message tail.
fn vector_floats(v: &Json, dims: u32, attr: &str) -> Result<()> {
    let bad = |m: String| Error::validation(format!("Vector index attribute '{attr}' {m}"));
    match single_member(v) {
        Some(("L", Json::Array(l))) => {
            if l.len() != dims as usize {
                return Err(bad(format!(
                    "must be a list or vector of exactly {dims} numbers, got {} elements",
                    l.len()
                )));
            }
            for e in l {
                let Some(("N", Json::String(n))) = single_member(e) else {
                    return Err(bad("must contain only numbers".into()));
                };
                if !fits_f32(n) {
                    return Err(bad(format!(
                        "element '{n}' cannot be represented as a 32-bit float"
                    )));
                }
            }
            Ok(())
        }
        Some(("FLOAT32VECTOR", Json::Array(l))) => {
            if l.len() != dims as usize {
                return Err(bad(format!(
                    "must be a list or vector of exactly {dims} numbers, got {} elements",
                    l.len()
                )));
            }
            Ok(())
        }
        Some((t, _)) => Err(bad(format!(
            "must be a list or vector of {dims} numbers, got type {t}"
        ))),
        None => Err(bad(format!(
            "must be a list or vector of {dims} numbers, got type "
        ))),
    }
}

/// Whether a number's text is a finite float32 that doesn't underflow to zero.
fn fits_f32(n: &str) -> bool {
    let Ok(num) = crate::number::Number::parse(n) else {
        return false;
    };
    let f = num.decimal().to_f32();
    f.is_finite() && (f != 0.0 || num.is_zero())
}

/// Validates the indexed attributes of an item about to be written.
pub fn check_item(s: &TableSchema, item: &crate::value::Item, _op: &str) -> Result<()> {
    for vi in &s.vector_indexes {
        if let Some(v) = item.get(&vi.attribute) {
            vector_floats(v, vi.dimensions, &vi.attribute)?;
        }
    }
    Ok(())
}

/// Query with `VectorSearch` (`Ok(None)` when the request has none).
pub async fn search(alt: &Alternator, req: &Request<'_>) -> Result<Option<Json>> {
    let body = req.body;
    let Some(vs) = body.get("VectorSearch") else {
        return Ok(None);
    };
    let schema = lookup_table(alt, table_name(body)?).await?;
    let Some(index) = body.get("IndexName").and_then(Json::as_str) else {
        return Err(Error::validation(
            "VectorSearch requires IndexName referring to a vector index",
        ));
    };
    let Some(vi) = schema.vector_indexes.iter().find(|v| v.name == index) else {
        return Err(Error::validation(format!(
            "VectorSearch IndexName '{index}' is not a vector index."
        )));
    };
    if !vs.is_object() {
        return Err(Error::validation(
            "VectorSearch requires a VectorSearch parameter",
        ));
    }
    let Some(qv) = vs.get("QueryVector") else {
        return Err(Error::validation(
            "VectorSearch requires a QueryVector parameter",
        ));
    };
    let elems: &[Json] = match single_member(qv) {
        Some(("L", Json::Array(l))) => l,
        Some(("FLOAT32VECTOR", Json::Array(l))) => l,
        Some(("FLOAT32VECTOR", _)) => {
            return Err(Error::validation(
                "VectorSearch QueryVector FLOAT32VECTOR must be a list of numbers",
            ));
        }
        Some(_) => {
            return Err(Error::validation(
                "VectorSearch QueryVector must be a list of numbers",
            ));
        }
        None => {
            return Err(Error::validation(
                "VectorSearch QueryVector must be a value with type 'FLOAT32VECTOR' or 'L'",
            ));
        }
    };
    if elems.len() != vi.dimensions as usize {
        return Err(Error::validation(format!(
            "VectorSearch QueryVector length {} does not match index Dimensions {}",
            elems.len(),
            vi.dimensions
        )));
    }
    let is_f32 = matches!(single_member(qv), Some(("FLOAT32VECTOR", _)));
    for e in elems {
        if is_f32 {
            let Json::Number(n) = e else {
                return Err(Error::validation(
                    "VectorSearch QueryVector must contain only numbers",
                ));
            };
            if !(n.as_f64() as f32).is_finite() {
                return Err(Error::validation(format!(
                    "VectorSearch QueryVector element '{}' cannot be represented as a 32-bit float",
                    n.text()
                )));
            }
        } else {
            let Some(("N", Json::String(n))) = single_member(e) else {
                return Err(Error::validation(
                    "VectorSearch QueryVector must contain only numbers",
                ));
            };
            if !fits_f32(n) {
                return Err(Error::validation(format!(
                    "VectorSearch QueryVector element '{n}' is not a valid number"
                )));
            }
        }
    }
    let limit_ok = body
        .get("Limit")
        .and_then(Json::as_number)
        .filter(|n| !n.is_double())
        .and_then(|n| n.as_i64());
    match limit_ok {
        Some(0) => return Err(Error::validation("Limit must be greater than 0")),
        Some(n) if n > 0 => {}
        _ => {
            return Err(Error::validation(
                "VectorSearch requires a positive integer Limit parameter",
            ));
        }
    }
    if consistent_read(body)? {
        return Err(Error::validation(
            "Consistent reads are not allowed on vector indexes",
        ));
    }
    if body.get("ExclusiveStartKey").is_some() {
        return Err(Error::validation(
            "VectorSearch does not support pagination (ExclusiveStartKey)",
        ));
    }
    if body.get("ScanIndexForward").is_some() {
        return Err(Error::validation(
            "VectorSearch does not support ScanIndexForward",
        ));
    }
    if let Some(Json::String(s)) = body.get("Select")
        && !matches!(
            s.as_str(),
            "ALL_ATTRIBUTES" | "SPECIFIC_ATTRIBUTES" | "ALL_PROJECTED_ATTRIBUTES" | "COUNT"
        )
    {
        return Err(Error::validation(format!(
            "Unknown Select value '{s}'. Allowed choices: ALL_ATTRIBUTES, SPECIFIC_ATTRIBUTES, ALL_PROJECTED_ATTRIBUTES, COUNT"
        )));
    }
    let has_projection =
        body.get("ProjectionExpression").is_some() || body.get("AttributesToGet").is_some();
    let select = body.get("Select").and_then(Json::as_str);
    if has_projection && !matches!(select, None | Some("SPECIFIC_ATTRIBUTES")) {
        return Err(Error::validation(
            "AttributesToGet or ProjectionExpression require Select to be either SPECIFIC_ATTRIBUTES or missing",
        ));
    }
    if select == Some("SPECIFIC_ATTRIBUTES") && !has_projection {
        return Err(Error::validation(
            "Select=SPECIFIC_ATTRIBUTES requires AttributesToGet or ProjectionExpression",
        ));
    }
    match vs.get("ReturnScores") {
        None => {}
        Some(Json::String(s)) if s == "NONE" => {}
        Some(Json::String(s)) if s == "SIMILARITY" => {
            if select == Some("COUNT") {
                return Err(Error::validation(
                    "VectorSearch ReturnScores=SIMILARITY cannot be used with Select=COUNT",
                ));
            }
        }
        Some(v) => {
            return Err(Error::validation(format!(
                "Unknown VectorSearch ReturnScores value {}. Allowed choices: NONE, SIMILARITY",
                v.to_json_string()
            )));
        }
    }
    let mut b = crate::expr::resolve::Bindings::new(body);
    super::item::projection(body, &mut b, "GetItem")?;
    if let Some(src) = super::item::expression_param(body, "FilterExpression")? {
        super::item::condition_param(src, "FilterExpression", &mut b)?;
    }
    b.check_unused(req.op)?;
    Err(Error::validation("Vector Store is disabled"))
}

/// A Query or Scan through a vector index's name without `VectorSearch`.
pub fn reject_index(s: &TableSchema, body: &Json) -> Result<()> {
    if let Some(index) = body.get("IndexName").and_then(Json::as_str)
        && s.vector_indexes.iter().any(|v| v.name == index)
    {
        return Err(Error::validation(format!(
            "IndexName '{index}' is a vector index for table '{}, so VectorSearch is mandatory in Query.",
            s.name
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libstdcxx_string_hash() {
        // `std::hash<std::string>{}(s)` printed by a g++ (libstdc++) build.
        for (s, h) in [
            ("", 6142509188972423790u64),
            ("abc", 3663726644998027833),
            ("idx00", 9529964531940244866),
            ("a longer name over 8 bytes", 12876300847071819917),
        ] {
            assert_eq!(hash_bytes(s.as_bytes()), h, "{s:?}");
        }
    }

    #[test]
    fn vector_index_order_like_scylla() {
        let order = |names: &[&str]| -> Vec<String> { scylla_order(names).into_iter().map(|i| names[i].to_string()).collect() };
        let idx = |n: usize| (0..n).map(|i| format!("idx{i:02}")).collect::<Vec<_>>();
        let split = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        // DescribeTable on scylladb/scylla:2026.3.3 (the golden files and probes).
        assert_eq!(order(&["bbb", "aaa", "ccc", "zzz", "mmm"]), split("zzz ccc bbb mmm aaa"));
        assert_eq!(order(&["ccc", "bbb", "aaa"]), split("ccc bbb aaa"));
        let x: Vec<String> = (1..=8).map(|i| format!("x{i:02}")).collect();
        let x: Vec<&str> = x.iter().map(String::as_str).collect();
        assert_eq!(order(&x), split("x08 x07 x05 x06 x04 x03 x02 x01"));
        let long = "y".repeat(192);
        assert_eq!(order(&["abc", &long, "A.b-c_9", "vi4"]), vec![long.clone(), "vi4".into(), "abc".into(), "A.b-c_9".into()]);
        for (n, want) in [
            (13, "idx11 idx12 idx10 idx09 idx08 idx04 idx03 idx06 idx02 idx07 idx05 idx01 idx00"),
            (14, "idx13 idx01 idx02 idx00 idx06 idx03 idx04 idx08 idx07 idx09 idx10 idx05 idx12 idx11"),
            (30, "idx11 idx26 idx12 idx05 idx10 idx09 idx07 idx08 idx04 idx23 idx22 idx06 idx03 idx02 idx29 idx01 idx25 idx13 idx14 idx21 idx00 idx16 idx18 idx15 idx19 idx20 idx17 idx27 idx24 idx28"),
        ] {
            let names = idx(n);
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            assert_eq!(order(&names), split(want), "{n}");
        }
        assert!(order(&[]).is_empty());
    }

    #[test]
    fn initial_tablets_like_scylla() {
        let t = |v: &str| initial_tablets(&[("system:initial_tablets".to_string(), v.to_string())]);
        // Captured from ScyllaDB 2026.3.3 (system_schema.scylla_keyspaces.initial_tablets).
        for (v, want) in [
            ("0", Some(0)),
            ("4", Some(4)),
            ("-1", Some(0)),
            ("-5", Some(0)),
            (" 5", Some(5)),
            ("+3", Some(3)),
            ("07", Some(7)),
            ("1e3", Some(1)),
            ("3.7", Some(3)),
            ("5abc", Some(5)),
            ("0x10", Some(0)),
            ("99999999999", Some(1215752191)),
            ("2147483648", Some(-2147483648)),
            ("4294967296", Some(0)),
            ("99999999999999999999", None),
            ("-99999999999999999999", None),
            ("abc", None),
            ("", None),
            ("none", None),
            (" ", None),
        ] {
            assert_eq!(t(v), want, "{v:?}");
        }
        assert_eq!(initial_tablets(&[]), Some(0));
    }
}
