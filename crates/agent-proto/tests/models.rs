//! Model option descriptors: what an older daemon sends still reads, and what a client asks for
//! is held to the model's options and the limits.

use agent_proto::*;
use serde_json::json;

#[test]
fn a_daemon_from_before_descriptors_still_lists_its_efforts() {
    let old: ModelInfo = serde_json::from_value(json!({ "id": "gpt-5.5", "name": "GPT-5.5", "default": true, "efforts": ["minimal", "xhigh"] })).unwrap();
    assert_eq!(old.efforts(), ["minimal", "xhigh"]);
    let Some(ModelOption::Select { choices, role, .. }) = old.reasoning() else { panic!("no reasoning option") };
    assert_eq!(*role, Some(OptionRole::Reasoning));
    assert_eq!(choices.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Minimal", "Extra High"]);
    assert!(old.images, "images default to on");
    // Written back, it is in the new shape only.
    let v = serde_json::to_value(&old).unwrap();
    assert!(v.get("efforts").is_none() && v.get("images").is_none());
    assert_eq!(v["options"][0]["id"], "effort");
    let again: ModelInfo = serde_json::from_value(v).unwrap();
    assert_eq!(again, old);
}

#[test]
fn a_model_switch_keeps_only_the_options_the_new_model_takes() {
    let opus = ModelInfo {
        options: vec![
            ModelOption::reasoning(vec![OptionChoice::new("high", "High")], Some("high")),
            ModelOption::Boolean { id: "fastMode".into(), label: "Fast Mode".into(), description: None, default: false, role: Some(OptionRole::Speed) },
            ModelOption::Select {
                id: "contextWindow".into(),
                label: "Context Window".into(),
                description: None,
                choices: vec![OptionChoice::new("200k", "200k"), OptionChoice::new("1m", "1M")],
                default: Some("200k".into()),
                role: Some(OptionRole::ContextWindow),
            },
        ],
        ..ModelInfo::new("claude-opus-4-6", "Claude Opus 4.6")
    };
    let wanted = ModelOptions::from([
        ("fastMode".into(), OptionValue::Flag(true)),
        ("contextWindow".into(), OptionValue::Choice("2m".into())),
        ("serviceTier".into(), OptionValue::Choice("fast".into())),
        ("effort".into(), OptionValue::Choice("high".into())),
    ]);
    assert_eq!(opus.accepted(&wanted), ModelOptions::from([("fastMode".into(), OptionValue::Flag(true))]));
    // A choice must be offered; a flag can't stand for a choice.
    let ok = ModelOptions::from([("contextWindow".into(), OptionValue::Choice("1m".into()))]);
    assert_eq!(opus.accepted(&ok), ok);
    assert!(opus.accepted(&ModelOptions::from([("contextWindow".into(), OptionValue::Flag(true))])).is_empty());
}

#[test]
fn options_from_clients_are_bounded() {
    assert!(check_options(&ModelOptions::new()).is_ok());
    let many: ModelOptions = (0..=option_limits::OPTIONS).map(|i| (format!("o{i}"), OptionValue::Flag(true))).collect();
    assert!(check_options(&many).unwrap_err().contains("at most"));
    assert!(check_options(&ModelOptions::from([("effort".into(), OptionValue::Choice("high".into()))])).unwrap_err().contains("effort"));
    assert!(check_options(&ModelOptions::from([("x".repeat(option_limits::ID + 1), OptionValue::Flag(true))])).is_err());
    assert!(check_options(&ModelOptions::from([("tier".into(), OptionValue::Choice("".into()))])).is_err());
    assert!(check_options(&ModelOptions::from([("tier".into(), OptionValue::Choice("v".repeat(option_limits::VALUE + 1)))])).is_err());
    // The machine's new-thread defaults go through the same check.
    let mut s = MachineSettings::default();
    s.defaults.options = many;
    assert!(s.check().is_err());
}
