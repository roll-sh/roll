//! A direct path from document bytes to a [`ParsedDocument`] for the common case, without the
//! intermediate JSON tree.
//!
//! The general path ([`super::parse_document`]) parses the source into a [`Value`] tree, runs
//! the dynamic-mapping pass over it and then walks it with an [`XParser`], cloning every string
//! value once more for each multi-field. For a bulk load of documents that only use mapped,
//! simple fields (most of them: logs, metrics, catalogue data) that is about half of the
//! preparation cost. [`FastPlan`] is compiled once per index mapping (per bulk request): the
//! leaf fields of the plain types (`text`, `keyword`, the integer types, `double`, `float`,
//! `half_float`, `scaled_float`, `boolean`, `date`/`date_nanos`, `ip`, `geo_point` as
//! `[lon, lat]`) with their multi-fields and the mapping parameters their parsers read, at the
//! root and in plain objects (not nested, enabled, with sub-objects: what ECS-style logs are
//! made of). [`FastPlan::parse`] then scans the document
//! bytes once and emits exactly the fields the general path would emit, in the same order.
//!
//! The fast path decides nothing on its own: it returns `None` (and the caller takes the
//! general path, which produces every error and every rarer behaviour) for anything outside
//! what it reproduces exactly — unknown or dotted keys, metadata fields, `null`,
//! strings for numeric fields, out-of-range numbers, out-of-range points, escapes in keys,
//! comments, duplicate keys, trailing content, invalid UTF-8. Values are converted with the
//! same functions the general parsers use (number text to `f64`, the field's date formatter).
//! Numbers indexed as text (a keyword or text field, a numeric field's keyword multi-field)
//! keep their text as written (`1.10`, `1E5`, `-0`), as Jackson's `text()` gives it to
//! Elasticsearch. `tests` checks the two paths agree field for field on generated documents.
//!
//! [`XParser`]: crate::query::XParser

use super::{DocField, FieldValue, ParsedDocument};
use crate::json::Value;
use crate::mapping::{FieldMapper, Mapper, MappingCtx};
use crate::meta::IndexMeta;
use crate::query::geo::GeoPoint;
use std::collections::HashMap;
use std::sync::Arc;

/// Deepest array nesting the fast path follows (the general path flattens any depth).
const MAX_ARRAY_DEPTH: usize = 8;

/// How one field's values are parsed.
#[derive(Clone)]
enum Kind {
    Text,
    /// `ignore_above` (effective, including the index default).
    Keyword(Option<i64>),
    Long,
    Integer,
    Short,
    Byte,
    Double,
    /// `float`, or `half_float` (rounded to half precision).
    Float { half: bool },
    /// A `scaled_float` with its scaling factor.
    ScaledFloat(f64),
    Boolean,
    Date { fmt: Arc<crate::time::DateFormatter>, nanos: bool },
    GeoPoint,
    /// `ip` from a string (anything else, and unparseable addresses, take the general path).
    Ip,
}

/// A field (or multi-field) the plan parses into.
struct Target {
    kind: Kind,
    /// The name its values carry (shared with the mapping).
    name: Arc<str>,
}

/// A field of the document (at the root or in a plain object).
struct Leaf {
    main: Target,
    /// Multi-fields, in mapping order.
    subs: Vec<Target>,
}

/// What the stored `_source` is for documents the plan parses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourcePlan {
    /// The document bytes as sent.
    Keep,
    /// Nothing (`_source` disabled).
    Drop,
}

/// What a key of an object is in the plan, with its position among the object's keys (for
/// duplicate detection).
#[derive(Clone, Copy)]
enum Entry {
    Leaf(usize, usize),
    /// A plain object (see [`plain_object`]): its scope.
    Object(usize, usize),
}

/// The root, or a plain object of the mapping: its keys.
struct Scope {
    by_name: HashMap<Box<str>, Entry, foldhash::fast::FixedState>,
}

/// Deepest object nesting the plan follows.
const MAX_OBJECT_DEPTH: usize = 16;

/// The compiled fast path of one index mapping.
pub(crate) struct FastPlan {
    leaves: Vec<Leaf>,
    /// The root's scope first.
    scopes: Vec<Scope>,
    pub(crate) source: SourcePlan,
    /// `index.mapping.array_objects.limit`: past it the general path fails the document.
    array_objects_limit: usize,
}

/// An object the general path walks without anything of its own: not nested, a pass-through
/// or disabled, with sub-objects, no `source_keep` of its own and no columnar flags.
fn plain_object(o: &crate::mapping::ObjectMapper) -> bool {
    matches!(o.kind, crate::mapping::ObjectKind::Object)
        && o.enabled != Some(false)
        && o.subobjects != Some(false)
        && o.source_keep.is_none()
        && o.disable_objects.is_none()
}

/// Field `f` at `full` with its multi-fields, when the plan parses all of them.
fn leaf_of(f: &FieldMapper, full: &str, ctx: &MappingCtx) -> Option<Leaf> {
    let kind = kind_of(f, ctx)?;
    let mut subs = Vec::with_capacity(f.fields.len());
    for (sub_name, sub) in &f.fields {
        match kind_of(sub, ctx) {
            // Multi-fields parse the parent's value; a point can't be one.
            Some(k) if !matches!(k, Kind::GeoPoint) => subs.push(Target { kind: k, name: sub.full_name(&format!("{full}.{sub_name}")) }),
            _ => return None,
        }
    }
    Some(Leaf { main: Target { kind, name: f.full_name(full) }, subs })
}

fn kind_of(f: &FieldMapper, ctx: &MappingCtx) -> Option<Kind> {
    if !f.copy_to.is_empty() || !f.props.is_empty() {
        return None;
    }
    Some(match f.ty {
        // `match_only_text` parses like `text` (only indexing differs).
        "text" | "match_only_text" => Kind::Text,
        "ip" => Kind::Ip,
        "keyword" => Kind::Keyword(f.effective("ignore_above", ctx).as_i64()),
        "long" => Kind::Long,
        "integer" => Kind::Integer,
        "short" => Kind::Short,
        "byte" => Kind::Byte,
        "double" => Kind::Double,
        "float" => Kind::Float { half: false },
        "half_float" => Kind::Float { half: true },
        "scaled_float" => Kind::ScaledFloat(super::values::scaling_factor(f, ctx)),
        "boolean" => Kind::Boolean,
        "date" | "date_nanos" => Kind::Date { fmt: f.date_formatter(ctx).ok()?, nanos: f.ty == "date_nanos" },
        // A `position` metric takes one point per document (checked by the general path).
        "geo_point" if f.fields.is_empty() && f.raw("time_series_metric").is_none() => Kind::GeoPoint,
        _ => return None,
    })
}

impl FastPlan {
    /// The plan for `idx`, or `None` when its documents always need the general path.
    pub(crate) fn new(idx: &IndexMeta) -> Option<FastPlan> {
        let ctx = idx.mapping_ctx();
        if ctx.columnar.is_some() || idx.mapping.data_stream_timestamp == Some(true) {
            return None;
        }
        let root = &idx.mapping.root;
        if root.enabled == Some(false) {
            return None;
        }
        let source = match crate::engine::synthetic::source_mode(idx) {
            crate::engine::synthetic::SourceMode::Disabled => SourcePlan::Drop,
            crate::engine::synthetic::SourceMode::Synthetic => return None,
            crate::engine::synthetic::SourceMode::Stored => {
                let sc = &idx.mapping.source;
                if sc.enabled == Some(false) {
                    SourcePlan::Drop
                } else if crate::opensearch::knn::derives_source(idx)
                    || sc.includes.as_ref().is_some_and(|v| !v.is_empty())
                    || sc.excludes.as_ref().is_some_and(|v| !v.is_empty())
                {
                    return None;
                } else {
                    SourcePlan::Keep
                }
            }
        };
        let mut plan = FastPlan { leaves: Vec::new(), scopes: Vec::new(), source, array_objects_limit: ctx.array_objects_limit.max(0) as usize };
        plan.scope(root, "", &ctx, 0);
        if plan.leaves.is_empty() {
            return None;
        }
        Some(plan)
    }

    /// Adds the scope of object `o` at `path` (`""`: the root) and returns its index: its leaf
    /// fields of the plain kinds and its plain objects.
    fn scope(&mut self, o: &crate::mapping::ObjectMapper, path: &str, ctx: &MappingCtx, depth: usize) -> usize {
        let at = self.scopes.len();
        self.scopes.push(Scope { by_name: HashMap::with_hasher(foldhash::fast::FixedState::default()) });
        let mut keys = 0;
        for (name, m) in &o.children {
            if name.trim().is_empty() || name.contains('.') || (path.is_empty() && crate::mapping::METADATA_FIELDS.contains(&name.as_str())) {
                continue;
            }
            let full = if path.is_empty() { name.clone() } else { format!("{path}.{name}") };
            let entry = match m {
                Mapper::Field(f) => {
                    let Some(leaf) = leaf_of(f, &full, ctx) else { continue };
                    self.leaves.push(leaf);
                    Entry::Leaf(self.leaves.len() - 1, keys)
                }
                Mapper::Object(child) if plain_object(child) && depth < MAX_OBJECT_DEPTH => Entry::Object(self.scope(child, &full, ctx, depth + 1), keys),
                _ => continue,
            };
            keys += 1;
            self.scopes[at].by_name.insert(name.as_str().into(), entry);
        }
        at
    }

    /// The parsed document, when the plan reproduces the general path for it exactly.
    pub(crate) fn parse(&self, source: &[u8]) -> Option<ParsedDocument> {
        let mut s = Scan { s: source, i: 0 };
        s.ws()?;
        if s.peek()? != b'{' {
            return None;
        }
        s.i += 1;
        let mut out = ParsedDocument { fields: Vec::with_capacity(self.leaves.len() * 2), ..Default::default() };
        let mut ignored: Vec<Arc<str>> = Vec::new();
        self.object(&mut s, 0, &mut out, &mut ignored, &mut 0)?;
        // Trailing content: only whitespace (the general parser ignores more, but rarely sees it).
        s.ws_end()?;
        if !ignored.is_empty() {
            let mut names: Vec<String> = ignored.iter().map(|n| n.to_string()).collect();
            names.sort();
            names.dedup();
            out.ignored = names;
        }
        Some(out)
    }

    /// The keys of the object (of `scope`) whose `{` the scanner just passed, through its `}`.
    /// Objects recurse no deeper than the plan's scopes do.
    fn object(&self, s: &mut Scan<'_>, scope: usize, out: &mut ParsedDocument, ignored: &mut Vec<Arc<str>>, arrays: &mut usize) -> Option<()> {
        let sc = &self.scopes[scope];
        let mut seen = SeenSet::new(sc.by_name.len());
        s.ws()?;
        if s.peek()? == b'}' {
            s.i += 1;
            return Some(());
        }
        loop {
            if s.peek()? != b'"' {
                return None;
            }
            let key = s.key()?;
            let entry = *sc.by_name.get(key)?;
            let (Entry::Leaf(_, k) | Entry::Object(_, k)) = entry;
            if !seen.insert(k) {
                // Duplicate keys fail in the general path's parser.
                return None;
            }
            s.ws()?;
            if s.peek()? != b':' {
                return None;
            }
            s.i += 1;
            s.ws()?;
            match entry {
                Entry::Leaf(li, _) => self.values(s, &self.leaves[li], out, ignored, 0)?,
                // An object, or an array of them (not `null` or a value).
                Entry::Object(child, _) => match s.peek()? {
                    b'{' => {
                        s.i += 1;
                        self.object(s, child, out, ignored, arrays)?;
                    }
                    b'[' => {
                        s.i += 1;
                        self.object_array(s, child, out, ignored, arrays, 1)?;
                    }
                    _ => return None,
                },
            }
            s.ws()?;
            match s.peek()? {
                b',' => {
                    s.i += 1;
                    s.ws()?;
                }
                b'}' => {
                    s.i += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// The objects of an array (of `scope`) whose `[` the scanner just passed, through its `]`:
    /// each counts toward `index.mapping.array_objects.limit`, inner arrays flatten, and a
    /// `null` or a value in it is left to the general path.
    fn object_array(&self, s: &mut Scan<'_>, scope: usize, out: &mut ParsedDocument, ignored: &mut Vec<Arc<str>>, arrays: &mut usize, depth: usize) -> Option<()> {
        s.ws()?;
        if s.peek()? == b']' {
            s.i += 1;
            return Some(());
        }
        loop {
            match s.peek()? {
                b'{' => {
                    *arrays += 1;
                    if *arrays > self.array_objects_limit {
                        return None;
                    }
                    s.i += 1;
                    self.object(s, scope, out, ignored, arrays)?;
                }
                b'[' if depth < MAX_ARRAY_DEPTH => {
                    s.i += 1;
                    self.object_array(s, scope, out, ignored, arrays, depth + 1)?;
                }
                _ => return None,
            }
            s.ws()?;
            match s.peek()? {
                b',' => {
                    s.i += 1;
                    s.ws()?;
                }
                b']' => {
                    s.i += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// The value(s) at the scanner: arrays hold several (except a point's `[lon, lat]`).
    fn values(&self, s: &mut Scan<'_>, leaf: &Leaf, out: &mut ParsedDocument, ignored: &mut Vec<Arc<str>>, depth: usize) -> Option<()> {
        if s.peek()? == b'[' {
            if matches!(leaf.main.kind, Kind::GeoPoint) && s.array_starts_with_number() {
                let p = s.lon_lat()?;
                out.fields.push(DocField { name: leaf.main.name.clone(), value: FieldValue::GeoPoint(p) });
                return Some(());
            }
            if depth >= MAX_ARRAY_DEPTH {
                return None;
            }
            s.i += 1;
            s.ws()?;
            if s.peek()? == b']' {
                s.i += 1;
                return Some(());
            }
            loop {
                self.values(s, leaf, out, ignored, depth + 1)?;
                s.ws()?;
                match s.peek()? {
                    b',' => {
                        s.i += 1;
                        s.ws()?;
                    }
                    b']' => {
                        s.i += 1;
                        return Some(());
                    }
                    _ => return None,
                }
            }
        }
        let tok = s.scalar()?;
        push(&leaf.main, &tok, out, ignored)?;
        for sub in &leaf.subs {
            push(sub, &tok, out, ignored)?;
        }
        Some(())
    }
}

/// Parses one scalar into `t`'s value (as `values::parse_non_null` does for these kinds).
/// Multi-fields read the same token as their parent (numbers as written).
fn push(t: &Target, tok: &Tok<'_>, out: &mut ParsedDocument, ignored: &mut Vec<Arc<str>>) -> Option<()> {
    let v = match (&t.kind, tok) {
        (Kind::Text, _) => FieldValue::Text(tok.text()),
        (Kind::Keyword(limit), _) => {
            let s = tok.text();
            if let Some(limit) = limit
                && (*limit < 0 || super::exceeds_ignore_above(&s, *limit as usize))
            {
                ignored.push(t.name.clone());
                return Some(());
            }
            FieldValue::Keyword(s)
        }
        (Kind::Long, Tok::Int(i, _)) => FieldValue::Long(*i),
        (Kind::Integer, Tok::Int(i, _)) => FieldValue::Long(i32::try_from(*i).ok()? as i64),
        (Kind::Short, Tok::Int(i, _)) => FieldValue::Long(i16::try_from(*i).ok()? as i64),
        (Kind::Byte, Tok::Int(i, _)) => FieldValue::Long(i8::try_from(*i).ok()? as i64),
        (Kind::Double, Tok::Int(i, _)) => FieldValue::Double(*i as f64),
        (Kind::Double, Tok::Float(d, _)) if d.is_finite() => FieldValue::Double(*d),
        // `XParser::float_value`: the number as a double, then a float.
        (Kind::Float { half }, Tok::Int(..) | Tok::Float(..)) => {
            let v = tok.number()? as f32;
            let stored = if *half { super::values::half_float(v) } else { v };
            if !stored.is_finite() {
                return None;
            }
            FieldValue::Float(stored)
        }
        (Kind::ScaledFloat(factor), Tok::Int(..) | Tok::Float(..)) => {
            let d = tok.number()?;
            if !d.is_finite() {
                return None;
            }
            FieldValue::Long(super::values::scale(d, *factor)?)
        }
        (Kind::Boolean, Tok::Bool(b)) => FieldValue::Boolean(*b),
        (Kind::Ip, Tok::Str(s)) => FieldValue::Ip(super::values::parse_ip(s)?),
        (Kind::Date { fmt, nanos }, Tok::Str(_) | Tok::Int(..) | Tok::Float(..)) => {
            let text: &str = match tok {
                Tok::Str(s) => s.as_ref(),
                Tok::Int(_, raw) | Tok::Float(_, raw) => raw,
                Tok::Bool(_) => return None,
            };
            let instant = fmt.parse_detailed(text, None, false).ok()?;
            if *nanos { FieldValue::Date(crate::time::to_epoch_nanos(instant).ok()?) } else { FieldValue::Date(instant.millis()) }
        }
        _ => return None,
    };
    out.fields.push(DocField { name: t.name.clone(), value: v });
    Some(())
}

/// Keys seen in the current object (duplicate detection).
struct SeenSet {
    small: u64,
    big: Vec<u64>,
}

impl SeenSet {
    fn new(n: usize) -> SeenSet {
        SeenSet { small: 0, big: if n > 64 { vec![0; n.div_ceil(64)] } else { Vec::new() } }
    }

    fn insert(&mut self, i: usize) -> bool {
        let (w, bit) = if self.big.is_empty() { (&mut self.small, i) } else { (&mut self.big[i / 64], i % 64) };
        let m = 1u64 << bit;
        let fresh = *w & m == 0;
        *w |= m;
        fresh
    }
}

/// A scalar token.
enum Tok<'a> {
    Str(std::borrow::Cow<'a, str>),
    /// The value and its text as written.
    Int(i64, &'a str),
    Float(f64, &'a str),
    Bool(bool),
}

impl Tok<'_> {
    /// A number token's value as a double (`XParser::double_value`).
    fn number(&self) -> Option<f64> {
        match self {
            Tok::Int(i, _) => Some(*i as f64),
            Tok::Float(d, _) => Some(*d),
            _ => None,
        }
    }

    /// `XParser::text`: strings as they are, numbers and booleans as Jackson renders them.
    /// Jackson's `text()`: numbers as written.
    fn text(&self) -> String {
        match self {
            Tok::Str(s) => s.to_string(),
            Tok::Int(_, raw) | Tok::Float(_, raw) => raw.to_string(),
            Tok::Bool(b) => b.to_string(),
        }
    }
}

/// A strict JSON scanner: anything the general parser would treat specially (or reject) ends
/// the fast path.
struct Scan<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Scan<'a> {
    #[inline]
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    #[inline]
    fn ws(&mut self) -> Option<()> {
        while let Some(&b) = self.s.get(self.i) {
            match b {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                // Comments.
                b'/' => return None,
                _ => return Some(()),
            }
        }
        Some(())
    }

    fn ws_end(&mut self) -> Option<()> {
        self.ws()?;
        (self.i == self.s.len()).then_some(())
    }

    /// The string at the scanner's `"`, without escapes; `None` if it has any. Returns the
    /// contents and moves past the closing quote.
    #[inline]
    fn plain_string(&mut self) -> Option<Result<&'a str, usize>> {
        let start = self.i + 1;
        let mut j = start;
        let s = self.s;
        while j < s.len() {
            let b = s[j];
            if b == b'"' {
                let text = std::str::from_utf8(&s[start..j]).ok()?;
                self.i = j + 1;
                return Some(Ok(text));
            }
            if b == b'\\' {
                return Some(Err(j));
            }
            if b < 0x20 {
                return None;
            }
            j += 1;
        }
        None
    }

    fn key(&mut self) -> Option<&'a str> {
        // Escaped names (rare) take the general path.
        self.plain_string()?.ok()
    }

    /// A string value; escaped ones are decoded by the general parser itself.
    fn string(&mut self) -> Option<std::borrow::Cow<'a, str>> {
        let open = self.i;
        let esc = match self.plain_string()? {
            Ok(s) => return Some(std::borrow::Cow::Borrowed(s)),
            Err(at) => at,
        };
        // Find the closing quote, skipping escaped characters.
        let mut j = esc;
        while j < self.s.len() {
            match self.s[j] {
                b'\\' => j += 2,
                b'"' => break,
                b if b < 0x20 => return None,
                _ => j += 1,
            }
        }
        if j >= self.s.len() {
            return None;
        }
        let Ok(Some(Value::Str(v))) = crate::json::parse(&self.s[open..=j]) else { return None };
        self.i = j + 1;
        Some(std::borrow::Cow::Owned(v))
    }

    fn scalar(&mut self) -> Option<Tok<'a>> {
        match self.peek()? {
            b'"' => Some(Tok::Str(self.string()?)),
            b't' => self.literal(b"true").map(|_| Tok::Bool(true)),
            b'f' => self.literal(b"false").map(|_| Tok::Bool(false)),
            b'-' | b'0'..=b'9' => self.number(),
            // `null` (null_value), objects and anything else.
            _ => None,
        }
    }

    fn literal(&mut self, lit: &[u8]) -> Option<()> {
        if !self.s[self.i..].starts_with(lit) {
            return None;
        }
        self.i += lit.len();
        // `truex` is a malformed token.
        match self.peek() {
            Some(b) if b.is_ascii_alphanumeric() || b == b'_' => None,
            _ => Some(()),
        }
    }

    /// A number in JSON's grammar (as the general parser reads it: `i64` integers, `f64`
    /// otherwise); integers beyond `i64` take the general path.
    fn number(&mut self) -> Option<Tok<'a>> {
        let s = self.s;
        let start = self.i;
        let mut j = start;
        if s.get(j) == Some(&b'-') {
            j += 1;
        }
        let digits_from = j;
        match s.get(j) {
            Some(b'0') => {
                j += 1;
                if s.get(j).is_some_and(u8::is_ascii_digit) {
                    return None;
                }
            }
            Some(b'1'..=b'9') => {
                while s.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                }
            }
            _ => return None,
        }
        let int_digits = j - digits_from;
        let mut float = false;
        if s.get(j) == Some(&b'.') {
            float = true;
            j += 1;
            if !s.get(j).is_some_and(u8::is_ascii_digit) {
                return None;
            }
            while s.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
        }
        if let Some(b'e' | b'E') = s.get(j) {
            float = true;
            j += 1;
            if let Some(b'+' | b'-') = s.get(j) {
                j += 1;
            }
            if !s.get(j).is_some_and(u8::is_ascii_digit) {
                return None;
            }
            while s.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
        }
        // A number running into a letter is malformed.
        if s.get(j).is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') || j - start > 300 {
            return None;
        }
        // SAFETY of the conversion: the bytes are ASCII digits and signs.
        let text = std::str::from_utf8(&s[start..j]).ok()?;
        self.i = j;
        if float {
            return Some(Tok::Float(text.parse::<f64>().ok()?, text));
        }
        if int_digits > 19 {
            return None;
        }
        Some(Tok::Int(text.parse::<i64>().ok()?, text))
    }

    /// Whether the array at the scanner starts with a number (a point's `[lon, lat]`).
    fn array_starts_with_number(&self) -> bool {
        let mut j = self.i + 1;
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.s.get(j) {
            j += 1;
        }
        matches!(self.s.get(j), Some(b'-' | b'0'..=b'9'))
    }

    /// `[lon, lat]` with both in range (`geo_point_array` plus `check_geo_point`).
    fn lon_lat(&mut self) -> Option<GeoPoint> {
        self.i += 1;
        self.ws()?;
        let lon = self.coord()?;
        self.ws()?;
        if self.peek()? != b',' {
            return None;
        }
        self.i += 1;
        self.ws()?;
        let lat = self.coord()?;
        self.ws()?;
        if self.peek()? != b']' {
            return None;
        }
        self.i += 1;
        ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some(GeoPoint { lat, lon })
    }

    fn coord(&mut self) -> Option<f64> {
        match self.number()? {
            Tok::Int(i, _) => Some(i as f64),
            Tok::Float(d, _) => Some(d),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
