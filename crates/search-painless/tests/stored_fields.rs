//! `params._fields` of search scripts (`LeafStoredFieldsLookup` / `FieldLookup`), as
//! Elasticsearch 9.5.5 answers for a document with stored `k: ["b", "a"]`, `n: 5`, a mapped
//! but unstored `u`, and no field `nope`.

use search_painless::{Context, Engine, Host, HostError, Settings, Value};
use serde_json::{json, Value as Json};

struct Stored;

impl Host for Stored {
    fn stored_field(&mut self, field: &str) -> Result<Option<Vec<Value>>, HostError> {
        Ok(match field {
            "k" => Some(vec![Value::str("b"), Value::str("a")]),
            "n" => Some(vec![Value::Long(5)]),
            "u" => Some(Vec::new()),
            _ => None,
        })
    }
}

fn run(src: &str) -> Json {
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let script = engine.compile(src, Context::Field).unwrap();
    let mut inst = script.instance();
    inst.set_params(&json!({}));
    match inst.execute(&mut Stored) {
        Ok(v) => v.to_json(),
        Err(e) => {
            let r = e.to_response();
            json!({ "error": r["error"]["caused_by"], "stack": r["error"]["script_stack"] })
        }
    }
}

#[test]
fn stored_fields_lookup() {
    assert_eq!(run("params._fields['k'].value"), json!("b"));
    assert_eq!(run("params._fields['k'].values"), json!(["b", "a"]));
    assert_eq!(run("params._fields['n'].value"), json!(5));
    assert_eq!(run("params['_fields']['n'].values[0] + 1"), json!(6));
    assert_eq!(run("params._fields['k'].isEmpty()"), json!(false));
    assert_eq!(run("params._fields['u'].values"), json!([]));
    assert_eq!(run("params._fields['u'].value"), Json::Null);
    assert_eq!(run("params._fields.containsKey('k')"), json!(true));
    assert_eq!(run("params._fields.containsKey('nope')"), json!(false));
    assert_eq!(
        run("params._fields['nope'].values"),
        json!({
            "error": {"type": "illegal_argument_exception", "reason": "No field found for [nope] in mapping"},
            "stack": [
                "org.elasticsearch.server@9.5.5/org.elasticsearch.search.lookup.LeafStoredFieldsLookup.loadFieldData(LeafStoredFieldsLookup.java:118)",
                "org.elasticsearch.server@9.5.5/org.elasticsearch.search.lookup.LeafStoredFieldsLookup.get(LeafStoredFieldsLookup.java:47)",
                "org.elasticsearch.server@9.5.5/org.elasticsearch.search.lookup.LeafStoredFieldsLookup.get(LeafStoredFieldsLookup.java:21)",
                "params._fields['nope'].values",
                "              ^---- HERE"
            ]
        })
    );
    assert_eq!(
        run("params._fields.size()"),
        json!({
            "error": {"type": "unsupported_operation_exception", "reason": null},
            "stack": ["org.elasticsearch.server@9.5.5/org.elasticsearch.search.lookup.LeafStoredFieldsLookup.size(LeafStoredFieldsLookup.java:65)", "params._fields.size()", "              ^---- HERE"]
        })
    );
    assert_eq!(run("params._fields['k'].getName()")["error"]["reason"], "dynamic method [org.elasticsearch.search.lookup.FieldLookup, getName/0] not found");
}
