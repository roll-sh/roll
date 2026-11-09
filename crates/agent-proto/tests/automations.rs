//! The automation wire types' fixtures (`fixtures/automation*.json`), parsed by the dashboard too
//! (`apps/dashboard/src/lib/automations-api.test.ts`). `ROLL_FIXTURES=write cargo test -p
//! agent-proto` rewrites them. The `covered_*` matches are exhaustive: a new variant doesn't
//! compile until it has a sample, and the tag counts below catch a variant listed but unsampled.

mod automation_samples;

use agent_proto::automation::*;
use automation_samples as s;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeSet;

fn check<V: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(name: &str, values: Vec<V>) {
    for v in &values {
        let text = serde_json::to_string(v).unwrap();
        let back: V = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {text} doesn't parse back: {e}"));
        assert_eq!(&back, v, "{name}: round trip changed {text}");
    }
    let text = serde_json::to_string_pretty(&values).unwrap() + "\n";
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(format!("{name}.json"));
    if std::env::var("ROLL_FIXTURES").as_deref() == Ok("write") {
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        have == text,
        "fixtures/{name}.json is stale: run `ROLL_FIXTURES=write cargo test -p agent-proto` and update \
         apps/dashboard/src/lib/automations-api.ts to match"
    );
}

fn covered_trigger(t: &Trigger) {
    match t {
        Trigger::Manual | Trigger::Schedule { .. } | Trigger::Interval { .. } | Trigger::Webhook | Trigger::Code { .. } | Trigger::Agent { .. } | Trigger::Integration { .. } => {}
    }
}

fn covered_target(t: &Target) {
    match t {
        Target::Machine { .. } | Target::Sandbox { .. } | Target::AnyOnline | Target::ProjectMachine => {}
    }
}

fn covered_project(p: &ProjectRef) {
    match p {
        ProjectRef::Id { .. } | ProjectRef::Repo { .. } => {}
    }
}

fn values<T: Serialize>(items: impl IntoIterator<Item = T>, key: Option<&str>) -> BTreeSet<String> {
    items
        .into_iter()
        .map(|i| {
            let v = serde_json::to_value(i).unwrap();
            match key {
                Some(k) => v[k].as_str().unwrap().to_string(),
                None => v.as_str().unwrap().to_string(),
            }
        })
        .collect()
}

#[test]
fn fixtures() {
    let a = s::automations();
    for x in &a {
        covered_trigger(&x.spec.trigger);
        if let Some(t) = &x.spec.defaults.target {
            covered_target(t);
        }
        if let Some(p) = &x.spec.defaults.project {
            covered_project(p);
        }
    }
    check("automations", a);
    check("automation_saved", s::saved());
    check("automation_runs", s::runs());
    check("automation_notices", s::notices());
    check("automation_catalogs", vec![s::catalog()]);
    check("automation_tests", s::tests());
    check("automation_test_results", s::test_results());
    check("automation_run_now", s::run_now());
}

#[test]
fn every_variant_is_sampled() {
    let a = s::automations();
    assert_eq!(values(a.iter().map(|x| &x.spec.trigger), Some("type")).len(), 7, "triggers");
    let filters: Vec<&Filter> = a.iter().flat_map(|x| &x.spec.filters).collect();
    assert_eq!(values(filters.iter().map(|f| f.op), None).len(), 8, "filter ops");
    assert_eq!(values(a.iter().map(|x| x.spec.concurrency), None).len(), 4, "concurrency");
    let targets: Vec<Target> = a.iter().filter_map(|x| x.spec.defaults.target.clone()).chain(
        s::every_step().iter().filter_map(|st| st.config.get("target").map(|t| serde_json::from_value::<Target>(t.clone()).unwrap())),
    ).collect();
    assert_eq!(values(targets, Some("type")).len(), 4, "targets");
    let runs = s::runs();
    assert_eq!(values(runs.iter().map(|r| r.run.status), None).len(), 7, "run states");
    assert_eq!(values(runs[0].run.steps.iter().map(|x| x.status), None).len(), 8, "step states");
    assert_eq!(values(s::notices().iter().map(|n| n.level), None).len(), 4, "notice levels");
    let kinds: BTreeSet<String> = s::every_step().iter().map(|x| x.kind.clone()).collect();
    for k in step_kinds::ALL {
        assert!(kinds.contains(k), "no sample of {k}");
    }
}

#[test]
fn every_built_in_config_parses_as_its_type() {
    for st in s::every_step() {
        let c = st.config.clone();
        let ok = match st.kind.as_str() {
            step_kinds::AGENT_START => serde_json::from_value::<AgentStart>(c).is_ok(),
            step_kinds::AGENT_WAIT => serde_json::from_value::<AgentWait>(c).is_ok(),
            step_kinds::AGENT_MESSAGE => serde_json::from_value::<AgentMessage>(c).is_ok(),
            step_kinds::AGENT_RESPOND => serde_json::from_value::<AgentRespond>(c).is_ok(),
            step_kinds::AGENT_PUBLISH => serde_json::from_value::<AgentPublish>(c).is_ok(),
            step_kinds::AGENT_SCRIPT => serde_json::from_value::<AgentScript>(c).is_ok(),
            step_kinds::AGENT_SETTLE => serde_json::from_value::<AgentSettle>(c).is_ok(),
            step_kinds::WEBHOOK => serde_json::from_value::<WebhookCall>(c).is_ok(),
            step_kinds::NOTIFY => serde_json::from_value::<NotifyConfig>(c).is_ok(),
            step_kinds::DELAY => serde_json::from_value::<DelayConfig>(c).is_ok(),
            _ => true,
        };
        assert!(ok, "{}: {}", st.kind, st.config);
    }
}

/// Defaults fill in what a spec leaves out, and a config field that is `null` is unset.
#[test]
fn null_config_fields_are_unset() {
    let a: AutomationSpec = serde_json::from_value(serde_json::json!({
        "name": "x", "trigger": { "type": "schedule", "cron": "* * * * *" }, "steps": []
    }))
    .unwrap();
    assert!(a.enabled);
    assert_eq!(a.trigger, Trigger::Schedule { cron: "* * * * *".into(), timezone: "UTC".into() });
    assert_eq!(a.concurrency, Concurrency::Parallel);
    let t: Target = serde_json::from_value(serde_json::json!({ "type": "sandbox" })).unwrap();
    assert_eq!(t, Target::Sandbox { size: "standard".into(), keep: false });

    // A config field that is `null` is unset: the builder sends nulls for fields nobody filled
    // in, and kinds with a `#[serde(default)]` map, list or flag refused them. Nulls deeper in
    // are data.
    let a: AutomationSpec = serde_json::from_value(serde_json::json!({
        "name": "x",
        "trigger": { "type": "integration", "source": "slack", "config": { "team": null, "channel": "C1" } },
        "steps": [{ "id": "w", "kind": "webhook", "config": { "url": "https://x", "headers": null, "method": null, "body": { "a": null } } }]
    }))
    .unwrap();
    let Trigger::Integration { config, .. } = &a.trigger else { panic!("{:?}", a.trigger) };
    assert_eq!(config, &serde_json::json!({ "channel": "C1" }));
    assert_eq!(a.steps[0].config, serde_json::json!({ "url": "https://x", "body": { "a": null } }));
    let w: WebhookCall = serde_json::from_value(a.steps[0].config.clone()).unwrap();
    assert_eq!((w.method.as_str(), w.headers.len()), ("POST", 0));
}
