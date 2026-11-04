//! Machine settings: what `MachineSettings::check` keeps and refuses (docs/code/agents.md,
//! "Machine settings").

use agent_proto::*;

fn with_env(name: &str, value: &str) -> MachineSettings {
    let mut s = MachineSettings::default();
    s.providers.insert(ProviderId::claude(), ProviderSettings { env: [(name.to_string(), value.to_string())].into(), ..ProviderSettings::default() });
    s
}

#[test]
fn credentials_and_addresses_never_go_through_roll() {
    for name in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_BASE_URL",
        "OPENAI_BASE_URL",
        "GH_TOKEN",
        "ANTHROPIC_AUTH_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "HTTPS_PROXY",
        "https_proxy",
        "OLLAMA_HOST",
        "AZURE_OPENAI_ENDPOINT",
        "DB_PASSWORD",
        "GOOGLE_APPLICATION_CREDENTIALS",
    ] {
        assert!(is_credential_env(name), "{name}");
        let err = with_env(name, "x").check().unwrap_err();
        assert!(err.contains("config.toml"), "{name}: {err}");
    }
    for name in ["CODEX_HOME", "CLAUDE_CONFIG_DIR", "RUST_LOG", "AWS_REGION"] {
        assert!(!is_credential_env(name), "{name}");
        assert!(with_env(name, "/somewhere").check().is_ok(), "{name}");
    }
    // Conservative on purpose: anything that names a token is refused, even a count of them.
    assert!(is_credential_env("CLAUDE_CODE_MAX_OUTPUT_TOKENS"));
}

#[test]
fn env_names_must_be_names() {
    for bad in ["", "1X", "A-B", "A B", "Ä"] {
        assert!(with_env(bad, "x").check().is_err(), "{bad:?}");
    }
    assert!(with_env("CODEX_HOME", &"x".repeat(settings_limits::ENV_VALUE + 1)).check().is_err());
}

#[test]
fn trims_unsets_empties_and_drops_default_providers() {
    let mut s = MachineSettings {
        worktree_dir: Some("  ".into()),
        branch_prefix: Some(" ada/wip ".into()),
        git_name: Some(" Ada ".into()),
        git_email: Some(concat!(" ada", "@", "example.com ").into()),
        ..MachineSettings::default()
    };
    s.providers.insert(ProviderId::codex(), ProviderSettings::default());
    s.providers.insert(
        ProviderId::claude(),
        ProviderSettings { binary: Some(" ".into()), models: vec![CustomModel { id: " big ".into(), name: Some(" ".into()) }], ..ProviderSettings::default() },
    );
    let s = s.check().unwrap();
    assert_eq!(s.worktree_dir, None);
    assert_eq!(s.branch_prefix.as_deref(), Some("ada/wip"));
    assert_eq!(s.git_name.as_deref(), Some("Ada"));
    assert_eq!(s.git_email.as_deref(), Some(concat!("ada", "@", "example.com")));
    assert!(!s.providers.contains_key(&ProviderId::codex()), "a provider with nothing set isn't kept");
    let claude = s.provider(&ProviderId::claude());
    assert_eq!(claude.binary, None);
    assert_eq!(claude.models, vec![CustomModel { id: "big".into(), name: None }]);
    assert!(s.provider(&ProviderId::codex()).enabled, "unset is on");
}

#[test]
fn refuses_what_it_cant_keep() {
    let bad = |s: MachineSettings| s.check().unwrap_err();
    let model = |ids: &[&str]| {
        let mut s = MachineSettings::default();
        s.providers.insert(
            ProviderId::codex(),
            ProviderSettings { models: ids.iter().map(|id| CustomModel { id: id.to_string(), name: None }).collect(), ..ProviderSettings::default() },
        );
        s
    };
    assert!(bad(model(&["a", " a"])).contains("twice"));
    assert!(bad(model(&[" "])).contains("slug"));
    assert!(bad(model(&[&"m".repeat(settings_limits::MODEL_ID + 1)])).contains("at most"));
    for prefix in ["a..b", "/a", "a/", ".a", "a b", "a.lock", "a//b"] {
        assert!(MachineSettings { branch_prefix: Some(prefix.into()), ..MachineSettings::default() }.check().is_err(), "{prefix}");
    }
    assert!(MachineSettings { worktree_dir: Some("relative/dir".into()), ..MachineSettings::default() }.check().is_err());
    assert!(MachineSettings { worktree_dir: Some("~/wt".into()), ..MachineSettings::default() }.check().is_ok());
    assert!(MachineSettings { pull_every: Some(settings_limits::PULL_EVERY + 1), ..MachineSettings::default() }.check().is_err());
    assert!(MachineSettings { max_depth: Some(settings_limits::MAX_DEPTH + 1), ..MachineSettings::default() }.check().is_err());
    assert!(MachineSettings { max_children: Some(0), ..MachineSettings::default() }.check().is_err());
    assert!(MachineSettings { git_email: Some("ada".into()), ..MachineSettings::default() }.check().is_err());
    assert!(MachineSettings { git_name: Some("Ada <x>".into()), ..MachineSettings::default() }.check().is_err());
}

#[test]
fn a_provider_turned_off_serializes_and_on_is_the_default() {
    let mut s = MachineSettings::default();
    s.providers.insert(ProviderId::codex(), ProviderSettings { enabled: false, ..ProviderSettings::default() });
    let json = serde_json::to_value(&s).unwrap();
    assert_eq!(json, serde_json::json!({ "providers": { "codex": { "enabled": false } } }));
    let back: MachineSettings = serde_json::from_value(serde_json::json!({ "providers": { "codex": {} } })).unwrap();
    assert!(back.provider(&ProviderId::codex()).enabled);
}
