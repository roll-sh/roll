//! Every path that writes a grant's rules or lifetime, or issues a grant a secret, meets the policy
//! (`Access::meets_policy`: roll's token policy and each product's own cap, roll secrets'
//! `max_token_days`), as `rolld/tests/auth.rs` holds every route to a catalogue row. A function
//! calling one of the store's writers asks `meets_policy` itself or is listed here with why not.

/// The store's calls that write `grants.rules`, `grants.expires_at` or temporary rules, or issue a
/// grant a secret.
const WRITERS: &[&str] = &[
    "store.create(",
    "store.create_workload(",
    "store.update(",
    "store.rotate(",
    "store.remember(",
    "store.swap_rules(",
    "store.apply_step_up(",
    "store.issue_code(",
    "store.issue_code_for(",
    "store.exchange_code(",
    "store.refresh(",
    "store.approve_device(",
    "store.poll_device(",
    "store.create_machine_client(",
    "store.rotate_machine_client(",
    "store.issue_client_access(",
    "store.create_bootstrap(",
    "store.redeem(",
    "store.decide(",
    "set_rules(c",
];

/// Callers that don't ask `meets_policy` themselves: (file, function, why).
const EXPLAINED: &[(&str, &str, &str)] = &[
    ("roll-access/src/api.rs", "approve", "`remembered` asks, with the rule, before anything is decided"),
    ("roll-access/src/api.rs", "apply_tighten", "a proposal only narrows the token's rules"),
    ("roll-access/src/lib.rs", "mint_decoy", "a decoy allows nothing"),
    ("roll-access/src/lib.rs", "mint_workload_via", "a workload login lasts minutes, within its identity's rule"),
    ("roll-access/src/lib.rs", "decide", "an access request's rule lasts a day at most, no longer than any cap"),
    ("roll-access/src/oauth.rs", "authorize", "`consented` asks"),
    ("roll-access/src/oauth.rs", "device_approve", "`consented` asks"),
    ("roll-access/src/oauth.rs", "token", "codes, refresh tokens and devices are for connections their consent checked"),
    ("roll-access/src/client_credentials.rs", "token", "an hour's access on a machine client its create and rotate checked"),
    ("roll-access/src/client_credentials.rs", "federated", "an hour's access on a machine client its create and rotate checked"),
    ("roll-access/src/bootstrap.rs", "mint_bootstrap", "a machine credential's chain: roll secrets checks its hours (guardrails' machine_hours)"),
    ("roll-access/src/bootstrap.rs", "redeem", "a machine credential's chain: capped as it's issued (guardrails' cap_hours)"),
    ("roll-access/src/enterprise.rs", "update_app", "each connection's next sign-in meets the policy (`connect` replaces one that doesn't)"),
    ("rolld/src/secrets.rs", "a_cut_off_revokes_tokens_through_roll_access_500_at_a_time", "a test's tokens"),
];

fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The name of the function a line is in: the last `fn` above it.
fn fn_name(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let t = t.strip_prefix("pub(crate) ").or_else(|| t.strip_prefix("pub ")).unwrap_or(t);
    let t = t.strip_prefix("async ").unwrap_or(t);
    let rest = t.strip_prefix("fn ")?;
    Some(&rest[..rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?])
}

#[test]
fn every_grant_writer_meets_the_policy() {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = vec![];
    for dir in ["roll-access/src", "rolld/src"] {
        sources(&crates.join(dir), &mut files);
    }
    let mut unasked = vec![];
    let mut seen = std::collections::BTreeSet::new();
    for path in files {
        let rel = path.strip_prefix(crates).unwrap().to_string_lossy().replace('\\', "/");
        // The store's own writers, and the store calling them inside itself.
        if rel == "roll-access/src/store.rs" || rel.starts_with("roll-access/src/store/") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // Each function's span: from its `fn` line to the next one.
        let starts: Vec<(usize, &str)> = lines.iter().enumerate().filter_map(|(i, l)| fn_name(l).map(|n| (i, n))).collect();
        for (i, line) in lines.iter().enumerate() {
            if !WRITERS.iter().any(|w| line.contains(w)) || line.trim_start().starts_with("//") {
                continue;
            }
            let Some(k) = starts.iter().rposition(|(at, _)| *at <= i) else { continue };
            let (from, name) = starts[k];
            let to = starts.get(k + 1).map_or(lines.len(), |(at, _)| *at);
            seen.insert((rel.clone(), name.to_string()));
            if lines[from..to].iter().any(|l| l.contains("meets_policy(")) || EXPLAINED.iter().any(|(f, n, _)| *f == rel && *n == name) {
                continue;
            }
            unasked.push(format!("{rel}:{} in fn {name}: {}", i + 1, line.trim()));
        }
    }
    assert!(unasked.is_empty(), "these write a grant's rules or lifetime, or issue it a secret, without Access::meets_policy (call it, or say why in EXPLAINED):\n{}", unasked.join("\n"));
    let stale: Vec<_> = EXPLAINED.iter().filter(|(f, n, _)| !seen.contains(&(f.to_string(), n.to_string()))).collect();
    assert!(stale.is_empty(), "EXPLAINED rows no writer call is in any more: {stale:?}");
}
