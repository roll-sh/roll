//! Document parsing (`DocumentParser`): a JSON document against an index mapping, to the typed
//! field values an index writes, with Elasticsearch's validation and exceptions.
//!
//! The walk follows the document's token stream through [`XParser`], so the first problem in
//! document order is the one reported, at Elasticsearch's `[line:col]`:
//!
//! - objects descend into object mappers (dotted names resolve through `subobjects`), `nested`
//!   objects become separate [`NestedDoc`]s, `enabled: false` objects are skipped unread;
//! - each concrete value goes through its field type's parser ([`values`]), then to the field's
//!   multi-fields and `copy_to` targets; `null` takes the `null_value`; malformed values of
//!   fields with `ignore_malformed` (and over-long keywords) are recorded in `_ignored`;
//! - unmapped fields follow the effective `dynamic` setting: the mapping update is computed by
//!   [`crate::mapping::dynamic::dynamic_update`] and merged first, so new fields parse like
//!   mapped ones; `strict` fails at the first unmapped field.
//!
//! Failures inside a field parser are wrapped like `FieldMapper.parse` wraps them: `failed to
//! parse field [f] of type [t] in document with id 'x'. Preview of field's value: 'v'`, the
//! preview being the value (objects read as Java maps, which moves the location to their end).
//!
//! The output is engine-neutral ([`ParsedDocument`]): the search phase turns it into index
//! writes, applying analyzers and normalizers.

pub mod values;

use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::mapping::{Dynamic, FieldMapper, Mapping, MappingCtx, MapperRef, MergeReason, ObjectMapper};
use crate::query::{Token, XParser};
use std::collections::BTreeSet;
use std::sync::Arc;
pub use values::FieldValue;

/// One indexed value of a field.
#[derive(Clone, Debug, PartialEq)]
pub struct DocField {
    /// The full field path (`a.b`, multi-fields as `title.raw`), shared with the mapping.
    pub name: Arc<str>,
    pub value: FieldValue,
}

/// A nested object, indexed as its own (hidden) document next to the root one.
#[derive(Clone, Debug, PartialEq)]
pub struct NestedDoc {
    /// The nested object's path (`_nested_path`).
    pub path: String,
    pub fields: Vec<DocField>,
}

/// The result of parsing one document.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ParsedDocument {
    pub fields: Vec<DocField>,
    /// Nested documents in the order their objects ended (children before parents, as Lucene
    /// blocks need them).
    pub nested: Vec<NestedDoc>,
    /// Field names recorded in `_ignored`, sorted.
    pub ignored: Vec<String>,
    /// `_doc_count`, when the document sets it.
    pub doc_count: Option<i64>,
    /// The mapping update the document's dynamic fields need (to merge and publish).
    pub dynamic_update: Option<Mapping>,
}

/// Parses `source` (the raw request body, for error locations) whose parsed form is `doc`.
/// `id` names the document in messages (`document with id 'x'`).
pub fn parse_document(mapping: &Mapping, ctx: &MappingCtx, doc: &Value, source: Option<&[u8]>, id: Option<&str>) -> EsResult<ParsedDocument> {
    let Value::Obj(doc_map) = doc else {
        return Err(EsError::document_parsing("failed to parse, document is empty"));
    };
    let desc = match id {
        Some(i) => format!("document with id '{i}'"),
        None => "document with id 'null'".to_string(),
    };
    // Dynamic fields: compute the mapping update first, then parse against the merged mapping.
    // A `strict` violation is reported where the walk meets the field, so that earlier value
    // errors come first, as in Elasticsearch's single pass.
    // Other failures of the dynamic pass (field name rules) are reported only when the walk
    // doesn't find them first, located.
    let mut deferred = None;
    let (effective, update, strict_pending) = match crate::mapping::dynamic::dynamic_update(mapping, doc_map, ctx) {
        Ok(Some(u)) => {
            let merged = mapping.merge(&u, ctx, MergeReason::MappingUpdate)?;
            (std::borrow::Cow::Owned(merged), Some(u), false)
        }
        Ok(None) => (std::borrow::Cow::Borrowed(mapping), None, false),
        Err(crate::mapping::dynamic::DynamicError::Strict { .. }) => (std::borrow::Cow::Borrowed(mapping), None, true),
        Err(crate::mapping::dynamic::DynamicError::Other(e)) => {
            deferred = Some(e);
            (std::borrow::Cow::Borrowed(mapping), None, false)
        }
    };
    let mut p = Walker {
        mapping: &effective,
        ctx,
        desc,
        p: XParser::new(doc, source, Vec::new()),
        out: ParsedDocument::default(),
        ignored: BTreeSet::new(),
        nested: Vec::new(),
        strict_pending,
    };
    p.p.next_token();
    let root_dynamic = effective.root.dynamic.unwrap_or(Dynamic::True);
    p.object(&effective.root, "", root_dynamic, true)?;
    if let Some(e) = deferred {
        return Err(e);
    }
    let mut out = p.out;
    out.ignored = p.ignored.into_iter().collect();
    out.dynamic_update = update;
    Ok(out)
}

/// How a field parser's failure is reported.
pub(crate) enum Wrap {
    /// `failed to parse field [f] of type [t] in <doc>. Preview of field's value: 'v'`.
    Field,
    /// `failed to parse: <cause message>`.
    Plain,
    /// `failed to parse field [f] of type [t]` (the point and shape mappers' own wrapper).
    Typed,
    /// Already a `document_parsing_exception` (or another final exception).
    None,
}

/// A field parser failure.
pub(crate) struct ValueError {
    pub cause: EsError,
    pub wrap: Wrap,
}

impl From<EsError> for ValueError {
    fn from(cause: EsError) -> Self {
        ValueError { cause, wrap: Wrap::Field }
    }
}

impl ValueError {
    pub(crate) fn plain(cause: EsError) -> ValueError {
        ValueError { cause, wrap: Wrap::Plain }
    }
    pub(crate) fn typed(cause: EsError) -> ValueError {
        ValueError { cause, wrap: Wrap::Typed }
    }
    pub(crate) fn none(cause: EsError) -> ValueError {
        ValueError { cause, wrap: Wrap::None }
    }
}

struct Walker<'m, 'p> {
    mapping: &'m Mapping,
    ctx: &'m MappingCtx,
    desc: String,
    p: XParser<'p>,
    out: ParsedDocument,
    ignored: BTreeSet<String>,
    /// Indexes into `out.nested` of the nested documents being filled (innermost last).
    nested: Vec<NestedDoc>,
    strict_pending: bool,
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() { name.to_string() } else { format!("{path}.{name}") }
}

impl<'m, 'p> Walker<'m, 'p> {
    fn doc_error(&self, msg: impl Into<String>) -> EsError {
        let (l, c) = self.p.token_location();
        EsError::document_parsing(format!("[{l}:{c}] {}", msg.into()))
    }

    fn push(&mut self, name: &Arc<str>, value: FieldValue) {
        let f = DocField { name: name.clone(), value };
        match self.nested.last_mut() {
            Some(n) => n.fields.push(f),
            None => self.out.fields.push(f),
        }
    }

    /// A `copy_to` value: it goes to the innermost document whose path prefixes the target
    /// (`DocumentParser.parseCopyFields` walks `doc.getParent()` until
    /// `field.startsWith(doc.getPrefix())`), so a nested field copying to a root field lands in
    /// the root document.
    fn push_copy(&mut self, target: &Arc<str>, value: FieldValue) {
        let f = DocField { name: target.clone(), value };
        let doc = self.nested.iter_mut().rev().find(|n| {
            target.len() > n.path.len() && target.as_bytes()[n.path.len()] == b'.' && target.starts_with(n.path.as_str())
        });
        match doc {
            Some(n) => n.fields.push(f),
            None => self.out.fields.push(f),
        }
    }

    /// Validates a field name as `DocumentParser` does.
    fn check_name(&self, name: &str) -> EsResult<()> {
        if name.is_empty() {
            return Err(self.doc_error("failed to parse: field name cannot be an empty string").caused_by(EsError::illegal_argument("field name cannot be an empty string")));
        }
        if name.trim().is_empty() {
            return Err(self.doc_error(format!("Field name cannot contain only whitespace: [{name}]")));
        }
        if name.contains('.') && name.split('.').any(|p| p.trim().is_empty()) {
            let msg = format!("field name cannot contain only whitespace: ['{name}']");
            return Err(self.doc_error(format!("failed to parse: {msg}")).caused_by(EsError::illegal_argument(msg)));
        }
        Ok(())
    }

    /// Walks the object the parser is on (its start token).
    fn object(&mut self, om: &'m ObjectMapper, path: &str, dynamic: Dynamic, root: bool) -> EsResult<()> {
        while let Some(t) = self.p.next_token() {
            match t {
                Token::EndObject => break,
                Token::FieldName => {
                    let name = self.p.current_name().unwrap_or("").to_string();
                    let name_loc = self.p.token_location();
                    self.check_name(&name)?;
                    let value_token = self.p.next_token().unwrap_or(Token::Null);
                    if root && crate::mapping::METADATA_FIELDS.contains(&name.as_str()) {
                        self.metadata_field(&name, name_loc)?;
                        continue;
                    }
                    self.field(om, path, &name, value_token, dynamic)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A metadata field inside the document: only `_doc_count` is allowed.
    fn metadata_field(&mut self, name: &str, _name_loc: (usize, usize)) -> EsResult<()> {
        if name == "_doc_count" {
            let v = match self.p.token() {
                Token::Number => self.p.long_value()?,
                _ => {
                    return Err(self.doc_error(format!("Field [{name}] must be a positive integer")));
                }
            };
            if v <= 0 {
                return Err(EsError::illegal_argument(format!("Field [_doc_count] must be a positive integer. Value [{v}] is not allowed.")));
            }
            self.out.doc_count = Some(v);
            return Ok(());
        }
        let (il, ic) = self.p.token_location();
        let cause = EsError::document_parsing(format!(
            "[{il}:{ic}] Field [{name}] is a metadata field and cannot be added inside a document. Use the index API request parameters."
        ));
        let preview = self.preview();
        let (l, c) = self.p.token_location();
        Err(EsError::document_parsing(format!(
            "[{l}:{c}] failed to parse field [{name}] of type [{name}] in {}. Preview of field's value: '{preview}'",
            self.desc
        ))
        .caused_by(cause))
    }

    /// The value preview `FieldMapper.parse` puts in messages: the value as Java prints it;
    /// objects and arrays are read whole (the parser moves to their end).
    fn preview(&mut self) -> String {
        match self.p.current_token() {
            Some(Token::StartObject) => match self.p.map() {
                Ok(m) => crate::json::java_map_string(&m),
                Err(_) => "null".into(),
            },
            Some(Token::StartArray) => match self.p.list() {
                Ok(l) => format!("[{}]", l.iter().map(Value::to_es_string).collect::<Vec<_>>().join(", ")),
                Err(_) => "null".into(),
            },
            Some(Token::String | Token::Number | Token::Boolean) => self.p.value().to_es_string(),
            _ => "null".into(),
        }
    }

    /// One field of an object, positioned on its value.
    fn field(&mut self, om: &'m ObjectMapper, path: &str, name: &str, token: Token, dynamic: Dynamic) -> EsResult<()> {
        let full = join(path, name);
        // Resolved from the enclosing object mapper (`ObjectMapper.getMapper`), not from the root.
        let mapper = crate::mapping::lookup_in(om, name);
        match mapper {
            Some(MapperRef::Object(child)) => {
                crate::stack::maybe_grow(|| self.object_value(child, &full, name, token, child.dynamic.unwrap_or(dynamic)))
            }
            Some(MapperRef::Field(f)) => self.field_values(f, &full, token),
            Some(MapperRef::Alias(_)) => Err(self.doc_error(format!("Cannot write to a field alias [{full}]."))),
            None => {
                // Dotted names inside an object resolve against the intermediate objects.
                if name.contains('.') && om.subobjects_enabled() {
                    let (head, rest) = name.split_once('.').unwrap_or((name, ""));
                    let head_full = join(path, head);
                    if let Some(MapperRef::Object(child)) = crate::mapping::lookup_in(om, head) {
                        return self.field(child, &head_full, rest, token, child.dynamic.unwrap_or(dynamic));
                    }
                }
                self.unmapped(path, name, token, dynamic)
            }
        }
    }

    /// A field the mapping doesn't know (after the dynamic update): skipped, or `strict`.
    fn unmapped(&mut self, path: &str, name: &str, _token: Token, dynamic: Dynamic) -> EsResult<()> {
        if self.strict_pending && dynamic.as_str().starts_with("strict") {
            let (l, c) = self.p.token_location();
            let within = if path.is_empty() { "_doc".to_string() } else { path.to_string() };
            let leaf = name.rsplit('.').next().unwrap_or(name);
            return Err(EsError::new(
                400,
                "strict_dynamic_mapping_exception",
                format!("[{l}:{c}] mapping set to {}, dynamic introduction of [{leaf}] within [{within}] is not allowed", dynamic.as_str()),
            ));
        }
        // dynamic: false (or runtime): kept in _source only.
        self.p.skip_children();
        Ok(())
    }

    /// An object mapper's value: an object, an array of objects, or null.
    fn object_value(&mut self, child: &'m ObjectMapper, full: &str, name: &str, token: Token, dynamic: Dynamic) -> EsResult<()> {
        match token {
            Token::StartObject => {
                if !child.is_enabled() {
                    self.p.skip_children();
                    return Ok(());
                }
                if child.is_nested() {
                    self.nested.push(NestedDoc { path: full.to_string(), fields: Vec::new() });
                    let r = self.object(child, full, dynamic, false);
                    let doc = self.nested.pop().unwrap_or(NestedDoc { path: full.to_string(), fields: Vec::new() });
                    r?;
                    self.nested_done(child, doc);
                    Ok(())
                } else {
                    self.object(child, full, dynamic, false)
                }
            }
            Token::StartArray => {
                if !child.is_enabled() {
                    self.p.skip_children();
                    return Ok(());
                }
                while let Some(t) = self.p.next_token() {
                    match t {
                        Token::EndArray => break,
                        Token::Null => {}
                        t => crate::stack::maybe_grow(|| self.object_value(child, full, name, t, dynamic))?,
                    }
                }
                Ok(())
            }
            Token::Null => Ok(()),
            _ => {
                if !child.is_enabled() {
                    return Ok(());
                }
                // Inside an array the parser has no current field name.
                let shown = self.p.current_name().unwrap_or("null").to_string();
                let _ = name;
                Err(self.doc_error(format!("object mapping for [{full}] tried to parse field [{shown}] as object, but found a concrete value")))
            }
        }
    }

    /// A finished nested document: added, and its fields copied to the parent or root when the
    /// mapping asks for it.
    fn nested_done(&mut self, child: &ObjectMapper, doc: NestedDoc) {
        if let crate::mapping::ObjectKind::Nested { include_in_parent, include_in_root } = &child.kind {
            let to_parent = include_in_parent.unwrap_or(false);
            let to_root = include_in_root.unwrap_or(false);
            if to_parent {
                for f in &doc.fields {
                    match self.nested.last_mut() {
                        Some(n) => n.fields.push(f.clone()),
                        None => self.out.fields.push(f.clone()),
                    }
                }
            }
            if to_root && !(to_parent && self.nested.is_empty()) {
                self.out.fields.extend(doc.fields.iter().cloned());
            }
        }
        self.out.nested.push(doc);
    }

    /// A field mapper's value(s): arrays hold several values, except for types whose value is
    /// itself an array (geo points as `[lon, lat]`, vectors).
    fn field_values(&mut self, f: &'m FieldMapper, full: &str, token: Token) -> EsResult<()> {
        if token == Token::StartArray && !values::takes_array(f, &self.p) {
            while let Some(t) = self.p.next_token() {
                match t {
                    Token::EndArray => break,
                    t => crate::stack::maybe_grow(|| self.field_values(f, full, t))?,
                }
            }
            return Ok(());
        }
        self.field_value(f, full)
    }

    /// One value of a field mapper, with its multi-fields and `copy_to` targets.
    fn field_value(&mut self, f: &'m FieldMapper, full: &str) -> EsResult<()> {
        let token = self.p.token();
        let multi = token != Token::StartObject && !f.fields.is_empty();
        // The value is kept for the multi-fields and copy_to targets only when there are some.
        let snapshot = (multi || !f.copy_to.is_empty()).then(|| self.p.value().clone());
        let r = {
            let mut cx = values::Cx { p: &mut self.p, ctx: self.ctx, ignored: &mut self.ignored, desc: &self.desc };
            values::parse_value(&mut cx, f, full)
        };
        match r {
            Ok(vals) => {
                let name = f.full_name(full);
                for v in vals {
                    self.push(&name, v);
                }
            }
            Err(e) => return Err(self.wrap(e, f, full)),
        }
        let Some(snapshot) = snapshot else { return Ok(()) };
        // Multi-fields and copy_to targets parse the same value.
        if multi {
            let mut path = String::new();
            for (sub_name, sub) in &f.fields {
                // `full.sub`, built only until the sub-field has cached it.
                let name = match sub.full_name.get() {
                    Some(n) if n.len() == full.len() + 1 + sub_name.len() && n.starts_with(full) && n.ends_with(sub_name.as_str()) => n.clone(),
                    _ => {
                        path.clear();
                        path.push_str(full);
                        path.push('.');
                        path.push_str(sub_name);
                        sub.full_name(&path)
                    }
                };
                let vals = {
                    let mut cx = values::Cx { p: &mut self.p, ctx: self.ctx, ignored: &mut self.ignored, desc: &self.desc };
                    values::parse_detached(&mut cx, sub, &name, &snapshot, token)?
                };
                for v in vals.unwrap_or_default() {
                    self.push(&name, v);
                }
            }
        }
        for target in &f.copy_to {
            let Some(MapperRef::Field(tf)) = self.mapping.lookup(target) else { continue };
            let vals = {
                let mut cx = values::Cx { p: &mut self.p, ctx: self.ctx, ignored: &mut self.ignored, desc: &self.desc };
                values::parse_detached(&mut cx, tf, target, &snapshot, token)?
            };
            let name = tf.full_name(target);
            for v in vals.unwrap_or_default() {
                self.push_copy(&name, v);
            }
        }
        Ok(())
    }

    /// Wraps a field parser failure (`FieldMapper.parse`).
    fn wrap(&mut self, e: ValueError, f: &FieldMapper, full: &str) -> EsError {
        match e.wrap {
            Wrap::None => e.cause,
            Wrap::Plain => {
                let msg = e.cause.reason().to_string();
                self.doc_error(format!("failed to parse: {msg}")).caused_by(e.cause)
            }
            Wrap::Typed => self.doc_error(format!("failed to parse field [{full}] of type [{}]", f.ty)).caused_by(e.cause),
            Wrap::Field => {
                // Jackson hands back the number as the type last read: floats print as floats.
                let preview = match (f.ty, self.p.current_token(), self.p.value()) {
                    ("float" | "half_float", Some(Token::Number), Value::Num(crate::json::Number::Float(d))) => crate::json::write::java_float(*d as f32),
                    _ => self.preview(),
                };
                let (l, c) = self.p.token_location();
                EsError::document_parsing(format!(
                    "[{l}:{c}] failed to parse field [{full}] of type [{}] in {}. Preview of field's value: '{preview}'",
                    f.ty, self.desc
                ))
                .caused_by(e.cause)
            }
        }
    }
}

#[cfg(test)]
mod tests;
