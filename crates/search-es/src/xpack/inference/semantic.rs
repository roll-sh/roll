//! `semantic_text` fields as the inference API sees them: which endpoints a mapping uses.

use crate::mapping::{Mapper, Mapping, ObjectMapper};

/// The endpoint a `semantic_text` field uses when its mapping names none.
pub const DEFAULT_INFERENCE_ID: &str = ".elser-2-elasticsearch";

/// The `inference_id` of every `semantic_text` field of a mapping (one entry per field).
pub fn inference_ids(mapping: &Mapping) -> Vec<String> {
    let mut out = Vec::new();
    walk(&mapping.root, &mut out);
    out
}

fn walk(o: &ObjectMapper, out: &mut Vec<String>) {
    for m in o.children.values() {
        match m {
            Mapper::Object(c) => walk(c, out),
            Mapper::Field(f) if f.ty == "semantic_text" => {
                out.push(f.raw("inference_id").and_then(|v| v.as_str()).unwrap_or(DEFAULT_INFERENCE_ID).to_string());
            }
            _ => {}
        }
    }
}
