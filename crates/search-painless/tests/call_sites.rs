//! Replays the captured `def` operator call-site fixture (`tests/fixtures/call_sites.json`,
//! from Elasticsearch 9.5.5's `_execute` API): each source runs a sequence of params in order,
//! and a source seen again continues where it left off, as Painless's linked call sites do.
//! The null-operand errors name the guard's parameter (`leftObject`, `rightObject`) once a site
//! is linked, `args[0]` while a unary site is not, and the operator's (`left`, `unary`, `o`)
//! after a guard miss relinked the site to the generic operator.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use search_painless::{Context, Engine, NoHost, Script, Settings};
use serde_json::Value as Json;

#[test]
fn def_operator_call_sites() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/call_sites.json");
    let cases: Json = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let mut scripts: HashMap<String, Arc<Script>> = HashMap::new();
    let mut failures = Vec::new();
    for c in cases.as_array().unwrap() {
        let src = c["source"].as_str().unwrap();
        let script = scripts.entry(src.to_string()).or_insert_with(|| engine.compile(src, Context::PainlessTest).unwrap()).clone();
        for run in c["runs"].as_array().unwrap() {
            let mut inst = script.instance();
            inst.set_params(&run["params"]);
            let have = match inst.execute(&mut NoHost) {
                Ok(v) => v.to_java_string(),
                Err(e) => e.cause_reason.clone().unwrap_or_default(),
            };
            let r = &run["result"];
            let want = r.get("result").or_else(|| r.get("err")).and_then(|x| x.as_str()).unwrap_or_default();
            if have != want {
                failures.push(format!("{src} {}: want {want:?}, got {have:?}", run["params"]));
            }
        }
    }
    for f in &failures {
        eprintln!("---- {f}");
    }
    assert!(failures.is_empty(), "{} call-site results differ from Elasticsearch", failures.len());
}
