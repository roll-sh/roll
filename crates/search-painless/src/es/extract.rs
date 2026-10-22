//! `grok(...)` and `dissect(...)` of runtime-field scripts (`NamedGroupExtractor`).
//!
//! Both are compile-time-only in Elasticsearch: their arguments must be constants, and the
//! extractor is built while the script compiles — so a bad grok expression or dissect pattern
//! is a compile error ("error invoking [...] at compile time"), reported after every semantic
//! check passed. The analyzer calls [`build`] and keeps the result as a constant object of the
//! program; scripts then call `extract(String)` on it per document.
//!
//! Grok expressions use the `legacy` pattern bank, and any Joni warning (a redundant nested
//! repeat, an unknown escape) refuses the expression, as Elasticsearch's runtime fields do.

use std::sync::Arc;

use serde_json::{json, Value as Json};

use super::{es, server_frames, EsObj};
use crate::dissect::Dissect;
use crate::grok::{CaptureType, Grok, GrokValue, PatternBank};
use crate::lookup::Builder;
use crate::types::cls;
use crate::value::{JList, JMap, Value};
use crate::vm::{Throw, Thrown, Vm, VR};

/// A compiled `NamedGroupExtractor`.
#[derive(Debug)]
pub enum Extractor {
    Grok(Grok),
    Dissect(Dissect),
}

/// Builds the extractor of a `grok(pattern)` / `dissect(pattern[, appendSeparator])` call with
/// constant arguments, or the `caused_by` of the compile error Elasticsearch reports.
pub fn build(function: &str, args: &[String]) -> Result<Arc<Extractor>, Json> {
    match function {
        "grok" => {
            let pattern = &args[0];
            let fail = |reason: String, cause: Json| json!({ "type": "illegal_argument_exception", "reason": format!("error compiling grok pattern [{pattern}]: {reason}"), "caused_by": cause });
            match Grok::new(PatternBank::legacy(), pattern) {
                Ok(g) if g.warnings().is_empty() => Ok(Arc::new(Extractor::Grok(g))),
                Ok(g) => {
                    let msg = format!("emitted warnings: [{}]", g.warnings().join(", "));
                    Err(fail(msg.clone(), json!({ "type": "illegal_argument_exception", "reason": msg })))
                }
                Err(e) => Err(fail(e.message.clone(), json!({ "type": e.kind, "reason": e.message }))),
            }
        }
        _ => match Dissect::new(&args[0], args.get(1).map(String::as_str)) {
            Ok(d) => Ok(Arc::new(Extractor::Dissect(d))),
            Err(e) => Err(json!({ "type": e.kind, "reason": e.message })),
        },
    }
}

const GROK: &str = "org.elasticsearch.grok@9.5.5/org.elasticsearch.grok.";
const EXTRACT: &str = "org.elasticsearch.runtimefields@9.5.5/org.elasticsearch.runtimefields.NamedGroupExtractor$GrokHelper$1.extract(NamedGroupExtractor.java:108)";

fn grok_frames(t: &mut Throw, from_match: bool) {
    if from_match {
        t.frames.push(format!("{GROK}GrokCaptureType.lambda$rawExtracter$0(GrokCaptureType.java:79)"));
        t.frames.push(format!("{GROK}GrokCaptureExtracter$MapExtracter.lambda$extract$1(GrokCaptureExtracter.java:79)"));
        t.frames.push("java.base/java.util.ArrayList.forEach(ArrayList.java:1612)".into());
        t.frames.push(format!("{GROK}GrokCaptureExtracter$MapExtracter.extract(GrokCaptureExtracter.java:79)"));
        t.frames.push(format!("{GROK}Grok.match(Grok.java:247)"));
        t.frames.push(format!("{GROK}Grok.innerCaptures(Grok.java:219)"));
    } else {
        t.frames.push(format!("{GROK}Grok.innerCaptures(Grok.java:217)"));
    }
    t.frames.push(format!("{GROK}Grok.captures(Grok.java:200)"));
    t.frames.push(EXTRACT.into());
}

/// The exception of a capture whose text does not parse as its type, with the frames of the
/// JDK parser and of grok's extraction.
fn convert_error(ty: CaptureType, text: &str) -> Thrown {
    let s = Value::str(text);
    let (mut t, lambda) = match ty {
        CaptureType::Int => (crate::lib_lang::parse_failure(Some(text), 10, false, true), "GrokCaptureType$2.lambda$nativeExtracter$0(GrokCaptureType.java:30)"),
        CaptureType::Long => (crate::lib_lang::parse_failure(Some(text), 10, true, true), "GrokCaptureType$3.lambda$nativeExtracter$0(GrokCaptureType.java:36)"),
        CaptureType::Float => (crate::lib_lang::float_failure(&s, true), "GrokCaptureType$4.lambda$nativeExtracter$0(GrokCaptureType.java:42)"),
        _ => (crate::lib_lang::float_failure(&s, false), "GrokCaptureType$5.lambda$nativeExtracter$0(GrokCaptureType.java:48)"),
    };
    t.frames.push(format!("{GROK}{lambda}"));
    grok_frames(&mut t, true);
    t
}

fn value_of(v: GrokValue) -> Value {
    match v {
        GrokValue::Str(s) => Value::string(s),
        GrokValue::Int(i) => Value::Int(i),
        GrokValue::Long(l) => Value::Long(l),
        GrokValue::Float(f) => Value::Float(f),
        GrokValue::Double(d) => Value::Double(d),
        GrokValue::Bool(b) => Value::Bool(b),
        GrokValue::List(l) => JList::array_list(l.into_iter().map(value_of).collect()),
    }
}

/// `NamedGroupExtractor.extract(String)`: the captures as a map, or null when nothing matches.
fn ng_extract(_: &mut Vm, a: &mut [Value]) -> VR {
    let Some(EsObj::Extractor(x)) = es(&a[0]) else {
        return Err(crate::vm::cce("not a NamedGroupExtractor"));
    };
    let text = match &a[1] {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    };
    match &**x {
        Extractor::Grok(g) => {
            let Some(text) = text else {
                let mut t = Throw::new(cls::NULL_POINTER_EXCEPTION, Some("Cannot invoke \"String.getBytes(java.nio.charset.Charset)\" because \"text\" is null".into()));
                grok_frames(&mut t, false);
                return Err(t);
            };
            match g.captures(text) {
                Ok(None) => Ok(Value::Null),
                Ok(Some(fields)) => {
                    let m = JMap::new(cls::LINKED_HASH_MAP);
                    for (k, v) in fields {
                        m.insert_raw(Value::string(k), value_of(v));
                    }
                    Ok(Value::Map(std::rc::Rc::new(m)))
                }
                Err(e) => Err(convert_error(e.ty, &e.text)),
            }
        }
        Extractor::Dissect(d) => Ok(match d.parse(text) {
            None => Value::Null,
            Some(fields) => {
                // `new HashMap<>(maxResults)`: the table size decides the iteration order
                let m = JMap::new(cls::HASH_MAP);
                m.data.borrow_mut().init_cap = crate::value::table_size_for(d.max_results() as u32);
                for (k, v) in fields {
                    m.insert_raw(Value::string(k), Value::string(v));
                }
                Value::Map(std::rc::Rc::new(m))
            }
        }),
    }
}

/// The context functions only ever run at compile time (the analyzer replaces their calls).
fn compile_time_only(_: &mut Vm, _: &mut [Value]) -> VR {
    Err(crate::vm::iae("grok and dissect are evaluated at compile time"))
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.supers(NAMED_GROUP_EXTRACTOR, &[OBJECT]);
    b.m(NAMED_GROUP_EXTRACTOR, "Map extract(String)", ng_extract);
    for c in [BOOLEAN_FIELD_SCRIPT, DATE_FIELD_SCRIPT, DOUBLE_FIELD_SCRIPT, GEO_POINT_FIELD_SCRIPT, GEOMETRY_FIELD_SCRIPT, IP_FIELD_SCRIPT, STRING_FIELD_SCRIPT, LONG_FIELD_SCRIPT, COMPOSITE_FIELD_SCRIPT] {
        b.s(c, "org.elasticsearch.runtimefields.NamedGroupExtractor grok(String)", compile_time_only);
        b.s(c, "org.elasticsearch.runtimefields.NamedGroupExtractor dissect(String)", compile_time_only);
        b.s(c, "org.elasticsearch.runtimefields.NamedGroupExtractor dissect(String,String)", compile_time_only);
    }
    let _ = server_frames;
}
