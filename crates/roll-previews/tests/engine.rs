//! The reconciler against fake products: what a person relies on from a preview's lifecycle.

mod common;

use common::*;
use roll_previews::ports::ChangeInfo;
use roll_previews::{Desired, Kind, ResourceState, State};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn a_change_gets_a_full_preview_that_follows_pushes_and_goes_when_it_lands() {
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_1", 7));
    e.engine.pass().await.unwrap();

    let p = e.code.last_report("chg_1");
    assert_eq!(p.state, State::Ready, "{p:?}");
    assert_eq!(p.slug, "pr107-shop");
    let refs: Vec<_> = p.resources.iter().map(|r| (r.kind, r.r#ref.clone().unwrap(), r.state)).collect();
    assert!(refs.contains(&(Kind::Postgres, format!("prj_db~pv-{}", &p.id[3..]), ResourceState::Ready)), "{refs:?}");
    assert_eq!(refs.len(), 2, "a branch of every data resource");
    // Every service of the project: the change's repo at its head, the other at its default head.
    let urls: Vec<_> = p.services.iter().map(|s| (s.name.as_str(), s.url.as_str(), s.revision.as_deref().unwrap())).collect();
    assert!(urls.contains(&("web", "http://web-pr107-shop.preview.test", "cs_7a")), "{urls:?}");
    assert!(urls.contains(&("api", "http://api-pr107-shop.preview.test", "cs_api0")), "{urls:?}");
    assert!(p.cost.sleeping_usd_month > 0.0 && p.cost.active_usd_hour > 0.0, "{:?}", p.cost);

    // Converging again changes nothing: branching is idempotent.
    e.engine.pass().await.unwrap();
    assert_eq!(e.pg.made.load(Ordering::SeqCst), 1);

    // A push redeploys the change's service at the new head; the data stays.
    e.code.edit("chg_1", |c| c.head = "cs_7b".into());
    e.engine.pass().await.unwrap();
    assert_eq!(e.apps.deployed.lock()[&(p.id.clone(), "web".to_string())], "cs_7b");
    assert_eq!(e.pg.made.load(Ordering::SeqCst), 1, "a push keeps the preview's data");

    // A reset re-branches from the parent, keeping the preview.
    e.engine.reset(&p.id, vec!["postgres:prj_db".into()]).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.pg.made.load(Ordering::SeqCst), 2);
    assert!(e.engine.store.preview(&p.id).unwrap().last_reset_at.is_some());

    // The change lands: services, branches and the preview go; the parents stay.
    e.code.edit("chg_1", |c| c.open = false);
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert!(e.engine.store.row(&p.id).is_err(), "the preview is gone");
    assert!(e.pg.branches.lock().is_empty() && e.redis.branches.lock().is_empty());
    assert!(e.apps.deployed.lock().is_empty());
    assert_eq!(*e.code.forgotten.lock(), vec![p.id.clone()]);
}

#[tokio::test]
async fn a_fork_waits_for_approval_and_a_failed_step_is_retried() {
    let e = env(json!({ "enabled": true, "resources": { "redis:rdb_cache": "skip" } }));
    e.code.open(ChangeInfo { trusted: false, ..change("chg_f", 3) });
    e.engine.pass().await.unwrap();
    let p = e.code.last_report("chg_f");
    assert_eq!(p.state, State::AwaitingApproval);
    assert_eq!(e.pg.made.load(Ordering::SeqCst), 0, "nothing of a fork runs or branches before approval");
    assert!(e.apps.deployed.lock().is_empty());

    // A writer approves the head; the product fails once, the retry converges.
    e.code.edit("chg_f", |c| c.approved = true);
    e.pg.fail_next.store(1, Ordering::SeqCst);
    e.engine.pass().await.unwrap();
    let failed = e.engine.store.row(&p.id).unwrap();
    assert_eq!(failed.state, State::Failed);
    assert!(failed.error.as_deref().unwrap().contains("busy"));
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap(); // the backoff has passed
    e.engine.pass().await.unwrap();
    let p = e.engine.store.preview(&p.id).unwrap();
    assert_eq!(p.state, State::Ready, "{p:?}");
    assert_eq!(p.resources.len(), 1, "redis is skipped by the project's rules");

    // A new push un-approves it: its services stop and it waits again.
    e.code.edit("chg_f", |c| {
        c.head = "cs_3b".into();
        c.approved = false;
    });
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.row(&p.id).unwrap().state, State::AwaitingApproval);
}

#[tokio::test]
async fn previews_beyond_the_limit_wait_and_start_as_others_go() {
    let e = env(json!({ "enabled": true, "max_previews": 1 }));
    e.code.open(change("chg_a", 1));
    e.engine.pass().await.unwrap();
    e.code.open(change("chg_b", 2));
    e.engine.pass().await.unwrap();
    let b = e.engine.store.row_by_change("chg_b").unwrap().unwrap();
    assert_eq!(b.state, State::Queued);
    let a = e.engine.store.row_by_change("chg_a").unwrap().unwrap();
    e.engine.delete(&a.id).unwrap();
    for _ in 0..3 {
        e.engine.pass().await.unwrap();
    }
    let b = e.engine.store.row(&b.id).unwrap();
    assert_eq!((b.state, b.desired), (State::Ready, Desired::Active));
    // A deleted change preview stays deleted until the change's next push.
    e.engine.delete(&b.id).unwrap();
    for _ in 0..3 {
        e.engine.pass().await.unwrap();
    }
    assert!(e.engine.store.row_by_change("chg_b").unwrap().is_none(), "not recreated at the same head");
    e.code.edit("chg_b", |c| c.head = "cs_2b".into());
    e.engine.pass().await.unwrap();
    assert!(e.engine.store.row_by_change("chg_b").unwrap().is_some(), "a push brings it back");
    // A manual preview of a branch is refused at the limit rather than queued.
    e.code.heads.lock().insert("acme/web@feat/x".into(), "cs_x".into());
    let err = e.engine.create(&e.project, "acme/web", Some("feat/x"), None, "me").await.unwrap_err();
    assert!(err.to_string().contains("max_previews"), "{err}");
    // Unused past its TTL it goes, unless it's kept; released, its rules decide again.
    let b = e.engine.store.row_by_change("chg_b").unwrap().unwrap();
    // A day before it goes, the change and its pull request are told when; used again, not.
    let (now, h) = (e.engine.store.now_ms().unwrap(), 3_600_000);
    e.engine.store.update(&b.id, |r| r.last_active_at = now - 156 * h).unwrap();
    e.engine.pass().await.unwrap();
    let goes = e.code.last_report("chg_b").goes_at;
    assert!(goes.is_some_and(|t| (t - (now + 12 * h)).abs() < 60_000), "{goes:?}");
    e.engine.store.update(&b.id, |r| r.last_active_at = now).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.code.last_report("chg_b").goes_at, None, "used again");
    e.engine.store.update(&b.id, |r| r.last_active_at = 0).unwrap();
    e.engine.keep(&b.id, 24).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.row(&b.id).unwrap().desired, Desired::Active, "kept past its TTL");
    assert!(e.engine.keep(&b.id, 721).is_err(), "30 days at most");
    e.engine.keep(&b.id, 0).unwrap();
    e.engine.pass().await.unwrap();
    assert!(e.engine.store.row(&b.id).map_or(true, |r| r.desired == Desired::Deleted), "unused past its TTL, it goes");
}

#[tokio::test]
async fn the_seed_runs_once_on_fresh_data_before_services_deploy() {
    let e = env(json!({ "enabled": true, "seed": { "image": "postgres:18-alpine", "run": "psql \"$DATABASE_URL\" -f seed/anonymise.sql" } }));
    e.apps.seed_exit.store(3, Ordering::SeqCst);
    e.code.open(change("chg_s", 9));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_s").unwrap().unwrap();
    assert_eq!(p.state, State::Failed, "a failing seed fails the preview");
    assert!(p.error.as_deref().unwrap().contains("seed output"), "{:?}", p.error);
    assert!(e.apps.deployed.lock().is_empty(), "nothing serves unseeded data");
    // It works on the retry: the branch's URL reached it; a push doesn't seed again; a reset does.
    e.apps.seed_exit.store(0, Ordering::SeqCst);
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.row(&p.id).unwrap().state, State::Ready);
    assert!(e.apps.seeds.lock().last().unwrap().starts_with("fake://prj_db~pv-"), "{:?}", e.apps.seeds.lock());
    e.code.edit("chg_s", |c| c.head = "cs_9b".into());
    e.engine.pass().await.unwrap();
    assert_eq!(e.apps.seeds.lock().len(), 2);
    e.engine.reset(&p.id, Vec::new()).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.apps.seeds.lock().len(), 3, "reset data is seeded again");
}

#[tokio::test]
async fn the_release_runs_at_each_new_head_before_services_and_its_schema_changes_show() {
    let e = env(json!({ "enabled": true, "release": { "image": "node:22", "run": "npm run migrate" } }));
    *e.pg.schema.lock() = Some((vec!["table public.users".into()], vec!["table public.users".into(), "column public.users.nick text".into()]));
    e.apps.seed_exit.store(1, Ordering::SeqCst);
    e.code.open(change("chg_r", 3));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_r").unwrap().unwrap();
    assert_eq!(p.state, State::Failed, "a failing release fails the preview");
    assert!(e.apps.deployed.lock().is_empty(), "nothing deploys over a failed migration");
    e.apps.seed_exit.store(0, Ordering::SeqCst);
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    let v = e.engine.store.view(e.engine.store.row(&p.id).unwrap()).unwrap();
    assert_eq!(v.state, State::Ready);
    assert_eq!(v.schema_changes.as_deref(), Some(&["+ column public.users.nick text".to_string()][..]));
    assert_eq!(e.code.last_report("chg_r").schema_changes, v.schema_changes, "the change (and its pull request) get them");
    // Before the services at that head; not again at the same head; again at a new one.
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    e.code.edit("chg_r", |c| c.head = "cs_3b".into());
    e.engine.pass().await.unwrap();
    let runs = e.apps.runs.lock().clone();
    let heads: Vec<_> = runs.iter().map(|(_, rev, deployed)| (rev.as_str(), *deployed)).collect();
    assert_eq!(heads, [("cs_3a", 0), ("cs_3a", 0), ("cs_3b", 0)], "{runs:?}");
}

#[tokio::test]
async fn a_delete_during_a_converge_and_an_unrecorded_branch_leave_nothing_behind() {
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_d", 4));
    // DELETE lands while the reconciler deploys: the converge mustn't settle over it.
    let engine = Arc::downgrade(&e.engine);
    *e.apps.on_deploy.lock() = Some(Box::new(move || {
        let en = engine.upgrade().unwrap();
        let r = en.store.row_by_change("chg_d").unwrap().unwrap();
        if r.desired == Desired::Active {
            en.delete(&r.id).unwrap();
        }
    }));
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert!(e.engine.store.row_by_change("chg_d").unwrap().is_none(), "deleted, not settled as ready");
    assert!(e.pg.branches.lock().is_empty() && e.apps.deployed.lock().is_empty());

    // A branch the product made whose ref never got recorded (a crash, a timeout) is found by
    // its name and deleted with the preview.
    *e.apps.on_deploy.lock() = None;
    e.code.open(change("chg_o", 5));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_o").unwrap().unwrap();
    e.engine.store.set_resource(&p.id, Kind::Postgres, "prj_db", |r| r.r#ref = None).unwrap();
    e.engine.delete(&p.id).unwrap();
    e.engine.pass().await.unwrap();
    assert!(e.pg.branches.lock().is_empty(), "{:?}", e.pg.branches.lock());
}

#[tokio::test]
async fn previews_wait_once_the_month_s_budget_is_spent() {
    let e = env(json!({ "enabled": true, "max_usd_month": 0.015 }));
    e.code.open(change("chg_1", 1));
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.row_by_change("chg_1").unwrap().unwrap().state, State::Ready);
    // Its branches have cost $0.02 this month (two at $0.01): over the cap.
    e.code.open(change("chg_2", 2));
    e.engine.pass().await.unwrap();
    let second = e.engine.store.row_by_change("chg_2").unwrap().unwrap();
    assert_eq!(second.state, State::Queued, "a new preview waits once the budget is spent");
    assert!(e.engine.store.activity(&second.id).unwrap().iter().any(|a| a.message.contains("max_usd_month")));
    // The project's repos hear about it once, at the highest share passed (100%), not every pass.
    e.engine.pass().await.unwrap();
    let sent = e.code.budgets.lock().clone();
    assert_eq!(sent.iter().map(|(_, a)| a.percent).collect::<Vec<_>>(), [100], "{sent:?}");
    assert!(sent[0].0.contains(&"acme/web".to_string()) && sent[0].1.max_usd_month == 0.015, "{sent:?}");
    // A raised budget is alerted on afresh: $0.02 is 80% of $0.025.
    e.engine.store.put_settings(&e.project, &roll_previews::Settings { enabled: true, max_usd_month: 0.025, ..Default::default() }).unwrap();
    e.engine.pass().await.unwrap();
    let sent = e.code.budgets.lock().clone();
    assert_eq!(sent.iter().map(|(_, a)| a.percent).collect::<Vec<_>>(), [100, 80], "{sent:?}");
}

#[tokio::test]
async fn a_changed_rule_reaches_existing_previews() {
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_m", 6));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_m").unwrap().unwrap();
    assert_eq!(e.pg.made.load(Ordering::SeqCst), 1);
    // The project moves Postgres to structure-only: the preview's full copy goes, a structure-only
    // branch takes its place (as the API does: the rules saved, every preview converging).
    let s = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "resources": { "postgres:prj_db": "schema" } })).unwrap();
    e.engine.store.put_settings(&e.project, &s).unwrap();
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    let pg = e.engine.store.resources(&p.id).unwrap().into_iter().find(|r| r.kind == Kind::Postgres).unwrap();
    assert_eq!(pg.mode, roll_previews::Mode::Schema);
    assert_eq!(e.pg.made.load(Ordering::SeqCst), 2, "branched again in its new mode");
    assert_eq!(e.pg.branches.lock().len(), 1, "the full copy is gone");
}

#[tokio::test]
async fn masked_postgres_is_masked_before_it_serves_and_new_rules_reach_it() {
    let e = env(json!({ "enabled": true, "masking": { "postgres:prj_db": { "public.users.email": "email" } } }));
    e.code.open(change("chg_m", 4));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_m").unwrap().unwrap();
    assert_eq!(p.state, State::Ready);
    let masks = e.pg.masks.lock().clone();
    assert_eq!(masks.len(), 1, "masked once, before Ready: {masks:?}");
    assert!(masks[0].0.starts_with("prj_db~") && masks[0].1.contains_key("public.users.email"));
    // A new rule reaches the open preview: masked again, without waiting for a reset.
    let mut s = e.engine.store.settings(&e.project).unwrap();
    s.masking.insert("postgres:prj_db".into(), [("public.users.email".to_string(), "email".to_string()), ("public.users.name".to_string(), "redact".to_string())].into_iter().collect());
    e.engine.store.put_settings(&e.project, &s).unwrap();
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap();
    let made = e.pg.made.load(Ordering::SeqCst);
    e.engine.pass().await.unwrap();
    assert_eq!(e.pg.masks.lock().len(), 2, "the changed rules were applied");
    // Never rewritten in place (its code may have left triggers a superuser's UPDATE would run):
    // a new branch, masked before it's Ready.
    assert_eq!(e.pg.made.load(Ordering::SeqCst), made + 1, "branched again for the new rules");
    assert_eq!(e.engine.store.row(&p.id).unwrap().state, State::Ready);
    // A reset of a masked branch branches it again too.
    e.engine.reset(&p.id, vec!["postgres:prj_db".into()]).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.pg.made.load(Ordering::SeqCst), made + 2);
    assert_eq!(e.pg.masks.lock().len(), 3);
    assert_eq!(e.engine.store.row(&p.id).unwrap().state, State::Ready);
}

#[tokio::test]
async fn the_secrets_environment_is_trusted_only_for_trusted_changes_where_production_is_allowed() {
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_t", 6));
    e.code.open(ChangeInfo { trusted: false, approved: true, ..change("chg_u", 7) });
    e.engine.pass().await.unwrap();
    let mut s = e.engine.store.settings(&e.project).unwrap();
    s.secrets.allow_production = true;
    e.engine.store.put_settings(&e.project, &s).unwrap();
    for c in ["chg_t", "chg_u"] {
        let id = e.engine.store.row_by_change(c).unwrap().unwrap().id;
        e.engine.store.update(&id, |r| r.next_at = 0).unwrap();
    }
    e.engine.pass().await.unwrap();
    let env_of = |c: &str| format!("pv-{}", &e.engine.store.row_by_change(c).unwrap().unwrap().id[3..]);
    let ensured = e.secrets.ensured.lock().clone();
    let trust = |c: &str| ensured.iter().filter(|(env, _)| *env == env_of(c)).map(|(_, t)| *t).collect::<Vec<_>>();
    assert_eq!(trust("chg_t"), [false, true], "trusted only once the project allows production");
    assert!(trust("chg_u").iter().all(|t| !t), "a fork's never is: {:?}", trust("chg_u"));
}

#[tokio::test]
async fn a_deploy_still_going_is_checked_again_and_a_failed_one_fails_the_preview() {
    let e = env(json!({ "enabled": true }));
    e.apps.still.store(1, Ordering::SeqCst);
    e.code.open(change("chg_d", 8));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_d").unwrap().unwrap();
    assert_ne!(p.state, State::Failed, "a build in progress isn't a failure");
    assert_eq!(p.attempts, 0);
    assert!(p.next_at < i64::MAX && p.next_at > 0, "it comes back soon: {}", p.next_at);
    assert!(e.engine.store.services(&p.id).unwrap().iter().any(|s| s.state == roll_previews::ServiceState::Deploying));
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.row(&p.id).unwrap().state, State::Ready, "live on the next look");
    // A failed build fails the preview (shown, retried), never "deployed".
    e.apps.fail_deploy.store(1, Ordering::SeqCst);
    e.code.edit("chg_d", |c| c.head = "cs_8b".into());
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row(&p.id).unwrap();
    assert_eq!(p.state, State::Failed);
    assert!(p.error.as_deref().unwrap_or("").contains("the build failed"), "{:?}", p.error);
}

#[tokio::test]
async fn another_repo_runs_its_branch_of_the_same_name() {
    let e = env(json!({ "enabled": true }));
    // The feature touches both repos: web's change is on feat/9, and api has a feat/9 too.
    e.code.heads.lock().insert("acme/api@feat/9".into(), "cs_api_feat".into());
    e.code.open(change("chg_x", 9));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_x").unwrap().unwrap();
    let rev = |svc: &str| e.apps.deployed.lock().get(&(p.id.clone(), svc.to_string())).cloned();
    assert_eq!(rev("web").as_deref(), Some("cs_9a"));
    assert_eq!(rev("api").as_deref(), Some("cs_api_feat"), "api's own feat/9, not its default branch");
    // A push to api's feat/9 reaches the preview though the change didn't move.
    e.code.heads.lock().insert("acme/api@feat/9".into(), "cs_api_feat2".into());
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(rev("api").as_deref(), Some("cs_api_feat2"));
}

#[tokio::test]
async fn path_filters_leave_out_changes_and_pushes_that_touch_only_ignored_paths() {
    let e = env(json!({ "enabled": true, "ignore_paths": ["docs/", "*.md"] }));
    e.code.paths.lock().insert("cs_9a".into(), vec!["docs/setup.md".into(), "README.md".into()]);
    e.code.open(change("chg_d", 9));
    e.engine.pass().await.unwrap();
    assert!(e.engine.store.row_by_change("chg_d").unwrap().is_none(), "docs only: no preview");
    // A push that touches code gets one.
    e.code.paths.lock().insert("cs_9b".into(), vec!["README.md".into(), "src/app.ts".into()]);
    e.code.edit("chg_d", |c| c.head = "cs_9b".into());
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_d").unwrap().unwrap();
    let rev = || e.apps.deployed.lock().get(&(p.id.clone(), "web".to_string())).cloned();
    assert_eq!(rev().as_deref(), Some("cs_9b"));
    // A push that touches only ignored paths keeps the services; the preview stands for it.
    e.code.between.lock().insert("cs_9b..cs_9c".into(), vec!["docs/faq.md".into()]);
    e.code.edit("chg_d", |c| c.head = "cs_9c".into());
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    let row = e.engine.store.row(&p.id).unwrap();
    assert_eq!((row.head.as_str(), row.build(), rev().as_deref()), ("cs_9c", "cs_9b", Some("cs_9b")));
    assert_eq!(e.code.last_report("chg_d").head, "cs_9c", "reported for the new head");
    // Rules that no longer leave that push out redeploy it.
    let s = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "ignore_paths": ["*.txt"] })).unwrap();
    e.engine.store.put_settings(&e.project, &s).unwrap();
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!((e.engine.store.row(&p.id).unwrap().runs, rev().as_deref()), (None, Some("cs_9c")));
    let s = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "ignore_paths": ["docs/", "*.md"] })).unwrap();
    e.engine.store.put_settings(&e.project, &s).unwrap();
    e.code.between.lock().insert("cs_9c..cs_9e".into(), vec!["docs/faq.md".into()]);
    e.code.edit("chg_d", |c| c.head = "cs_9e".into());
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.row(&p.id).unwrap().build(), "cs_9c");
    // The next push that counts (measured from what runs) redeploys.
    e.code.between.lock().insert("cs_9c..cs_9d".into(), vec!["docs/faq.md".into(), "src/app.ts".into()]);
    e.code.edit("chg_d", |c| c.head = "cs_9d".into());
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(rev().as_deref(), Some("cs_9d"));
    assert_eq!(e.engine.store.row(&p.id).unwrap().runs, None);
    // `paths` alone never keeps a build: another service may run what it leaves out.
    let s = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "paths": ["src/"], "ignore_paths": [] })).unwrap();
    e.engine.store.put_settings(&e.project, &s).unwrap();
    e.code.between.lock().insert("cs_9d..cs_9f".into(), vec!["api/server.rs".into()]);
    e.code.edit("chg_d", |c| c.head = "cs_9f".into());
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(rev().as_deref(), Some("cs_9f"));
    // Manual mode leaves a preview made by hand alone.
    let s = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "manual": true, "paths": [] })).unwrap();
    e.engine.store.put_settings(&e.project, &s).unwrap();
    e.engine.pass().await.unwrap();
    assert!(e.engine.store.row_by_change("chg_d").unwrap().is_none(), "the rules' own preview goes");
    e.code.open(change("chg_h", 12));
    e.engine.create(&e.project, "acme/web", None, Some("chg_h"), "me").await.unwrap();
    e.engine.pass().await.unwrap();
    assert!(e.engine.store.row_by_change("chg_h").unwrap().is_some_and(|r| r.desired == Desired::Active), "made by hand, it stays");
}

#[tokio::test]
async fn a_preview_s_own_variables_redeploy_it_and_never_shadow_roll_s() {
    let e = env(json!({ "enabled": true, "env": { "FEATURE": "off" } }));
    e.code.open(change("chg_v", 4));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_v").unwrap().unwrap();
    let var = |k: &str| e.apps.vars.lock().get(&(p.id.clone(), "web".to_string())).and_then(|v| v.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()));
    let set = |k: &str, v: Option<&str>| std::collections::BTreeMap::from([(k.to_string(), v.map(String::from))]);
    assert_eq!(var("FEATURE").as_deref(), Some("off"));
    e.engine.set_vars(&p.id, set("FEATURE", Some("on"))).await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(var("FEATURE").as_deref(), Some("on"), "redeployed with the preview's own");
    for k in ["ROLL_PREVIEW", "PORT", "DATABASE_URL", "BAD-NAME"] {
        assert!(matches!(e.engine.set_vars(&p.id, set(k, Some("x"))).await, Err(roll_previews::Error::BadRequest(_))), "{k}");
    }
    e.engine.set_vars(&p.id, set("FEATURE", None)).await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(var("FEATURE").as_deref(), Some("off"), "removed: the project's again");
}

#[tokio::test]
async fn a_service_with_a_health_path_is_ready_only_once_it_answers_well() {
    let e = env(json!({ "enabled": true }));
    *e.code.health.lock() = Some("/healthz".into());
    *e.apps.answers.lock() = Some((500, "TypeError: db is undefined".into()));
    e.code.open(change("chg_hc", 5));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_hc").unwrap().unwrap();
    assert_eq!(p.state, State::Failed);
    assert!(p.error.as_deref().is_some_and(|m| m.contains("answered 500 on /healthz: TypeError: db is undefined")), "{:?}", p.error);
    // Fixed (or warmed up): the retry asks again and it's ready.
    *e.apps.answers.lock() = Some((200, "ok".into()));
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.row(&p.id).unwrap().state, State::Ready);
}

#[tokio::test]
async fn a_new_head_s_services_are_woken_so_the_first_visit_is_warm() {
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_w", 6));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_w").unwrap().unwrap();
    // The wakes run after the pass, on their own tasks.
    let woken = || async {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        e.apps.woken.lock().iter().filter(|(id, _)| *id == p.id).map(|(_, s)| s.clone()).collect::<Vec<_>>()
    };
    assert_eq!(woken().await, vec!["web".to_string()], "the change's own service, not the other repo's");
    // Off: a push leaves it asleep.
    let s = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "warm": false })).unwrap();
    e.engine.store.put_settings(&e.project, &s).unwrap();
    e.code.edit("chg_w", |c| c.head = "cs_6b".into());
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(woken().await.len(), 1);
}

#[tokio::test]
async fn earlier_pushes_stay_reachable_at_their_own_urls() {
    use roll_previews::ports::EnvSource;
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_p", 8));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_p").unwrap().unwrap();
    for head in ["cs_8b", "cs_8c", "cs_8d", "cs_8e"] {
        e.code.edit("chg_p", |c| c.head = head.into());
        e.engine.pass().await.unwrap();
        e.engine.pass().await.unwrap();
    }
    // The three latest earlier heads, newest first, each at a URL of its own.
    let view = e.engine.store.preview(&p.id).unwrap();
    let earlier: Vec<&str> = view.earlier.iter().map(|x| x.revision.as_str()).collect();
    assert_eq!(earlier, ["cs_8d", "cs_8c", "cs_8b"]);
    assert!(view.earlier.iter().all(|x| x.service == "web" && x.url != view.url.clone().unwrap()));
    assert!(e.apps.removed.lock().contains(&(p.id.clone(), "web@cs_8a".into())), "the oldest's container goes");
    // An earlier push runs its own head on its own host.
    let (t, _) = e.engine.env(&p.id, "web@cs_8c").await.unwrap();
    assert_eq!(t.revision, "cs_8c");
    assert!(view.earlier.iter().any(|x| x.url.contains(&t.host)));
    // A fork's earlier heads lost their approval with the push: they don't run.
    e.engine.store.update(&p.id, |r| r.trusted = false).unwrap();
    assert!(e.engine.env(&p.id, "web@cs_8c").await.is_err());
    // Deleted, they all go.
    e.code.edit("chg_p", |c| c.open = false);
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert!(e.apps.removed.lock().contains(&(p.id.clone(), "web@cs_8d".into())));
}

/// A signed-in person sees only their own projects' previews (as the workspace's projects); a
/// token, the operator and admins see all.
#[tokio::test]
async fn a_signed_in_person_sees_only_their_own_projects_previews() {
    use tower::ServiceExt;
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_o", 2));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_o").unwrap().unwrap();
    let call = |who: Option<(&str, bool)>, path: String| {
        let router = roll_previews::api::router(e.engine.clone());
        let who = who.map(|(user, admin)| roll_auth::SignedIn { user: user.into(), handle: user.into(), admin, code_owner: None, session: "ses_1".into() });
        async move {
            let mut req = axum::http::Request::get(path).body(axum::body::Body::empty()).unwrap();
            if let Some(w) = who {
                req.extensions_mut().insert(w);
            }
            let resp = router.oneshot(req).await.unwrap();
            let status = resp.status().as_u16();
            let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
            (status, String::from_utf8_lossy(&body).to_string())
        }
    };
    // Someone else's (the operator's project): not there for them.
    let other = Some(("usr_b", false));
    assert_eq!(call(other, "/v1/previews".into()).await, (200, "[]".to_string()));
    assert_eq!(call(other, format!("/v1/previews/{}", p.id)).await.0, 404);
    assert_eq!(call(other, format!("/v1/previews/{}/env", p.id)).await.0, 404);
    assert_eq!(call(other, format!("/v1/previews/projects/{}/settings", e.project)).await.0, 404);
    // Their own project's: there.
    let mine = e.ws.create("usr_b", "mine", "", None).unwrap();
    assert_eq!(call(other, format!("/v1/previews/projects/{}/settings", mine.id)).await.0, 200);
    // A token (no signed-in person) and an admin see it.
    assert_eq!(call(None, format!("/v1/previews/{}", p.id)).await.0, 200);
    assert_eq!(call(Some(("usr_c", true)), format!("/v1/previews/{}", p.id)).await.0, 200);
}

#[tokio::test]
async fn a_family_of_branches_gets_its_own_variables() {
    let e = env(json!({ "enabled": true, "env": { "FEATURE": "off" }, "env_for": { "feat/*": { "FEATURE": "branch" } } }));
    e.code.open(change("chg_f1", 3));
    e.code.open(ChangeInfo { branch: Some("fix/3".into()), ..change("chg_f2", 4) });
    e.engine.pass().await.unwrap();
    let var = |change: &str| {
        let id = e.engine.store.row_by_change(change).unwrap().unwrap().id;
        e.apps.vars.lock().get(&(id, "web".to_string())).and_then(|v| v.iter().find(|(n, _)| n == "FEATURE").map(|(_, v)| v.clone()))
    };
    assert_eq!(var("chg_f1").as_deref(), Some("branch"), "feat/3 matches feat/*");
    assert_eq!(var("chg_f2").as_deref(), Some("off"), "fix/3 keeps the project's");
    // A preview's own variable still wins.
    let id = e.engine.store.row_by_change("chg_f1").unwrap().unwrap().id;
    e.engine.set_vars(&id, std::collections::BTreeMap::from([("FEATURE".to_string(), Some("mine".to_string()))])).await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(var("chg_f1").as_deref(), Some("mine"));
}
