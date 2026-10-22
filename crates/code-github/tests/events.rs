//! Every webhook action roll acts on (pull requests, issues, installations, comments), deleted
//! refs, malformed payloads, signature header forms, and every App configuration error.

use code_github::mapping::{issue_draft, pull_request_draft, push_draft};
use code_github::webhook::{CommentAction, InstallationAction, IssueAction, PullRequestAction, RepositoriesAction};
use code_github::webhook::sign;
use code_github::{AppConfig, Event, verify_signature};
use serde_json::Value;

fn fixture(name: &str) -> Value {
    serde_json::from_slice(&std::fs::read(format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap()
}

fn with_action(name: &str, action: &str) -> Vec<u8> {
    let mut v = fixture(name);
    v["action"] = Value::String(action.into());
    serde_json::to_vec(&v).unwrap()
}

#[test]
fn every_pull_request_action() {
    for (action, want) in [
        ("opened", PullRequestAction::Opened),
        ("synchronize", PullRequestAction::Synchronize),
        ("closed", PullRequestAction::Closed),
        ("reopened", PullRequestAction::Reopened),
        ("edited", PullRequestAction::Edited),
    ] {
        let Event::PullRequest(pr) = Event::parse("pull_request", &with_action("pull_request_opened", action)).unwrap() else {
            panic!("{action}")
        };
        assert_eq!(pr.action, want);
        let d = pull_request_draft(&pr.pull_request);
        assert_eq!(d.github_pr, pr.number);
        assert!(!d.head_sha.is_empty() && !d.head_ref.is_empty());
    }
    for ignored in ["labeled", "assigned", "review_requested", "converted_to_draft"] {
        assert!(matches!(Event::parse("pull_request", &with_action("pull_request_opened", ignored)).unwrap(), Event::Ignored { .. }), "{ignored}");
    }
}

#[test]
fn every_issue_action() {
    for (action, want) in [
        ("opened", IssueAction::Opened),
        ("edited", IssueAction::Edited),
        ("closed", IssueAction::Closed),
        ("reopened", IssueAction::Reopened),
        ("labeled", IssueAction::Labeled),
        ("unlabeled", IssueAction::Unlabeled),
    ] {
        let Event::Issues(i) = Event::parse("issues", &with_action("issues_labeled", action)).unwrap() else { panic!("{action}") };
        assert_eq!(i.action, want);
        let d = issue_draft(&i.issue);
        assert_eq!(d.external_id, format!("github#{}", d.number));
    }
    assert!(matches!(Event::parse("issues", &with_action("issues_labeled", "pinned")).unwrap(), Event::Ignored { .. }));
    for (action, want) in [("created", CommentAction::Created), ("edited", CommentAction::Edited), ("deleted", CommentAction::Deleted)] {
        let Event::IssueComment(c) = Event::parse("issue_comment", &with_action("issue_comment_created", action)).unwrap() else { panic!() };
        assert_eq!(c.action, want);
    }
}

#[test]
fn installation_actions() {
    for (action, want) in [
        ("created", InstallationAction::Created),
        ("deleted", InstallationAction::Deleted),
        ("suspend", InstallationAction::Suspend),
        ("unsuspend", InstallationAction::Unsuspend),
        ("new_permissions_accepted", InstallationAction::NewPermissionsAccepted),
    ] {
        let Event::Installation { action: got, .. } = Event::parse("installation", &with_action("installation_created", action)).unwrap() else { panic!("{action}") };
        assert_eq!(got, want);
    }
    let mut removed = fixture("installation_repositories_added");
    removed["action"] = "removed".into();
    removed["repositories_removed"] = removed["repositories_added"].clone();
    removed["repositories_added"] = Value::Array(vec![]);
    let Event::InstallationRepositories { action, added, removed, .. } = Event::parse("installation_repositories", &serde_json::to_vec(&removed).unwrap()).unwrap() else {
        panic!()
    };
    assert_eq!(action, RepositoriesAction::Removed);
    assert!(added.is_empty() && !removed.is_empty());
}

#[test]
fn deleted_refs_and_malformed_payloads() {
    let mut push = fixture("push_branch_created");
    push["before"] = push["after"].clone();
    push["after"] = "0000000000000000000000000000000000000000".into();
    push["created"] = false.into();
    push["deleted"] = true.into();
    let Event::Push(p) = Event::parse("push", &serde_json::to_vec(&push).unwrap()).unwrap() else { panic!() };
    let d = push_draft(&p);
    assert!(d.after.is_none(), "a deleted ref has no new head");
    assert!(d.before.is_some());

    // Tags aren't branches.
    let mut tag = fixture("push");
    tag["ref"] = "refs/tags/v1.0".into();
    let Event::Push(p) = Event::parse("push", &serde_json::to_vec(&tag).unwrap()).unwrap() else { panic!() };
    let d = push_draft(&p);
    assert_eq!(d.branch(), None);
    assert!(!d.default_branch);

    assert!(Event::parse("push", b"{not json").is_err());
    assert!(Event::parse("pull_request", br#"{"action":"opened"}"#).is_err(), "a known action with a missing body is an error");
    assert!(matches!(Event::parse("deployment", br#"{"action":"created"}"#).unwrap(), Event::Ignored { event, .. } if event == "deployment"));
    assert!(matches!(Event::parse("", b"{}").unwrap(), Event::Ignored { .. }));
}

#[test]
fn signature_header_forms() {
    let body = br#"{"zen":"x"}"#;
    let sig = sign(b"s", body);
    assert!(verify_signature(b"s", body, Some(&sig)));
    assert!(verify_signature(b"s", body, Some(&format!("  {sig} "))), "surrounding whitespace is fine");
    assert!(!verify_signature(b"s", body, None));
    assert!(!verify_signature(b"s", body, Some(&sig.replace("sha256=", "sha1="))));
    assert!(!verify_signature(b"s", body, Some("sha256=zz")));
    assert!(!verify_signature(b"s", body, Some("sha256=")));
    assert!(!verify_signature(b"s", body, Some(&sig[..sig.len() - 2])));
}

#[test]
fn app_config_errors() {
    let full = |k: &str| -> Option<String> {
        match k {
            "ROLL_GITHUB_APP_ID" => Some(" 7 ".into()),
            "ROLL_GITHUB_PRIVATE_KEY" => Some("pem".into()),
            "ROLL_GITHUB_WEBHOOK_SECRET" => Some("s".into()),
            "ROLL_GITHUB_APP_SLUG" => Some("roll".into()),
            "ROLL_GITHUB_WEB_URL" => Some("https://ghe.example/".into()),
            _ => None,
        }
    };
    let c = AppConfig::from_lookup(full).unwrap().unwrap();
    assert_eq!(c.app_id, 7);
    assert_eq!(c.web_base, "https://ghe.example");
    assert_eq!(c.api_base, "https://api.github.com");
    assert_eq!(c.install_url(None), "https://ghe.example/apps/roll/installations/new");
    for missing in ["ROLL_GITHUB_PRIVATE_KEY", "ROLL_GITHUB_WEBHOOK_SECRET", "ROLL_GITHUB_APP_SLUG"] {
        let err = AppConfig::from_lookup(|k| if k == missing { None } else { full(k) }).unwrap_err().to_string();
        assert!(err.contains(missing.trim_end_matches("_PATH")), "{missing}: {err}");
    }
    // The key can come from a file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.pem");
    std::fs::write(&path, "from file").unwrap();
    let p = path.to_string_lossy().to_string();
    let c = AppConfig::from_lookup(|k| match k {
        "ROLL_GITHUB_PRIVATE_KEY" => None,
        "ROLL_GITHUB_PRIVATE_KEY_PATH" => Some(p.clone()),
        k => full(k),
    })
    .unwrap()
    .unwrap();
    assert_eq!(c.private_key_pem, "from file");
    let err = AppConfig::from_lookup(|k| match k {
        "ROLL_GITHUB_PRIVATE_KEY" => None,
        "ROLL_GITHUB_PRIVATE_KEY_PATH" => Some("/nonexistent/key.pem".into()),
        k => full(k),
    })
    .unwrap_err();
    assert!(err.to_string().contains("/nonexistent/key.pem"), "{err}");
}

#[test]
fn review_events() {
    use code_github::webhook::{ReviewAction, ThreadAction};
    for (action, want) in [("submitted", ReviewAction::Submitted), ("edited", ReviewAction::Edited), ("dismissed", ReviewAction::Dismissed)] {
        let Event::PullRequestReview(e) = Event::parse("pull_request_review", &with_action("pull_request_review_submitted", action)).unwrap() else {
            panic!("{action}")
        };
        assert_eq!(e.action, want);
        assert_eq!(e.review.state, "changes_requested");
        assert_eq!(e.review.user.as_ref().unwrap().login, "coderabbitai[bot]");
        assert_eq!(e.pull_request.number, 42);
        assert_eq!(e.repository.full_name, "acme/api");
    }
    for (action, want) in [("created", CommentAction::Created), ("edited", CommentAction::Edited), ("deleted", CommentAction::Deleted)] {
        let raw = with_action("pull_request_review_comment_created", action);
        let ev = Event::parse("pull_request_review_comment", &raw).unwrap();
        assert!(ev.installation_id().is_some() && ev.repository().is_some() && ev.sender().is_some());
        let Event::PullRequestReviewComment(e) = ev else { panic!("{action}") };
        assert_eq!(e.action, want);
        assert_eq!(e.comment.path, "src/limit.rs");
        assert_eq!(e.comment.line, Some(2));
        assert_eq!(e.comment.side.as_deref(), Some("RIGHT"));
        assert_eq!(e.comment.in_reply_to_id, None);
        assert!(e.comment.body.contains("```suggestion"));
    }
    for (action, want) in [("resolved", ThreadAction::Resolved), ("unresolved", ThreadAction::Unresolved)] {
        let Event::PullRequestReviewThread(e) = Event::parse("pull_request_review_thread", &with_action("pull_request_review_thread_resolved", action)).unwrap() else {
            panic!("{action}")
        };
        assert_eq!(e.action, want);
        assert_eq!(e.thread.comments[0].id, 1800000001);
    }
    assert!(matches!(Event::parse("pull_request_review", &with_action("pull_request_review_submitted", "requested")).unwrap(), Event::Ignored { .. }));
    assert!(matches!(Event::parse("pull_request_review_thread", &with_action("pull_request_review_thread_resolved", "x")).unwrap(), Event::Ignored { .. }));
    assert!(Event::parse("pull_request_review", b"{\"action\":\"submitted\"}").is_err(), "a review event without its review is an error");
}
