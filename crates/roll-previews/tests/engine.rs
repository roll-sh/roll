//! The reconciler against fake products: what a person relies on from a preview's lifecycle.

mod common;

use common::*;
use roll_previews::ports::ChangeInfo;
use roll_previews::{Desired, Kind, ResourceState, State, VisualState};
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
    // How its head came up, step by step, in order.
    let tl = e.engine.store.preview(&p.id).unwrap().timeline.expect("its timeline");
    let steps: Vec<&str> = tl.steps.iter().map(|x| x.step.as_str()).collect();
    assert_eq!((tl.head.as_str(), steps), ("cs_7a", vec!["data", "setup", "deploy", "checks", "ready"]));
    assert!(tl.steps.windows(2).all(|w| w[0].at <= w[1].at) && tl.started_at <= tl.steps[0].at);

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
    let e = env(json!({ "enabled": true, "resources": { "redis:rdb_cache": "skip" }, "seed": { "image": "node:22", "run": "npm run seed" } }));
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
    // Approved to run, not read for what it sends out: roll only, though the project's own
    // previews reach anything (`egress_forks`).
    let egress = e.apps.egress.lock().clone();
    assert!(!egress.is_empty() && egress.values().all(|x| *x == roll_previews::settings::EgressRule::Roll), "{egress:?}");
    let jobs = e.apps.seed_egress.lock().clone();
    assert!(!jobs.is_empty() && jobs.iter().all(|x| *x == roll_previews::settings::EgressRule::Roll), "its seed too: {jobs:?}");
    assert_eq!(p.reaches, roll_previews::settings::EgressRule::Roll, "and it says so");

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
async fn with_screenshots_a_baseline_of_the_default_branch_is_kept_to_compare_changes_with() {
    let e = env(json!({ "enabled": true, "max_previews": 1, "github": { "screenshot": true, "pages": ["/pricing", "api:/", "api:/health"], "openapi": "/openapi.json" } }));
    let mut png = Vec::new();
    image::RgbaImage::new(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    *e.apps.shot.lock() = Some(png);
    // Main's API, and the change's: one operation gone, one new.
    let ops = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    e.apps.api.lock().insert("cs_web0".into(), ops(&["DELETE /users/{id}", "GET /users", "GET /users param query:limit type integer"]));
    e.apps.api.lock().insert("cs_1a".into(), ops(&["GET /users", "GET /users param query:limit required", "GET /users param query:limit type integer", "POST /orders"]));
    e.code.open(change("chg_1", 1));
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    let rows = e.engine.store.rows(Some(&e.project)).unwrap();
    let base = rows.iter().find(|r| r.change_id.is_none()).expect("a baseline").clone();
    assert_eq!((base.repo.as_str(), base.branch.as_deref(), base.head.as_str(), base.state), ("acme/web", Some("main"), "cs_web0", State::Ready));
    let chg = e.engine.store.row_by_change("chg_1").unwrap().unwrap();
    assert_eq!(chg.state, State::Ready, "the baseline holds none of the project's one slot");
    // Whichever screenshot comes first, the change ends up compared with main's.
    let mut compared = None;
    for _ in 0..3000 {
        compared = e.code.last_report("chg_1").base_change;
        // Main's screenshot may compare the change before the change's own report is in: the
        // change's own comparison, right after, has its API too.
        // Each page's own comparison lands with it, or a moment after.
        let pages_compared = e.code.last_report("chg_1").pages.iter().all(|p| p.base_percent.is_some());
        if compared.as_ref().is_some_and(|c| c.api.is_some()) && pages_compared {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let compared = compared.expect("compared with the baseline");
    assert_eq!((compared.branch.as_str(), compared.percent), ("main", 0.0));
    assert_eq!(compared.api_breaking, ["GET /users: the parameter query:limit becomes required"], "what breaks inside an operation both keep");
    let api = compared.api.expect("its API against main's");
    assert_eq!((api.removed, api.added), (vec!["DELETE /users/{id}".to_string()], vec!["POST /orders".to_string()]), "what main's callers lose, and what's new");
    assert_eq!(e.engine.store.base_change(&e.engine.store.row_by_change("chg_1").unwrap().unwrap().id).unwrap().and_then(|b| b.head).as_deref(), Some("cs_web0"), "at its merge base");
    // Its other pages too, another service's included, each against the baseline's at the same path.
    let pages: Vec<_> = e.code.last_report("chg_1").pages.iter().map(|p| (p.path.clone(), p.base_percent)).collect();
    assert_eq!(pages, vec![("/pricing".to_string(), Some(0.0)), ("api:/".to_string(), Some(0.0)), ("api:/health".to_string(), Some(0.0))]);
    // It lasts while there are changes, whatever its age, and goes when the rule is off.
    e.engine.store.update(&base.id, |r| r.last_active_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.row(&base.id).unwrap().desired, Desired::Active);
    // A repo taken off the project's canvas takes its previews with it, the baseline too.
    e.ws.remove(roll_workspace::Scope::All, &e.project, roll_workspace::Kind::Repo, "acme/web").unwrap();
    e.engine.pass().await.unwrap();
    for id in [&base.id, &chg.id] {
        assert!(e.engine.store.row(id).map_or(true, |r| r.desired == Desired::Deleted), "no preview of a repo the project left");
    }
    e.ws.place(roll_workspace::Scope::All, &e.project, roll_workspace::Kind::Repo, "acme/web", 0.0, 0.0).unwrap();
    e.engine.pass().await.unwrap();
    let base = e.engine.store.rows(Some(&e.project)).unwrap().into_iter().find(|r| r.is_baseline() && r.desired == Desired::Active).expect("back with the repo");
    // On roll apps (whose pages can't be screenshotted yet) it goes too, rather than cost for nothing.
    *e.apps.catalog.lock() = Some(vec![roll_previews::ports::CatalogService { name: "web".into(), repo: Some("acme/web".into()), root: String::new(), memory_mb: 512 }]);
    e.engine.pass().await.unwrap();
    assert!(e.engine.store.row(&base.id).map_or(true, |r| r.desired == Desired::Deleted));
    *e.apps.catalog.lock() = None;
    let s = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "github": { "baseline": false } })).unwrap();
    e.engine.store.put_settings(&e.project, &s).unwrap();
    e.engine.pass().await.unwrap();
    assert!(e.engine.store.rows(Some(&e.project)).unwrap().iter().all(|r| !r.is_baseline() || r.desired == Desired::Deleted), "and with the rule off");
    assert_eq!(e.engine.store.row_by_change("chg_1").unwrap().map(|r| r.desired), Some(Desired::Active), "the change's preview came back with its repo");
}

#[tokio::test]
async fn a_visual_change_waits_for_someone_to_accept_it_at_each_head() {
    let e = env(json!({ "enabled": true, "visual_review": "check", "github": { "screenshot": true } }));
    let png = |v: u8| {
        let mut out = Vec::new();
        image::RgbaImage::from_pixel(4, 4, image::Rgba([v, v, v, 255])).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    };
    *e.apps.shot.lock() = Some(png(0));
    e.code.open(change("chg_1", 1));
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    let review = || e.engine.store.preview(&e.engine.store.row_by_change("chg_1").unwrap().unwrap().id).unwrap().visual_review;
    for _ in 0..3000 {
        if review().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(review().map(|v| v.state), Some(VisualState::Unchanged), "the same as main: nothing to review");
    // A push whose page looks different waits for someone.
    let chg = e.engine.store.row_by_change("chg_1").unwrap().unwrap();
    let base = e.engine.store.rows(Some(&e.project)).unwrap().into_iter().find(|r| r.change_id.is_none()).unwrap();
    // The first page and /pricing, against main's (Chromatic reviews each snapshot).
    let pricing = |v: u8| vec![roll_previews::ports::PageShot { path: "/pricing".into(), png: png(v), errors: 0, report: Default::default() }];
    e.engine.store.set_page_shots(&base.id, "cs_main", &pricing(0), &["/pricing".into()]).unwrap();
    let shoot = |head: &str, first: u8, page: u8| {
        e.engine.store.set_screenshot(&chg.id, head, &png(first), None, None).unwrap();
        e.engine.store.set_page_shots(&chg.id, head, &pricing(page), &["/pricing".into()]).unwrap();
        e.engine.store.compare_with_base(&chg.id, &base.id, "main", None).unwrap();
    };
    let waiting = || review().map(|v| v.pages.iter().filter(|p| p.by.is_none()).map(|p| p.path.clone()).collect::<Vec<_>>());
    shoot("cs_1b", 255, 255);
    assert_eq!(review().map(|v| (v.state, v.percent)), Some((VisualState::Changed, 100.0)));
    assert_eq!(waiting(), Some(vec!["".to_string(), "/pricing".to_string()]));
    assert!(e.engine.accept_visual(&base.id, "ada", &[], true, None, None, None).await.is_err(), "only a change's");
    // Not by its own author.
    let err = e.engine.accept_visual(&chg.id, "carol", &[], true, None, Some("cs_1b"), None).await.unwrap_err();
    assert!(err.to_string().contains("own author"), "{err}");
    let err = e.engine.accept_visual(&chg.id, "token:ci", &["carol".into()], false, None, Some("cs_1b"), None).await.unwrap_err();
    assert!(err.to_string().contains("own author"), "nor with their own token: {err}");
    // What was seen: an older screenshot's head is refused, this one's accepted.
    let err = e.engine.accept_visual(&chg.id, "ada", &[], true, None, Some("cs_1a"), None).await.unwrap_err();
    assert!(err.to_string().contains("a newer screenshot came in"), "{err}");
    // One page at a time: the change waits until every changed page is accepted.
    assert!(matches!(e.engine.accept_visual(&chg.id, "bob", &[], true, Some(Some("own_bob")), Some("cs_1b"), None).await, Err(roll_previews::Error::Forbidden(_))), "no write on its repo");
    let p = e.engine.accept_visual(&chg.id, "ada", &[], true, Some(Some("own_acme")), Some("cs_1b"), Some("/pricing")).await.unwrap();
    assert_eq!(p.visual_review.map(|v| v.state), Some(VisualState::Changed));
    // Waiting for review is an event for followers.
    let waits = e.engine.store.events(0, 500).unwrap().into_iter().filter(|x| x.preview == chg.id && x.kind == "visual_change").count();
    assert_eq!(waits, 1);
    assert_eq!(waiting(), Some(vec!["".to_string()]));
    assert!(e.engine.accept_visual(&chg.id, "ada", &[], true, None, Some("cs_1b"), Some("/nowhere")).await.is_err(), "a page it has no screenshot of");
    let p = e.engine.accept_visual(&chg.id, "ada", &[], true, None, Some("cs_1b"), Some("")).await.unwrap();
    assert_eq!(p.visual_review.map(|v| (v.state, v.by)), Some((VisualState::Accepted, Some("ada".into()))));
    assert_eq!(e.code.last_report("chg_1").visual_review.map(|v| v.state), Some(VisualState::Accepted), "the change is told at once");
    // A push that changes nothing visible keeps it.
    shoot("cs_1c", 255, 255);
    assert!(e.engine.store.carry_acceptance(&chg.id, 0.0).unwrap());
    assert_eq!(review().map(|v| v.state), Some(VisualState::Accepted), "pixel-identical to what was accepted");
    // One that changes /pricing only asks about /pricing again; the first page stays accepted.
    shoot("cs_1d", 255, 128);
    assert!(e.engine.store.carry_acceptance(&chg.id, 0.0).unwrap());
    assert_eq!(review().map(|v| v.state), Some(VisualState::Changed));
    assert_eq!(waiting(), Some(vec!["/pricing".to_string()]));
    // Accept with no page: every page there.
    e.engine.accept_visual(&chg.id, "ada", &[], true, None, Some("cs_1d"), None).await.unwrap();
    assert_eq!(review().map(|v| v.state), Some(VisualState::Accepted));
    // Under the project's threshold a page counts as unchanged (a clock `hide` can't name).
    let mut one_pixel = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 255, 255, 255]));
    one_pixel.put_pixel(0, 0, image::Rgba([0, 0, 0, 255]));
    let mut dot = Vec::new();
    one_pixel.write_to(&mut std::io::Cursor::new(&mut dot), image::ImageFormat::Png).unwrap();
    e.engine.store.set_screenshot(&base.id, "cs_main2", &png(255), None, None).unwrap();
    e.engine.store.set_page_shots(&base.id, "cs_main2", &pricing(255), &["/pricing".into()]).unwrap();
    e.engine.store.set_screenshot(&chg.id, "cs_1e", &dot, None, None).unwrap();
    e.engine.store.set_page_shots(&chg.id, "cs_1e", &pricing(255), &["/pricing".into()]).unwrap();
    e.engine.store.compare_with_base(&chg.id, &base.id, "main", None).unwrap();
    assert_eq!(review().map(|v| (v.state, v.percent)), Some((VisualState::Changed, 6.25)), "one pixel in 16");
    // A page that moves by itself (its two captures differ) is said to, its change still to review.
    let moving = roll_previews::PageHealth { errors: 0, first_error: None, failed: 0, first_failed: None, unstable: true };
    e.engine.store.set_page_report(&chg.id, Some(&moving), None, None, None, None).unwrap();
    let r = review().unwrap();
    assert!(r.state == VisualState::Changed && r.pages.iter().any(|p| p.path.is_empty() && p.unstable), "{r:?}");
    let s = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "github": { "visual_threshold": 10.0 } })).unwrap();
    e.engine.store.put_settings(&e.project, &s).unwrap();
    assert_eq!(review().map(|v| v.state), Some(VisualState::Unchanged));
    assert!(e.engine.store.settings(&e.project).unwrap().patched(&json!({ "github": { "visual_threshold": 60.0 } })).is_err(), "past 50%");
    // Another page gets the first page's checks, against main's page at the same path.
    let with_links = |broken: &[&str]| {
        let report = roll_previews::PageReport {
            links: Some(roll_previews::PageLinks { checked: 3, broken: broken.iter().map(|p| roll_previews::BrokenLink { path: p.to_string(), status: 404 }).collect() }),
            text: Some(format!("Pricing\n{}", broken.join("\n"))),
            ..Default::default()
        };
        vec![roll_previews::ports::PageShot { path: "/pricing".into(), png: png(255), errors: 0, report }]
    };
    e.engine.store.set_page_shots(&base.id, "cs_main2", &with_links(&[]), &["/pricing".into()]).unwrap();
    e.engine.store.keep_base_history(&base.id, "cs_main2").unwrap();
    e.engine.store.set_page_shots(&chg.id, "cs_1e", &with_links(&["/gone"]), &["/pricing".into()]).unwrap();
    e.engine.store.compare_with_base(&chg.id, &base.id, "main", None).unwrap();
    let pricing = e.engine.store.page_checks(&chg.id).unwrap().into_iter().find(|p| p.path == "/pricing").unwrap();
    let against = pricing.against.unwrap();
    assert_eq!((pricing.links.map(|l| l.broken.len()), against.links_added, against.text.map(|t| t.added)), (Some(1), Some(1), Some(vec!["/gone".to_string()])));
    // A list carries it light: the counts a row reads, not each page's checks.
    let light = e.engine.store.view(e.engine.store.row(&chg.id).unwrap()).unwrap().light();
    let page = light.pages.iter().find(|p| p.path == "/pricing").unwrap();
    assert!(page.links.is_none() && page.against.is_none() && page.base_percent.is_some());
    // Its endpoints against main's, timed with each one's screenshot (`github.endpoints`).
    // And their JSON answers' shapes: a field main's has and the change's doesn't.
    let timed = |ms, shape: &[&str]| roll_previews::PageSpeed {
        ttfb_ms: 1,
        fcp_ms: None,
        load_ms: 9,
        bytes: 1,
        lcp_ms: None,
        cls: None,
        endpoints: vec![roll_previews::EndpointTime { path: "/api/users".into(), ms, status: 200, shape: shape.iter().map(|l| l.to_string()).collect() }],
    };
    e.engine.store.set_screenshot(&base.id, "cs_main3", &png(255), Some(&timed(40, &["$ object", "email string"])), None).unwrap();
    e.engine.store.set_screenshot(&chg.id, "cs_1f", &dot, Some(&timed(140, &["$ object"])), None).unwrap();
    e.engine.store.compare_with_base(&chg.id, &base.id, "main", None).unwrap();
    let v = e.engine.store.preview(&chg.id).unwrap();
    let ep = v.base_change.unwrap().endpoints;
    assert!(ep.len() == 1 && ep[0].main_ms == Some(40) && ep[0].slower(), "{ep:?}");
    assert_eq!(ep[0].shape_changes.iter().map(|c| (c.path.as_str(), c.now.is_none())).collect::<Vec<_>>(), [("email", true)]);
    assert!(v.page_speed.unwrap().now.endpoints[0].shape.is_empty(), "the record doesn't carry the shapes");
    // An answer that failed isn't compared by shape (its error body isn't the endpoint's answer).
    let mut failing = timed(140, &["$ object"]);
    failing.endpoints[0].status = 500;
    e.engine.store.set_screenshot(&chg.id, "cs_1g", &dot, Some(&failing), None).unwrap();
    e.engine.store.compare_with_base(&chg.id, &base.id, "main", None).unwrap();
    assert!(e.engine.store.preview(&chg.id).unwrap().base_change.unwrap().endpoints[0].shape_changes.is_empty());
    // Main's API document read and the change's not: said so, nothing compared.
    e.engine.store.set_page_report(&base.id, None, None, None, None, Some(&["GET /users".to_string()])).unwrap();
    e.engine.store.set_page_report(&chg.id, None, None, None, None, None).unwrap();
    e.engine.store.compare_with_base(&chg.id, &base.id, "main", None).unwrap();
    let b = e.engine.store.preview(&chg.id).unwrap().base_change.unwrap();
    assert!(b.api_unread && b.api.is_none(), "{b:?}");
}

/// Production's browser checks on a preview's head: read after it's up (never holding it), on the
/// record and reported to the change, until each has run.
#[tokio::test]
async fn production_browser_checks_on_a_preview_are_read_after_its_head_is_up() {
    use roll_previews::ports::BrowserCheckResult;
    let e = env(json!({ "enabled": true }));
    let check = |passed, error: Option<&str>| BrowserCheckResult {
        name: "checkout".into(),
        passed,
        error: error.map(str::to_string),
        files: (passed == Some(false)).then(|| roll_previews::BrowserCheckFiles { service_id: "svc_web".into(), environment: "pv-x".into() }),
    };
    e.apps.browser.lock().insert("web".into(), vec![check(None, None)]);
    e.code.open(change("chg_bc", 6));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_bc").unwrap().unwrap();
    assert_eq!(p.state, State::Ready, "not held up");
    e.engine.pass().await.unwrap();
    let state = |e: &Env| e.engine.store.preview(&p.id).unwrap().browser_checks.iter().map(|c| (c.name.clone(), c.state, c.error.clone())).collect::<Vec<_>>();
    assert_eq!(state(&e), [("checkout".to_string(), roll_previews::CheckState::Pending, None)]);
    e.apps.browser.lock().insert("web".into(), vec![check(Some(false), Some("Timeout 5000ms exceeded\u{1b}[31m"))]);
    e.engine.pass().await.unwrap();
    assert_eq!(state(&e), [("checkout".to_string(), roll_previews::CheckState::Failed, Some("Timeout 5000ms exceeded\\u{1b}[31m".to_string()))], "printable");
    assert!(e.code.last_report("chg_bc").browser_checks.iter().any(|c| c.name == "checkout" && c.state == roll_previews::CheckState::Failed), "the change hears it");
    // Where its screenshot and trace are, for the sheet's links.
    let files = e.engine.store.preview(&p.id).unwrap().browser_checks[0].files.clone();
    assert_eq!(files.map(|f| (f.service_id, f.environment)), Some(("svc_web".to_string(), "pv-x".to_string())));
    // And followers of the events, once a head.
    e.engine.pass().await.unwrap();
    let said = e.engine.store.events(0, 500).unwrap().into_iter().filter(|x| x.preview == p.id && x.kind == "browser_checks_failed").count();
    assert_eq!(said, 1);
}

/// What happened to previews, for followers: made, up, gone, in the order it happened, from any
/// point (`latest`: from now on).
#[tokio::test]
async fn previews_events_follow_a_preview_from_made_to_gone() {
    let e = env(json!({ "enabled": true }));
    let from = e.engine.store.latest_event().unwrap();
    e.code.open(change("chg_ev", 4));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_ev").unwrap().unwrap();
    e.code.edit("chg_ev", |c| c.open = false);
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    let got = e.engine.store.events(from, 100).unwrap();
    let mine: Vec<(&str, &str)> = got.iter().filter(|x| x.preview == p.id).map(|x| (x.kind.as_str(), x.data["head"].as_str().unwrap_or(""))).collect();
    assert_eq!(mine, [("created", "cs_4a"), ("ready", "cs_4a"), ("deleted", "cs_4a")]);
    assert!(got.windows(2).all(|w| w[0].seq < w[1].seq) && got.iter().all(|x| x.project == e.project && x.change.as_deref() == Some("chg_ev")));
    let second = got[1].seq;
    assert_eq!(e.engine.store.events(got[0].seq, 100).unwrap().first().map(|x| x.seq), Some(second), "from any point");
    assert_eq!(e.engine.store.latest_event().unwrap(), got.last().unwrap().seq);
}

/// What a preview's services use right now (Render's metrics): the running ones, with the memory
/// each runs with; a service that isn't running has none.
#[tokio::test]
async fn a_previews_metrics_are_what_its_running_services_use() {
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_u", 4));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_u").unwrap().unwrap();
    let web = e.engine.store.services(&p.id).unwrap().into_iter().next().unwrap();
    e.apps.metrics.lock().insert(web.name.clone(), (0.25, 96 << 20));
    let m = e.engine.metrics(&p.id).await.unwrap();
    assert_eq!(m, [roll_previews::ServiceMetrics { service: web.name.clone(), cpu: 0.25, memory_bytes: 96 << 20, memory_mb: web.memory_mb }], "only the running one, against what it runs with");
    assert!(matches!(e.engine.metrics("pv_nope").await, Err(roll_previews::Error::NotFound(_))));
}

/// A trusted preview's own services get roll's word for the clouds the project names
/// (`oidc_audiences`): an address of their own (`ROLL_OIDC_URL`) answering a token for a named
/// audience, as the service it is (another audience is a 403, an address that isn't one a 404),
/// answered again for a minute rather than signed again.
#[tokio::test]
async fn a_previews_service_gets_roll_s_word_for_a_named_cloud_only() {
    use roll_previews::ports::EnvSource;
    struct FakeIssuer(std::sync::Mutex<Vec<(String, String, String)>>);
    #[async_trait::async_trait]
    impl roll_previews::ports::Issuer for FakeIssuer {
        async fn issue(&self, _project: &str, _repo: &str, preview: &str, service: &str, aud: &str, _ttl: std::time::Duration) -> anyhow::Result<Option<String>> {
            self.0.lock().unwrap().push((preview.into(), service.into(), aud.into()));
            Ok(Some(format!("jwt-for-{service}-{aud}")))
        }
    }
    let e = env(json!({ "enabled": true, "oidc_audiences": ["sts.amazonaws.com", "roll"] }));
    let issuer = Arc::new(FakeIssuer(std::sync::Mutex::new(Vec::new())));
    let _ = e.engine.issuer.set(issuer.clone());
    e.code.open(change("chg_oi", 9));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_oi").unwrap().unwrap();
    let web = e.engine.store.services(&p.id).unwrap()[0].name.clone();
    let (_, vars) = EnvSource::env(e.engine.as_ref(), &p.id, &web).await.unwrap();
    let var = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let url = var("ROLL_OIDC_URL").expect("an address of its own");
    assert!(url.starts_with(&format!("http://127.0.0.1:7070/v1/previews/{}/oidc/", p.id)) && var("ROLL_OIDC_AUDIENCE").as_deref() == Some("sts.amazonaws.com"), "{url}");
    let token = url.rsplit('/').next().unwrap();
    assert_eq!(e.engine.oidc(&p.id, token, None).await.unwrap().token, format!("jwt-for-{web}-sts.amazonaws.com"));
    assert_eq!(e.engine.oidc(&p.id, token, Some("roll")).await.unwrap().token, format!("jwt-for-{web}-roll"));
    assert!(matches!(e.engine.oidc(&p.id, token, Some("storage.googleapis.com")).await, Err(roll_previews::Error::Forbidden(_))));
    assert!(matches!(e.engine.oidc(&p.id, "0123abcd", None).await, Err(roll_previews::Error::NotFound(_))));
    e.engine.oidc(&p.id, token, None).await.unwrap();
    assert_eq!(issuer.0.lock().unwrap().len(), 2, "a minute's token answered again, not signed again");
}

/// A look at a data branch from the sheet (Neon's Tables): a ready Postgres branch's tables and a
/// table's rows, through the product as the branch's own role; a kind with no way to show it yet
/// is a 409, and a data branch the preview doesn't have a 404.
#[tokio::test]
async fn a_previews_data_branch_is_looked_at_from_the_sheet() {
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_db", 8));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_db").unwrap().unwrap();
    let res = e.engine.store.resources(&p.id).unwrap();
    let id = |k: Kind| res.iter().find(|r| r.kind == k).map(|r| format!("{}:{}", r.kind.as_str(), r.parent)).unwrap();
    let tables = e.engine.browse(&p.id, &id(Kind::Postgres), None).await.unwrap();
    assert!(matches!(&tables, roll_previews::Browse::Tables { tables } if tables[0].name == "public.users"), "{tables:?}");
    let rows = e.engine.browse(&p.id, &id(Kind::Postgres), Some("public.users")).await.unwrap();
    assert!(matches!(&rows, roll_previews::Browse::Rows { rows, .. } if rows[0][0].as_deref().is_some_and(|v| v.ends_with(".invalid"))), "{rows:?}");
    assert!(matches!(e.engine.browse(&p.id, &id(Kind::Redis), None).await, Err(roll_previews::Error::Conflict(_))));
    assert!(matches!(e.engine.browse(&p.id, "postgres:nope", None).await, Err(roll_previews::Error::NotFound(_))));
}

/// A secret value its pages serve (Netlify's secrets scan) is named on its record and in its
/// activity, with where, never the value; one meant for the browser (`NEXT_PUBLIC_…`) or a user
/// name isn't.
#[tokio::test]
async fn a_secret_value_its_pages_serve_is_named_on_its_record() {
    let e = env(json!({ "enabled": true, "github": { "screenshot": true } }));
    e.secrets.values.lock().push(("STRIPE_KEY".into(), "sk_test_51Hx9abc".into()));
    e.secrets.values.lock().push(("NEXT_PUBLIC_MAPS_KEY".into(), "pk_maps_123456".into()));
    e.secrets.values.lock().push(("PGUSER".into(), "pv_role_abcdef".into()));
    let mut png = Vec::new();
    image::RgbaImage::new(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    *e.apps.shot.lock() = Some(png);
    *e.apps.served.lock() = vec![("/assets/app.js".into(), b"const k = 'sk_test_51Hx9abc', m = 'pk_maps_123456', u = 'pv_role_abcdef';".to_vec())];
    e.code.open(change("chg_ss", 5));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_ss").unwrap().unwrap();
    let served = |e: &Env| e.engine.store.preview(&p.id).unwrap().served_secrets;
    for _ in 0..3000 {
        if !served(&e).is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(served(&e), [roll_previews::ServedSecret { key: "STRIPE_KEY".into(), path: "/assets/app.js".into() }]);
    let said: Vec<String> = e.engine.store.activity(&p.id).unwrap().into_iter().map(|a| a.message).filter(|m| m.contains("serve secret values")).collect();
    assert!(said.len() == 1 && said[0].contains("STRIPE_KEY in /assets/app.js") && !said[0].contains("sk_test"), "{said:?}");
}

/// A preview's errors are its services' groups, newest first, named by service, with a secret its
/// code printed shown as `***` in the title and the sample (as its logs are), a value a cut split
/// included; its values are read only when there's an error (counting reads none), and a
/// fingerprint isn't the backend's; 404 for no such preview.
#[tokio::test]
async fn a_previews_errors_are_its_services_groups_masked() {
    let e = env(json!({ "enabled": true }));
    e.secrets.values.lock().push(("STRIPE_KEY".into(), "sk_test_51Hx9".into()));
    e.code.open(change("chg_er", 4));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_er").unwrap().unwrap();
    let web = e.engine.store.services(&p.id).unwrap().into_iter().next().unwrap();
    let masks_read = || e.secrets.reasons.lock().iter().filter(|r| *r == "logs").count();
    let before = masks_read();
    assert!(e.engine.errors(&p.id, 60).await.unwrap().is_empty());
    assert_eq!(masks_read(), before, "no errors, no values read");
    let group = |f: &str, title: &str, last_seen: i64| roll_previews::ErrorGroup { service: String::new(), fingerprint: f.into(), kind: "line".into(), title: title.into(), count: 2, first_seen: 1, last_seen, sample: format!("{title}\n  at charge()"), trace: None };
    e.apps.errors.lock().insert(web.name.clone(), vec![group("aa", "card declined sk_test_51", 5), group("bb", "Stripe rejected sk_test_51Hx9", 9)]);
    assert_eq!(e.engine.error_count(&p.id, 360).await.unwrap(), 4);
    assert_eq!(masks_read(), before, "counting reads no values");
    let got = e.engine.errors(&p.id, 360).await.unwrap();
    assert_eq!(got.iter().map(|g| (g.service.as_str(), g.title.as_str())).collect::<Vec<_>>(), [(web.name.as_str(), "Stripe rejected ***"), (web.name.as_str(), "card declined ***")]);
    assert_eq!((got[0].sample.as_str(), got[1].sample.as_str()), ("Stripe rejected ***\n  at charge()", "card declined ***\n  at charge()"));
    assert!(got.iter().all(|g| g.fingerprint.len() == 16 && !["aa", "bb"].contains(&g.fingerprint.as_str())), "{got:?}");
    assert!(matches!(e.engine.errors("pv_nope", 60).await, Err(roll_previews::Error::NotFound(_))));
}

/// A project that sends its logs to a drain (roll apps' drains with `previews`) gets its previews'
/// services on roll's runtime there too, masked, in the preview's `pv-` environment: one
/// follower a running service while a drain wants it, and a follower after a restart starts at
/// the last line sent, so nothing is sent twice. Its jobs' output is the `build` source.
#[tokio::test]
async fn a_previews_runtime_output_goes_to_the_projects_log_drains_masked() {
    let e = env(json!({ "enabled": true, "seed": { "image": "node:22", "run": "node seed.js" } }));
    e.secrets.values.lock().push(("STRIPE_KEY".into(), "sk_test_51Hx9".into()));
    let drains = Arc::new(common::FakeDrains::default());
    let _ = e.engine.drains.set(drains.clone());
    e.code.open(change("chg_d", 12));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_d").unwrap().unwrap();
    let web = e.engine.store.services(&p.id).unwrap().into_iter().next().unwrap().name;
    e.apps.running.lock().insert((p.id.clone(), web.clone()));
    e.engine.pass().await.unwrap();
    assert!(e.apps.drain_since.lock().is_empty() && drains.sent.lock().is_empty(), "no drain wants previews' output: nothing followed, the seed's output not sent");
    drains.wanted.store(true, Ordering::SeqCst);
    // A reset seeds again: the job's output, masked, as the `build` source.
    e.engine.reset(&p.id, Vec::new()).unwrap();
    e.engine.pass().await.unwrap();
    let seeded: Vec<_> = drains.sent.lock().drain(..).map(|(env, svc, source, l)| (env, svc, source, l.text)).collect();
    assert_eq!(seeded, [(format!("pv-{}", p.id.trim_start_matches("pv_")), "seed".to_string(), "build", "seed output, STRIPE_KEY=***".to_string())]);
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.apps.drain_since.lock().len(), 1, "one follower a service");
    let say = |stderr: bool, l: &str| {
        let tx = e.apps.drain_tx.lock().clone().unwrap();
        let l = l.to_string();
        async move { tx.send((stderr, l)).await.unwrap() }
    };
    say(false, "2026-11-18T10:00:00.000000001Z listening on :3000").await;
    say(true, "2026-11-18T10:00:00.000000002Z charging with sk_test_51Hx9").await;
    let sent = |n: usize| {
        let drains = drains.clone();
        async move {
        for _ in 0..200 {
            if drains.sent.lock().len() >= n {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        drains.sent.lock().iter().map(|(env, svc, _, l)| (env.clone(), svc.clone(), l.stderr, l.text.clone())).collect::<Vec<_>>()
        }
    };
    let env_name = format!("pv-{}", p.id.trim_start_matches("pv_"));
    assert_eq!(sent(2).await, [(env_name.clone(), web.clone(), false, "listening on :3000".to_string()), (env_name.clone(), web.clone(), true, "charging with ***".to_string())]);
    assert_eq!(drains.sent.lock()[0].3.t, 1_794_996_000_000);
    // The container goes (a redeploy): the next follower starts at the last line sent, and the
    // line Docker gives again from there isn't sent twice.
    *e.apps.drain_tx.lock() = None;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    e.engine.pass().await.unwrap();
    // From the earlier of the two streams' last lines: stdout's (stderr's own last is skipped as it comes again).
    assert_eq!(e.apps.drain_since.lock().last().map(String::as_str), Some("2026-11-18T10:00:00.000000001Z"));
    say(true, "2026-11-18T10:00:00.000000002Z charging with sk_test_51Hx9").await;
    say(false, "2026-11-18T10:00:01.000000000Z ready").await;
    let all = sent(3).await;
    assert_eq!(all.len(), 3, "{all:?}");
    assert_eq!(all[2].3, "ready");
    // stdout and stderr come through pipes of their own: a stderr line may come before an earlier
    // stdout one, and both are sent.
    say(true, "2026-11-18T10:00:03.000000000Z card declined").await;
    say(false, "2026-11-18T10:00:02.000000000Z charging order 42").await;
    let all = sent(5).await;
    assert_eq!(all.iter().skip(3).map(|l| l.3.as_str()).collect::<Vec<_>>(), ["card declined", "charging order 42"], "{all:?}");
}

/// A shell in a preview's running service (Render's shell, `heroku run`): what's typed reaches its
/// terminal, a viewer who joins later gets the scrollback, the activity says who opened it, a
/// preview holds a few at once, and closing one ends it.
#[tokio::test]
async fn a_shell_in_a_previews_service_is_said_and_bounded() {
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_sh", 14));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_sh").unwrap().unwrap();
    let sid = e.engine.open_shell(&p.id, None, "ana", 80, 24).await.unwrap();
    let web = e.engine.store.services(&p.id).unwrap().into_iter().next().unwrap().name;
    assert!(e.engine.store.activity(&p.id).unwrap().iter().any(|a| a.message == format!("ana opened a shell in {web}")));
    let (first, mut rx, closed) = e.engine.shell_screen(&p.id, &sid).unwrap();
    assert!(first.is_empty());
    e.engine.shell_keys(&p.id, &sid, b"echo hi\r".to_vec()).await.unwrap();
    let mut seen = Vec::new();
    while seen.len() < 8 {
        seen.extend_from_slice(&tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.unwrap().unwrap());
    }
    assert_eq!(seen, b"echo hi\r");
    assert_eq!(e.engine.shell_screen(&p.id, &sid).unwrap().0, b"echo hi\r", "a later viewer gets the scrollback");
    assert!(matches!(e.engine.shell_screen("pv_other", &sid), Err(roll_previews::Error::NotFound(_))), "a shell is only its preview's");
    let mut more = Vec::new();
    for _ in 1..roll_previews::engine::shell::MAX_SHELLS {
        more.push(e.engine.open_shell(&p.id, Some(&web), "ana", 80, 24).await.unwrap());
    }
    assert!(matches!(e.engine.open_shell(&p.id, None, "ana", 80, 24).await, Err(roll_previews::Error::Conflict(_))), "a preview holds a few at once");
    e.engine.close_shell(&p.id, &sid).unwrap();
    assert!(*closed.borrow(), "closing it ends it");
    for _ in 0..200 {
        if e.engine.shell_screen(&p.id, &sid).is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(e.engine.shell_screen(&p.id, &sid).is_err(), "a closed shell is gone");
    e.engine.open_shell(&p.id, None, "ana", 80, 24).await.expect("its place is free again");
}

/// A preview's traces (the project's `traces: true`): its services get the OpenTelemetry SDK's
/// variables, its intake keeps spans only with the preview's own token (anything else is a 404),
/// and they're read back as traces, masked as its logs are; an error group whose sample names one
/// opens it.
#[tokio::test]
async fn a_previews_traces_are_kept_by_its_token_and_read_masked() {
    use roll_previews::engine::traces::Span;
    let e = env(json!({ "enabled": true, "traces": true }));
    e.secrets.values.lock().push(("STRIPE_KEY".into(), "sk_test_51Hx9".into()));
    e.secrets.values.lock().push(("SIGNING_KEY".into(), "0f1e2d3c4b5a69788796a5b4c3d2e1f0".into()));
    e.code.open(change("chg_tr", 17));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_tr").unwrap().unwrap();
    let web = e.engine.store.services(&p.id).unwrap().into_iter().next().unwrap().name;
    // What a start of the service gets (the runtime reads it then, as its `EnvSource`).
    let (_, vars) = roll_previews::ports::EnvSource::env(e.engine.as_ref(), &p.id, &web).await.unwrap();
    let var = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let endpoint = var("OTEL_EXPORTER_OTLP_ENDPOINT");
    assert!(endpoint.starts_with(&format!("http://127.0.0.1:7070/v1/previews/{}/otlp/", p.id)), "{endpoint}");
    assert_eq!(var("OTEL_SERVICE_NAME"), web);
    let token = endpoint.rsplit('/').next().unwrap().to_string();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    let span = |id: &str, parent: &str, name: &str, error: bool, attrs: Vec<(String, String)>| Span { trace: "a".repeat(32), span: id.into(), parent: parent.into(), name: name.into(), service: web.clone(), start: now - 30_000_000, end: now, error, attrs };
    assert!(matches!(e.engine.keep_spans(&p.id, "not-the-token", vec![span("0000000000000001", "", "GET /", false, vec![])]), Err(roll_previews::Error::NotFound(_))));
    let kept = e.engine.keep_spans(&p.id, &token, vec![span("0000000000000001", "", "GET /checkout?key=sk_test_51Hx9", false, vec![]), span("0000000000000002", "0000000000000001", "charge", true, vec![("stripe.key".into(), "sk_test_51Hx9".into())])]).unwrap();
    assert_eq!(kept, 2);
    let list = e.engine.traces(&p.id, 60).await.unwrap();
    assert_eq!((list["traces"][0]["name"].as_str(), list["traces"][0]["spans"].as_u64(), list["traces"][0]["error"].as_bool()), (Some("GET /checkout?key=***"), Some(2), Some(true)), "{list}");
    let one = e.engine.trace(&p.id, &"a".repeat(32)).await.unwrap();
    assert_eq!(one["spans"][1]["attrs"][0][1].as_str(), Some("***"), "{one}");
    assert!(!one.to_string().contains("sk_test_51Hx9"));
    // A service on roll apps keeps its own (its pv- environment's, turned on by the deploy): on the
    // same list, and one opens from it, masked the same way.
    assert_eq!(e.apps.traces_asked.lock().get(&(p.id.clone(), web.clone())), Some(&true), "the deploy asks for the environment's traces");
    let row = roll_previews::engine::traces::TraceRow { trace: "b".repeat(32), name: "POST /pay sk_test_51Hx9".into(), service: "api".into(), start_ms: now / 1_000_000 + 5, ms: 12, spans: 1, error: false, repeated: None };
    *e.apps.app_traces.lock() = vec![(row, vec![span("0000000000000009", "", "POST /pay sk_test_51Hx9", false, vec![])])];
    let list = e.engine.traces(&p.id, 60).await.unwrap();
    assert_eq!(list["traces"].as_array().map(Vec::len), Some(2), "{list}");
    assert_eq!(list["traces"][0]["name"].as_str(), Some("POST /pay ***"), "the newest first, masked");
    let apps_one = e.engine.trace(&p.id, &"b".repeat(32)).await.unwrap();
    assert_eq!(apps_one["spans"][0]["name"].as_str(), Some("POST /pay ***"));
    // Each route's time from the traces (masked), with nothing of main's to compare yet.
    let routes = e.engine.routes(&p.id, 60).await.unwrap();
    assert!(routes["routes"].as_array().unwrap().iter().any(|r| r["name"] == "GET /checkout?key=***" && r["count"] == 1 && r["main_p50_ms"].is_null()), "{routes}");
    // Turned off, what roll apps kept isn't the preview's to show.
    let mut s = e.engine.store.settings(&e.project).unwrap();
    s.traces = false;
    e.engine.store.put_settings(&e.project, &s).unwrap();
    assert_eq!(e.engine.traces(&p.id, 60).await.unwrap()["traces"].as_array().map(Vec::len), Some(1), "only the runtime's kept spans");
    // A preview's spans are bounded by bytes (16 MiB), not only by count: big spans push the oldest out.
    let big = |i: usize| Span { trace: format!("{i:032x}"), span: "0000000000000001".into(), parent: String::new(), name: "big".into(), service: web.clone(), start: now - 1_000_000, end: now, error: false, attrs: vec![("blob".into(), "x".repeat(1 << 20))] };
    e.engine.keep_spans(&p.id, &token, (0..20).map(big).collect()).unwrap();
    let n = e.engine.traces(&p.id, 60).await.unwrap()["traces"].as_array().map(Vec::len).unwrap();
    assert!(n <= 16, "{n} traces of 1 MiB spans kept");
    // An error whose sample names a kept trace (an SDK logs its request's trace id) opens it; one
    // naming a trace it doesn't keep doesn't.
    let kept = e.engine.traces(&p.id, 60).await.unwrap()["traces"][0]["trace"].as_str().unwrap().to_string();
    let group = |f: &str, title: &str, trace: &str| roll_previews::ErrorGroup { service: String::new(), fingerprint: f.into(), kind: "line".into(), title: title.into(), count: 1, first_seen: 1, last_seen: 2, sample: format!("{title} trace_id={trace}"), trace: None };
    e.apps.errors.lock().insert(web.clone(), vec![group("aa", "kept", &kept.to_ascii_uppercase()), group("bb", "not kept", &"c".repeat(32))]);
    let got = e.engine.errors(&p.id, 60).await.unwrap();
    let trace_of = |title: &str| got.iter().find(|g| g.title == title).and_then(|g| g.trace.clone());
    assert_eq!((trace_of("kept"), trace_of("not kept")), (Some(kept), None), "{got:?}");
    // A secret's value sent as a trace id (the code chose it) shows as `***` too.
    let hex = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
    e.engine.keep_spans(&p.id, &token, vec![Span { trace: hex.into(), ..span("0000000000000003", "", "GET /x", false, vec![]) }]).unwrap();
    let list = e.engine.traces(&p.id, 60).await.unwrap().to_string();
    assert!(!list.contains(hex) && list.contains("\"trace\":\"***\""), "{list}");
}

/// A route clearly slower than on main's preview (from both previews' traces) goes on the change's
/// record, and the change is reported again with it (its pull request's comment).
#[tokio::test]
async fn a_route_slower_than_mains_reaches_the_record_and_the_report() {
    use roll_previews::engine::traces::Span;
    let e = env(json!({ "enabled": true, "traces": true, "github": { "screenshot": true } }));
    let mut png = Vec::new();
    image::RgbaImage::new(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    *e.apps.shot.lock() = Some(png);
    e.code.open(change("chg_sr", 18));
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    let base = e.engine.store.rows(Some(&e.project)).unwrap().into_iter().find(|r| r.is_baseline()).expect("main's preview");
    let p = e.engine.store.row_by_change("chg_sr").unwrap().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    // Each request a root span and its query, the query the time that grew.
    let keep = |id: &str, n: u64, ms: u64, query_ms: u64| {
        let spans = (0..n)
            .flat_map(|i| {
                let trace = format!("{:016x}{i:016x}", ms);
                [
                    Span { trace: trace.clone(), span: "0000000000000001".into(), parent: String::new(), name: "GET /orders".into(), service: "web".into(), start: now - ms * 1_000_000, end: now, error: false, attrs: vec![] },
                    Span { trace, span: "0000000000000002".into(), parent: "0000000000000001".into(), name: "SELECT orders".into(), service: "web".into(), start: now - query_ms * 1_000_000, end: now, error: false, attrs: vec![] },
                ]
            })
            .collect();
        e.engine.keep_spans(id, &e.engine.store.trace_token(id).unwrap(), spans).unwrap();
    };
    keep(&base.id, 5, 100, 50);
    keep(&p.id, 5, 400, 330);
    e.engine.read_slower_routes().await;
    let s = e.engine.store.preview(&p.id).unwrap().slower_routes.expect("slower than main's");
    assert_eq!((s.branch.as_str(), s.routes.len(), s.routes[0].name.as_str(), s.routes[0].p50_ms, s.routes[0].main_p50_ms), ("main", 1, "GET /orders", 400, 100));
    assert_eq!(s.routes[0].grew, Some(roll_previews::GrewSpan { span: "SELECT orders".into(), ms: 330, main_ms: 50 }), "where its time went");
    assert_eq!(e.code.last_report("chg_sr").slower_routes, Some(s), "reported again with it");
    assert!(e.engine.store.preview(&base.id).unwrap().slower_routes.is_none(), "main isn't slower than itself");
    // A route with a budget of its own is over it whatever main's does (main's own, here).
    let mut st = e.engine.store.settings(&e.project).unwrap();
    st.github.budget.routes.insert("GET /orders".into(), 80);
    e.engine.store.put_settings(&e.project, &st).unwrap();
    e.engine.read_slower_routes().await;
    let s = e.engine.store.preview(&base.id).unwrap().slower_routes.expect("main's over its budget");
    assert_eq!((s.routes[0].name.as_str(), s.routes[0].p50_ms, s.routes[0].budget_ms), ("GET /orders", 100, Some(80)));
}

/// An error group main's preview doesn't have goes on the change's record (masked, the most
/// frequent first), and the change is reported again with it; one main has too doesn't count.
#[tokio::test]
async fn an_error_mains_preview_doesnt_have_reaches_the_record_and_the_report() {
    let e = env(json!({ "enabled": true, "github": { "screenshot": true } }));
    e.secrets.values.lock().push(("STRIPE_KEY".into(), "sk_test_51Hx9".into()));
    let mut png = Vec::new();
    image::RgbaImage::new(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    *e.apps.shot.lock() = Some(png);
    e.code.open(change("chg_ne", 19));
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    let base = e.engine.store.rows(Some(&e.project)).unwrap().into_iter().find(|r| r.is_baseline()).expect("main's preview");
    let p = e.engine.store.row_by_change("chg_ne").unwrap().unwrap();
    let web = e.engine.store.services(&p.id).unwrap().into_iter().next().unwrap().name;
    let group = |f: &str, title: &str, count: u64| roll_previews::ErrorGroup { fingerprint: f.into(), kind: "line".into(), title: title.into(), count, first_seen: 1, last_seen: 2, sample: title.into(), ..Default::default() };
    // One main's has too under another fingerprint, its line naming each preview's own id: not new.
    let own = |id: &str| format!("lost the connection for {id}");
    e.apps.errors.lock().insert(format!("{}/{web}", base.id), vec![group("old", "a flaky upstream", 3), group("own-main", &own(&base.id), 2)]);
    e.apps.errors.lock().insert(format!("{}/{web}", p.id), vec![group("old", "a flaky upstream", 4), group("rare", "RangeError", 1), group("new", "Stripe rejected sk_test_51Hx9", 9), group("own-mine", &own(&p.id), 2)]);
    e.engine.read_new_errors().await;
    let seen = e.engine.store.preview(&p.id).unwrap();
    assert!(seen.regressions_compared, "compared at its head");
    let n = seen.new_errors.expect("new against main's");
    assert_eq!(n.head, p.head, "read at its head");
    assert_eq!((n.branch.as_str(), n.total, n.errors.iter().map(|x| x.title.as_str()).collect::<Vec<_>>()), ("main", 2, vec!["Stripe rejected ***", "RangeError"]));
    assert_eq!(e.code.last_report("chg_ne").new_errors, Some(n), "reported again with it");
    // Required, someone other than its author accepts them at its head (a person, not a token).
    let mut s = e.engine.store.settings(&e.project).unwrap();
    s.regressions = roll_previews::settings::RegressionsRule::Required;
    e.engine.store.put_settings(&e.project, &s).unwrap();
    assert!(matches!(e.engine.accept_regressions(&p.id, "carol", &[], true, None, None).await, Err(roll_previews::Error::Forbidden(_))), "not its author");
    // Nor under another of their names (their roll handle, the change carrying their GitHub login).
    assert!(matches!(e.engine.accept_regressions(&p.id, "carol-roll", &["carol-roll".into(), "carol".into()], true, None, None).await, Err(roll_previews::Error::Forbidden(_))), "their linked login");
    assert!(matches!(e.engine.accept_regressions(&p.id, "token:ci", &[], false, None, None).await, Err(roll_previews::Error::Forbidden(_))), "not a token");
    assert!(matches!(e.engine.accept_regressions(&p.id, "ada", &[], true, None, Some("cs_old")).await, Err(roll_previews::Error::Conflict(_))), "a newer push");
    assert!(matches!(e.engine.accept_regressions(&p.id, "bob", &[], true, Some(Some("own_bob")), Some(&p.head)).await, Err(roll_previews::Error::Forbidden(_))), "no write on its repo");
    let accepted = e.engine.accept_regressions(&p.id, "ada", &[], true, Some(Some("own_acme")), Some(&p.head)).await.unwrap();
    assert_eq!((accepted.regressions.as_deref(), accepted.regressions_accepted.map(|a| a.by)), (Some("required"), Some("ada".to_string())));
    // A new one at the same head isn't covered by it: accepted again or not at all.
    e.apps.errors.lock().get_mut(&format!("{}/{web}", p.id)).unwrap().push(group("later", "TimeoutError", 1));
    e.engine.read_new_errors().await;
    assert!(e.engine.store.preview(&p.id).unwrap().regressions_accepted.is_none(), "more than was accepted");
    // Fixed (gone from its output): it goes.
    e.apps.errors.lock().insert(format!("{}/{web}", p.id), vec![group("old", "a flaky upstream", 5)]);
    e.engine.read_new_errors().await;
    assert!(e.engine.store.preview(&p.id).unwrap().new_errors.is_none());
}

/// A port of a preview's service on a laptop (`roll preview forward`): bytes go both ways, port 0
/// is refused, a debugger's many connections are said once, and a preview holds a bounded number.
#[tokio::test]
async fn a_forwarded_port_carries_bytes_is_said_once_and_is_bounded() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_fw", 16));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_fw").unwrap().unwrap();
    let mut f = e.engine.forward(&p.id, None, 9229, "ana").await.unwrap();
    f.io.write_all(b"ping").await.unwrap();
    let mut got = [0u8; 4];
    f.io.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"ping");
    let again = e.engine.forward(&p.id, None, 9229, "ana").await.unwrap();
    let said = e.engine.store.activity(&p.id).unwrap().into_iter().filter(|a| a.message == format!("ana forwarded port 9229 of {}", f.service)).count();
    assert_eq!(said, 1, "a debugger's connections are said once");
    assert!(matches!(e.engine.forward(&p.id, None, 0, "ana").await, Err(roll_previews::Error::BadRequest(_))));
    let mut open = vec![f, again];
    while open.len() < roll_previews::engine::shell::MAX_FORWARDS {
        open.push(e.engine.forward(&p.id, None, 5432, "ana").await.unwrap());
    }
    assert!(matches!(e.engine.forward(&p.id, None, 5432, "ana").await, Err(roll_previews::Error::Conflict(_))), "a preview holds a bounded number");
    open.pop();
    e.engine.forward(&p.id, None, 5432, "ana").await.expect("a closed one gives its place back");
}

/// Files in and out of a preview's service (`roll preview cp`): what's written comes back, nothing
/// at a path is a 404 before any of the answer, a relative path is refused, and the activity says
/// who copied what.
#[tokio::test]
async fn files_go_in_and_out_of_a_previews_service_and_are_said() {
    use tokio::io::AsyncReadExt;
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_cp", 15));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_cp").unwrap().unwrap();
    let web = e.engine.store.services(&p.id).unwrap().into_iter().next().unwrap().name;
    assert_eq!(e.engine.write_files(&p.id, None, "/tmp", bytes::Bytes::from_static(b"a tar"), "ana").await.unwrap(), web);
    let (svc, mut got) = e.engine.read_files(&p.id, Some(&web), "/tmp", "ana").await.unwrap();
    let mut read = Vec::new();
    got.read_to_end(&mut read).await.unwrap();
    assert_eq!((svc.as_str(), read.as_slice()), (web.as_str(), &b"a tar"[..]));
    assert!(matches!(e.engine.read_files(&p.id, None, "/nothing/here", "ana").await, Err(roll_previews::Error::NotFound(_))));
    assert!(matches!(e.engine.write_files(&p.id, None, "tmp", bytes::Bytes::new(), "ana").await, Err(roll_previews::Error::BadRequest(_))));
    assert!(matches!(e.engine.read_files(&p.id, Some("nope"), "/tmp", "ana").await, Err(roll_previews::Error::NotFound(_))));
    let said: Vec<String> = e.engine.store.activity(&p.id).unwrap().into_iter().map(|a| a.message).collect();
    assert!(said.contains(&format!("ana copied files into {web} at /tmp")) && said.contains(&format!("ana copied /tmp out of {web}")), "{said:?}");
}

/// A preview's output is read by anyone who can read the preview, and its code (a fork's too) can
/// print what it was given: its services' logs, its jobs' output and what its page said in a
/// screenshot run (the pull request's comment) show a secret's value as `***` (as checks'
/// summaries do; a branch's credentials too, `masks.rs`).
#[tokio::test]
async fn a_previews_logs_and_job_output_hide_its_secrets() {
    let e = env(json!({ "enabled": true, "seed": { "image": "node:22", "run": "node seed.js" }, "github": { "screenshot": true } }));
    e.secrets.values.lock().push(("STRIPE_KEY".into(), "sk_test_51Hx9".into()));
    let mut png = Vec::new();
    image::RgbaImage::new(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    *e.apps.shot.lock() = Some(png);
    *e.apps.page_says.lock() = Some("Stripe rejected sk_test_51Hx9".into());
    e.apps.seed_exit.store(3, Ordering::SeqCst);
    e.code.open(change("chg_m", 9));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_m").unwrap().unwrap();
    let error = p.error.clone().unwrap_or_default();
    assert!(error.contains("seed output, STRIPE_KEY=***") && !error.contains("sk_test_51Hx9"), "{error}");
    e.apps.seed_exit.store(0, Ordering::SeqCst);
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    *e.apps.log.lock() = vec!["listening on :3000".into(), "charging with sk_test_51Hx9".into()];
    let (_, lines) = e.engine.logs(&p.id, None, 10).await.unwrap();
    assert_eq!(lines, ["listening on :3000", "charging with ***"]);
    // Followed too (the dashboard's live logs, `roll preview logs -f`).
    let (_, mut rx) = e.engine.follow(&p.id, None, 10).await.unwrap();
    let mut followed = Vec::new();
    while followed.len() < 2 {
        followed.push(tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.unwrap().unwrap());
    }
    assert_eq!(followed, [(false, "listening on :3000".to_string()), (false, "charging with ***".to_string())], "a tail doesn't say which stream");
    // Where the backend follows (Docker), each line says whether it came on stderr, masked too.
    *e.apps.followed.lock() = Some(vec![(false, "listening on :3000".into()), (true, "charging with sk_test_51Hx9".into())]);
    let (_, mut rx) = e.engine.follow(&p.id, None, 10).await.unwrap();
    let mut tagged = Vec::new();
    while tagged.len() < 2 {
        tagged.push(tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.unwrap().unwrap());
    }
    assert_eq!(tagged, [(false, "listening on :3000".to_string()), (true, "charging with ***".to_string())]);
    let mut said = None;
    for _ in 0..400 {
        said = e.engine.store.page_health(&p.id).unwrap().and_then(|h| h.first_error);
        if said.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(said.as_deref(), Some("Stripe rejected ***"));
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
async fn page_states_and_other_browsers_are_taken_and_compared_like_pages() {
    let e = env(json!({ "enabled": true, "github": { "screenshot": true, "states": [{ "name": "cart", "page": "/", "steps": [{ "click": "#open" }] }], "browsers": ["firefox"] } }));
    let mut png = Vec::new();
    image::RgbaImage::new(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    *e.apps.shot.lock() = Some(png);
    // And one from the repo's own roll.toml, versioned with the selectors it names.
    *e.code.repo_states.lock() = vec![roll_previews::settings::StateRule { name: "menu".into(), page: "/".into(), steps: vec![roll_previews::settings::Step::Press("Tab".into())] }];
    e.code.open(change("chg_st", 6));
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    let mut state = None;
    for _ in 0..3000 {
        state = e.code.last_report("chg_st").pages.into_iter().find(|p| p.path == "state:cart" && p.base_percent.is_some());
        if state.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(state.map(|p| p.base_percent), Some(Some(0.0)), "taken, kept and compared with main's same state");
    assert!(e.apps.states.lock().iter().any(|n| n == "cart"), "the screenshot was asked to reach it");
    assert!(e.apps.states.lock().iter().any(|n| n == "menu"), "and the repo's own state, from its roll.toml");
    // Another browser's first page the same way.
    let firefox = e.code.last_report("chg_st").pages.into_iter().find(|p| p.path == "browser:firefox").map(|p| p.base_percent);
    assert_eq!(firefox, Some(Some(0.0)), "Firefox's first page, compared with main's in Firefox");
    // A push whose browsers run fails keeps no older Firefox picture to be reviewed as its own.
    e.apps.no_browsers.store(true, std::sync::atomic::Ordering::SeqCst);
    e.code.edit("chg_st", |c| c.head = "cs_6b".into());
    let mut gone = false;
    for _ in 0..3000 {
        e.engine.pass().await.unwrap();
        let pages = e.code.last_report("chg_st").pages;
        if pages.iter().any(|p| p.path == "state:cart") && !pages.iter().any(|p| p.path == "browser:firefox") {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(gone, "{:?}", e.code.last_report("chg_st").pages.iter().map(|p| &p.path).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_roll_apps_service_s_screenshots_come_from_its_own_config() {
    let e = env(json!({ "enabled": true, "github": { "screenshot": true } }));
    let mut png = Vec::new();
    image::RgbaImage::new(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    *e.apps.shot.lock() = Some(png);
    *e.apps.catalog.lock() = Some(vec![roll_previews::ports::CatalogService { name: "web".into(), repo: Some("acme/web".into()), root: "apps/web".into(), memory_mb: 512 }]);
    // Its [app.previews.screenshots] (roll apps describes it), not a [preview] the repo also has.
    *e.code.app_screenshots.lock() = Some(code_preview::config::Screenshots { pages: vec![], hide: vec![], states: vec![roll_previews::settings::StateRule { name: "apps-cart".into(), page: "/".into(), steps: vec![roll_previews::settings::Step::Press("Tab".into())] }] });
    *e.code.repo_states.lock() = vec![roll_previews::settings::StateRule { name: "preview-only".into(), page: "/".into(), steps: vec![roll_previews::settings::Step::Wait(1)] }];
    e.code.open(change("chg_ap", 7));
    for _ in 0..3000 {
        e.engine.pass().await.unwrap();
        if e.apps.states.lock().iter().any(|n| n == "apps-cart") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let states = e.apps.states.lock().clone();
    assert!(states.iter().any(|n| n == "apps-cart") && !states.iter().any(|n| n == "preview-only"), "{states:?}");
    assert!(e.code.app_roots.lock().iter().all(|r| r == "apps/web"), "read at the service's root");
}

#[tokio::test]
async fn the_teardown_runs_once_at_a_head_the_preview_ran_before_its_data_goes() {
    let e = env(json!({ "enabled": true, "teardown": { "image": "node:22", "run": "node scripts/unregister.js" } }));
    // A fork, approved at its first head; a push nobody approved after it.
    e.code.open(ChangeInfo { trusted: false, ..change("chg_t", 4) });
    e.engine.pass().await.unwrap();
    e.code.edit("chg_t", |c| c.approved = true);
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_t").unwrap().unwrap();
    assert_eq!(p.state, State::Ready);
    e.code.edit("chg_t", |c| {
        c.head = "cs_4b".into();
        c.approved = false;
    });
    e.engine.pass().await.unwrap();
    // It goes: the teardown fails, and so does the first removal, which is tried again.
    e.apps.seed_exit.store(2, Ordering::SeqCst);
    e.apps.fail_remove.store(1, Ordering::SeqCst);
    e.engine.delete(&p.id).unwrap();
    for _ in 0..4 {
        e.engine.store.update(&p.id, |r| r.next_at = 0).ok();
        e.engine.pass().await.unwrap();
    }
    assert!(e.engine.store.row(&p.id).is_err(), "a failed teardown doesn't keep the preview");
    let runs: Vec<_> = e.apps.runs.lock().iter().map(|(run, rev, _)| (run.clone(), rev.clone())).collect();
    assert_eq!(runs, [("node scripts/unregister.js".to_string(), "cs_4a".to_string())], "once, at the approved head, never the unapproved push");
    assert!(e.apps.seeds.lock().last().unwrap().starts_with("fake://prj_db~pv-"), "before the branch went: {:?}", e.apps.seeds.lock());
    let told = e.code.teardowns.lock().clone();
    assert!(told.len() == 1 && told[0].0 == p.id && told[0].1.contains("exit code 2"), "the change is told how it failed: {told:?}");
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
    assert_eq!(v.release.as_ref().map(|x| (x.head.as_str(), x.before_ms)), Some(("cs_3a", None)), "how long it took is kept");
    // Before the services at that head; not again at the same head; again at a new one.
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    // Another change's migration lands first: production gains a table the preview's branch
    // doesn't have. Not this change's doing, so neither a change nor a risk ("drops table").
    *e.pg.schema.lock() = Some((vec!["table public.users".into(), "table public.orders".into()], vec!["table public.users".into(), "column public.users.nick text".into()]));
    e.code.edit("chg_r", |c| c.head = "cs_3b".into());
    e.engine.pass().await.unwrap();
    let v = e.engine.store.view(e.engine.store.row(&p.id).unwrap()).unwrap();
    assert_eq!(v.schema_changes.as_deref(), Some(&["+ column public.users.nick text".to_string()][..]));
    assert!(v.schema_risks.is_empty(), "{:?}", v.schema_risks);
    let runs = e.apps.runs.lock().clone();
    let heads: Vec<_> = runs.iter().map(|(_, rev, deployed)| (rev.as_str(), *deployed)).collect();
    assert_eq!(heads, [("cs_3a", 0), ("cs_3a", 0), ("cs_3b", 0)], "{runs:?}");
    let v = e.engine.store.view(e.engine.store.row(&p.id).unwrap()).unwrap();
    assert!(v.release.is_some_and(|x| x.head == "cs_3b" && x.before_ms.is_some()), "a new head's run, with the one before's time");
}

#[tokio::test]
async fn a_search_branch_masked_again_in_place_keeps_the_schema_it_was_made_with() {
    let e = env_with_search(json!({ "enabled": true, "release": { "image": "node:22", "run": "npm run migrate" }, "masking": { "search:srch_logs": { "logs.ip": "redact" } } }));
    *e.search.schema.lock() = Some((vec!["index logs".into()], vec!["index logs".into(), "field logs.price float".into()]));
    e.code.open(change("chg_sm", 8));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_sm").unwrap().unwrap();
    // (Headed: the project's Postgres has a schema too.)
    let shown = |e: &Env| e.engine.store.view(e.engine.store.row(&p.id).unwrap()).unwrap().schema_changes;
    assert_eq!(shown(&e).as_deref(), Some(&["# search:srch_logs".to_string(), "+ field logs.price float".to_string()][..]));
    // The rules change: the branch is masked again in place, after the change's release wrote its
    // field there. That's no base: the next head still shows the field as the change's.
    *e.search.branch_now.lock() = Some(vec!["index logs".into(), "field logs.price float".into()]);
    let s = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "masking": { "search:srch_logs": { "logs.ip": "email" } } })).unwrap();
    e.engine.store.put_settings(&e.project, &s).unwrap();
    e.code.edit("chg_sm", |c| c.head = "cs_8b".into());
    e.engine.pass().await.unwrap();
    assert!(e.search.masks.lock().len() >= 2, "masked again: {:?}", e.search.masks.lock());
    assert_eq!(shown(&e).as_deref(), Some(&["# search:srch_logs".to_string(), "+ field logs.price float".to_string()][..]));
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
    // Until they have, `connect` hands out no connection to it: the branch has the new column's
    // real values.
    let handed = e.engine.connections(&p.id).await.unwrap();
    assert!(!handed.iter().any(|c| c.kind == Kind::Postgres), "a branch masked with older rules isn't handed out");
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap();
    let made = e.pg.made.load(Ordering::SeqCst);
    e.engine.pass().await.unwrap();
    assert_eq!(e.pg.masks.lock().len(), 2, "the changed rules were applied");
    let handed = e.engine.connections(&p.id).await.unwrap();
    assert!(handed.iter().any(|c| c.kind == Kind::Postgres), "masked with today's rules, it is");
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

/// The rules dialog's suggestions: an unmasked column that looks like people's data is offered
/// with a rule, a masked one isn't, nor a resource the caller's grant can't read; a parent that
/// sleeps isn't woken but named.
#[tokio::test]
async fn mask_suggestions_name_unmasked_peoples_data_of_branched_resources() {
    use roll_previews::mask::Column;
    let e = env(json!({ "enabled": true, "masking": { "postgres:prj_db": { "public.users.name": "redact" } } }));
    for r in ["prj_hidden", "prj_asleep"] {
        e.ws.place(roll_workspace::Scope::All, &e.project, roll_workspace::Kind::Postgres, r, 0.0, 0.0).unwrap();
    }
    let col = |path: &str| Column { path: path.into(), r#type: Some("text".into()), ..Column::default() };
    e.pg.columns.lock().insert("prj_db".into(), Ok(vec![col("public.users.email"), col("public.users.name"), col("public.users.created_at"), col("public.products.name")]));
    e.pg.columns.lock().insert("prj_hidden".into(), Ok(vec![col("public.users.email")]));
    e.pg.columns.lock().insert("prj_asleep".into(), Err(roll_previews::ports::Asleep));
    let got = e.engine.mask_suggestions(&e.project, |_, r| r != "prj_hidden").await.unwrap();
    assert_eq!(
        got,
        json!({
            "suggestions": [{ "resource": "postgres:prj_db", "path": "public.users.email", "rule": "email", "why": "an email address" }],
            "unread": [{ "resource": "postgres:prj_asleep", "why": "asleep: open it to read its columns" }],
        })
    );
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
    // A push whose service is still starting when it's asked: left for the next poll, it goes on
    // as soon as the service is up (the live run's preview spent 13 of 32 s waiting so).
    e.apps.starting.store(1, Ordering::SeqCst);
    e.code.edit("chg_hc", |c| c.head = "cs_5b".into());
    e.engine.pass().await.unwrap();
    let now = e.engine.store.now_ms().unwrap();
    let waiting = e.engine.store.row(&p.id).unwrap();
    assert!(waiting.state == State::Updating && waiting.next_at > now, "{waiting:?}");
    e.engine.service_up(&p.id);
    assert!(e.engine.store.row(&p.id).unwrap().next_at <= e.engine.store.now_ms().unwrap(), "due at once");
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
    // Its link says it's gone, with the way to the latest (not a 404 that reads as broken).
    let label = |url: &str| url.split("://").nth(1).unwrap().split('.').next().unwrap().to_string();
    let b = view.earlier.iter().find(|x| x.revision == "cs_8b").unwrap();
    let a = label(&b.url).replacen("-8b-", "-8a-", 1);
    let web = view.services.iter().find(|x| x.name == "web").map(|x| x.url.clone());
    let gone = roll_previews::store::GonePush { preview: p.id.clone(), service: "web".into(), revision: "cs_8a".into(), current_url: web };
    assert_eq!(e.engine.store.gone_push(&a).unwrap(), Some(gone.clone()), "that service's latest push");
    // Under the project's own preview domain too, and only that project's.
    let none = |_: &str| String::new();
    e.engine.store.set_project_domain(&e.project, Some(("preview.acme.dev", true)), &none, &none).unwrap();
    e.engine.store.set_project_domain("prj_other", Some(("preview.other.dev", false)), &none, &none).unwrap();
    assert_eq!(e.engine.store.gone_domain_push(&a, "preview.acme.dev").unwrap(), Some(gone));
    assert_eq!(e.engine.store.gone_domain_push(&a, "preview.other.dev").unwrap(), None);
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

/// On roll apps, an earlier push is that build started again on its own copy of the data (a
/// no-traffic rollback with the copy's variables), behind the preview's link; kept only when the
/// running build is the head it ran.
#[tokio::test]
async fn a_roll_apps_services_earlier_push_is_its_builds_own_url() {
    let e = env(json!({ "enabled": true }));
    *e.apps.catalog.lock() = Some(vec![roll_previews::ports::CatalogService { name: "web".into(), repo: Some("acme/web".into()), root: String::new(), memory_mb: 512 }]);
    e.code.git.lock().extend([("cs_9a".to_string(), "aaa".to_string()), ("cs_9b".to_string(), "bbb".to_string())]);
    e.code.open(change("chg_ae", 9));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_ae").unwrap().unwrap();
    e.apps.live.lock().insert("web".into(), ("aaa".into(), "https://0123456789ab--pv-x.apps.test".into()));
    e.code.edit("chg_ae", |c| c.head = "cs_9b".into());
    e.engine.pass().await.unwrap();
    let view = e.engine.store.preview(&p.id).unwrap();
    let token = e.engine.store.access(&p.id).unwrap().unwrap().link_token().unwrap().to_string();
    assert_eq!(view.earlier.len(), 1, "{:?}", view.earlier);
    let x = &view.earlier[0];
    // The same build again, taking no traffic, on its own copy of the data.
    let views = e.apps.views.lock().clone();
    assert!(views.len() == 1 && views[0].1 == "dep_aaa" && views[0].2.iter().any(|(k, v)| k == "DATABASE_URL" && v.contains("-9a")), "{views:?}");
    assert_eq!((x.service.as_str(), x.revision.as_str(), x.url.as_str()), ("web", "cs_9a", "https://view-dep_aaa--pv-x.apps.test"));
    assert_eq!(x.share_url.as_deref(), Some(format!("https://view-dep_aaa--pv-x.apps.test/?roll_access={token}").as_str()));
    assert!(x.own_data.contains(&roll_previews::Kind::Postgres));
    // A running build that isn't the head it ran (the new one live already) isn't kept as it.
    e.apps.live.lock().insert("web".into(), ("ccc".into(), "https://fedcba987654--pv-x.apps.test".into()));
    e.code.edit("chg_ae", |c| c.head = "cs_9c".into());
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.preview(&p.id).unwrap().earlier.len(), 1, "only cs_9a's");
    assert!(!e.engine.store.pinned_data(&p.id).unwrap().iter().any(|d| d.revision == "cs_9b"), "cs_9b's copies go: nothing uses them");
}

/// An earlier push runs on its own copy of the data, taken before the next head's release, so a
/// migration that renamed a column doesn't break it; copies go with their push, and with the data
/// they copied (a reset).
#[tokio::test]
async fn an_earlier_push_runs_on_its_own_copy_of_the_data() {
    use roll_previews::ports::EnvSource;
    let e = env(json!({ "enabled": true, "release": { "image": "node:22", "run": "npm run migrate" } }));
    e.code.open(change("chg_od", 7));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_od").unwrap().unwrap();
    let var = |env: &[(String, String)], k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap();
    let db = |env: &[(String, String)]| var(env, "DATABASE_URL");
    e.code.edit("chg_od", |c| c.head = "cs_7b".into());
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    let (_, live) = e.engine.env(&p.id, "web").await.unwrap();
    let (_, earlier) = e.engine.env(&p.id, "web@cs_7a").await.unwrap();
    // Taken in a step of their own, which the timeline shows.
    let steps: Vec<String> = e.engine.store.preview(&p.id).unwrap().timeline.map(|t| t.steps.into_iter().map(|s| s.step).collect()).unwrap_or_default();
    assert!(steps.windows(2).any(|w| w[0] == "data" && w[1] == "copies"), "{steps:?}");
    assert!(db(&earlier).contains("-7a") && db(&earlier) != db(&live), "{} / {}", db(&earlier), db(&live));
    assert!(var(&earlier, "REDIS_URL").contains("-7a") && var(&earlier, "REDIS_URL") != var(&live, "REDIS_URL"), "Redis too");
    // And it says so, wherever its link is shown.
    let shown = e.engine.store.preview(&p.id).unwrap().earlier;
    assert_eq!(shown.iter().find(|x| x.revision == "cs_7a").map(|x| x.own_data.clone()), Some(vec![roll_previews::Kind::Postgres, roll_previews::Kind::Redis]));
    // Kept with its push: the fourth push since evicts cs_7a, and its copy goes.
    for head in ["cs_7c", "cs_7d", "cs_7e"] {
        e.code.edit("chg_od", |c| c.head = head.into());
        e.engine.pass().await.unwrap();
        e.engine.pass().await.unwrap();
    }
    let copies = |e: &Env| e.engine.store.pinned_data(&p.id).unwrap().into_iter().map(|d| d.revision).collect::<Vec<_>>();
    assert_eq!(copies(&e), ["cs_7b", "cs_7b", "cs_7c", "cs_7c", "cs_7d", "cs_7d"], "Postgres and Redis");
    assert!(!e.pg.branches.lock().iter().chain(e.redis.branches.lock().iter()).any(|b| b.ends_with("-7a")), "{:?}", e.pg.branches.lock());
    // An evicted copy that can't be deleted (its product down) doesn't hold the push up: it stays
    // recorded, deleted later.
    e.pg.fail_snapshot_deletes.store(1, Ordering::SeqCst);
    e.code.edit("chg_od", |c| c.head = "cs_7e2".into());
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    let v = e.engine.store.preview(&p.id).unwrap();
    assert!(v.state == State::Ready && v.head == "cs_7e2", "{:?} {}", v.state, v.head);
    assert!(copies(&e).contains(&"cs_7b".to_string()), "cs_7b's Postgres copy stays recorded: {:?}", copies(&e));
    // A reset replaces the data: the copies of the old go with it.
    e.engine.reset(&p.id, Vec::new()).unwrap();
    e.engine.pass().await.unwrap();
    assert!(copies(&e).is_empty());
    // A copy that fails doesn't hold the push up: said, and that push shares the preview's data
    // for that product.
    e.pg.fail_snapshots.store(1, Ordering::SeqCst);
    e.code.edit("chg_od", |c| c.head = "cs_7f".into());
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    let v = e.engine.store.preview(&p.id).unwrap();
    assert!(v.state == State::Ready && v.head == "cs_7f", "{:?} {}", v.state, v.head);
    assert_eq!(v.earlier.iter().find(|x| x.revision == "cs_7e2").map(|x| x.own_data.clone()), Some(vec![roll_previews::Kind::Redis]));
    assert!(e.engine.store.activity(&p.id).unwrap().iter().any(|a| a.message.starts_with("couldn't keep postgres:prj_db's data")));
}

/// A preview whose egress goes through its proxy ("Roll only") gets the keys bound to hosts as
/// placeholders, made by whoever last saved the project's rules (roll secrets checks them); with
/// no one recorded (the operator saved them) or open egress, it resolves as before.
#[tokio::test]
async fn a_roll_only_preview_takes_placeholders_made_by_who_saved_its_rules() {
    use roll_previews::ports::EnvSource;
    let e = env(json!({ "enabled": true, "egress": "roll", "egress_hosts": ["api.stripe.com"] }));
    e.code.open(change("chg_ph", 6));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_ph").unwrap().unwrap();
    let asked = || e.secrets.placeholders.lock().len();
    // Saved by the operator: no one makes them.
    let _ = e.engine.env(&p.id, "web").await;
    assert_eq!(asked(), 0);
    e.engine.store.set_rules_maker(&e.project, Some("usr_b")).unwrap();
    // One key bound to a host its egress allows and one it doesn't: the second is said.
    *e.secrets.placeholder_hosts.lock() = vec![("STRIPE_KEY".into(), vec!["api.stripe.com".into(), "api.other.example".into()])];
    let _ = e.engine.env(&p.id, "web").await;
    assert_eq!(e.secrets.placeholders.lock().last().cloned(), Some((roll_previews::ports::intercept_consumer(&p.id), "usr_b".to_string())));
    let said: Vec<String> = e.engine.store.activity(&p.id).unwrap().into_iter().map(|a| a.message).collect();
    assert!(said.iter().any(|m| m.contains("its egress doesn't allow") && m.contains("STRIPE_KEY → api.other.example") && !m.contains("api.stripe.com")), "{said:?}");
    // On its record: the key and its hosts, and what its egress proxy did (never a value).
    e.engine.store.update_intercepted(&p.id, |x| x.swapped += 2).unwrap();
    let held = e.engine.store.preview(&p.id).unwrap().intercepted.expect("its keys");
    assert_eq!((held.keys[0].key.as_str(), held.keys[0].hosts.len(), held.swapped), ("STRIPE_KEY", 2, 2));
    // roll apps' proxies' counts for its services there, beside its own.
    let apps = roll_previews::AppsIntercepted { requests: 3, swapped: 1, refused: vec!["api.stripe.com: a placeholder in the query".into()] };
    e.engine.store.set_runtime_fact(&p.id, "intercepted_apps", Some(&apps)).unwrap();
    assert_eq!(e.engine.store.preview(&p.id).unwrap().intercepted.and_then(|x| x.on_apps), Some(apps));
    // Its CA changes (a key's bound hosts did): its services restart, trusting the new one.
    struct Ca(parking_lot::Mutex<String>);
    struct Nothing;
    impl roll_previews::intercept::Interception for Nothing {
        fn live(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        fn covers(&self, _: &str) -> bool {
            false
        }
        fn leaf(&self, _: &str) -> anyhow::Result<roll_previews::intercept::Leaf> {
            anyhow::bail!("none")
        }
        fn filter(&self, _: roll_previews::intercept::Outgoing<'_>) -> Result<roll_previews::intercept::Filtered, String> {
            Ok(Default::default())
        }
    }
    impl roll_previews::ports::Intercepts for Ca {
        fn of(&self, _: &str) -> anyhow::Result<Option<roll_previews::ports::Intercepted>> {
            Ok(Some(roll_previews::ports::Intercepted { id: self.0.lock().clone(), hosts: vec!["api.stripe.com".into()], ca_pem: String::new(), interception: Arc::new(Nothing) }))
        }
    }
    let ca = Arc::new(Ca(parking_lot::Mutex::new("ica_1".into())));
    let _ = e.engine.intercepts.set(ca.clone());
    e.engine.store.update(&p.id, |row| row.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    let before = e.engine.store.service_config(&p.id, "web").unwrap();
    *ca.0.lock() = "ica_2".into();
    e.engine.store.update(&p.id, |row| row.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    let after = e.engine.store.service_config(&p.id, "web").unwrap();
    assert!(before != after && after.as_deref().is_some_and(|c| c.contains("iica_2")), "{before:?} {after:?}");
    // Open egress: nothing would swap them, so none.
    let n = asked();
    let mut st = e.engine.store.settings(&e.project).unwrap();
    st.egress = roll_previews::settings::EgressRule::Open;
    st.egress_forks = roll_previews::settings::EgressRule::Open;
    st.egress_hosts.clear();
    e.engine.store.put_settings(&e.project, &st).unwrap();
    let _ = e.engine.env(&p.id, "web").await;
    assert_eq!(asked(), n);
}

/// A push that touches none of what a service watches (`roll.toml` `watch`) keeps the build it
/// runs, and it still starts at the new head; one that touches its paths rebuilds it.
#[tokio::test]
async fn a_push_outside_a_services_watch_paths_keeps_its_build() {
    use roll_previews::ports::EnvSource;
    let e = env(json!({ "enabled": true }));
    *e.code.watch.lock() = vec!["src/**".to_string()];
    e.code.open(change("chg_wt", 7));
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_wt").unwrap().unwrap();
    let running = || e.engine.store.services(&p.id).unwrap().into_iter().find(|x| x.name == "web").and_then(|x| x.revision);
    assert_eq!(running().as_deref(), Some("cs_7a"));
    // Docs only: the service keeps cs_7a, the preview is at the new head, and it starts.
    e.code.between.lock().insert("cs_7a..cs_7b".into(), vec!["docs/guide.md".into()]);
    e.code.edit("chg_wt", |c| c.head = "cs_7b".into());
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(running().as_deref(), Some("cs_7a"), "kept");
    assert_eq!(e.engine.store.row(&p.id).unwrap().head, "cs_7b");
    assert!(e.engine.store.activity(&p.id).unwrap().iter().any(|a| a.message.starts_with("kept web's build at")));
    let (t, _) = e.engine.env(&p.id, "web").await.unwrap();
    assert_eq!(t.revision, "cs_7a");
    // Its own code: rebuilt at the head.
    e.code.between.lock().insert("cs_7a..cs_7c".into(), vec!["src/app.js".into()]);
    e.code.edit("chg_wt", |c| c.head = "cs_7c".into());
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(running().as_deref(), Some("cs_7c"));
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
    assert_eq!(call(other, format!("/v1/previews/{}/qr.png", p.id)).await.0, 404, "its link with the access token, as a QR code");
    assert_eq!(call(other, format!("/v1/previews/{}/connect", p.id)).await.0, 404, "its branches' credentials");
    assert_eq!(call(other, format!("/v1/previews/projects/{}/settings", e.project)).await.0, 404);
    // Their own project's: there.
    let mine = e.ws.create("usr_b", "mine", "", None).unwrap();
    assert_eq!(call(other, format!("/v1/previews/projects/{}/settings", mine.id)).await.0, 200);
    // A create is made in the project roll-access decided it on (`X-Roll-Project`), never in another
    // the body names.
    let req = axum::http::Request::post("/v1/previews")
        .header("x-roll-project", mine.id.as_str())
        .body(axum::body::Body::from(json!({ "project": e.project, "repo": "acme/web", "branch": "main" }).to_string()))
        .unwrap();
    assert_eq!(roll_previews::api::router(e.engine.clone()).oneshot(req).await.unwrap().status().as_u16(), 400);
    // A member (a viewer here) passes previews' own check once roll-access decided the request by
    // their role (their session's grant beside it; roll-access/tests/access.rs::
    // a_members_role_decides_what_they_do_to_a_preview: a viewer reads it, never its credentials).
    // Without a decision (past the gate), only an owner does.
    struct Addresses;
    impl roll_workspace::People for Addresses {
        fn person(&self, user: &str) -> Option<roll_workspace::Person> {
            let h = user.strip_prefix("usr_")?;
            Some(roll_workspace::Person { handle: h.into(), email: Some(format!("{h}@example.com")), admin: false })
        }
    }
    e.ws.set_people(std::sync::Arc::new(Addresses));
    let (_, url) = e.ws.invite(&roll_workspace::Actor::operator(), &e.project, "v@example.com", "viewer", None).unwrap();
    e.ws.accept("usr_v", url.split("token=").nth(1).unwrap(), None).unwrap();
    assert_eq!(call(Some(("usr_v", false)), format!("/v1/previews/{}", p.id)).await.0, 404, "no role decided");
    let mut req = axum::http::Request::get(format!("/v1/previews/{}", p.id)).body(axum::body::Body::empty()).unwrap();
    req.extensions_mut().insert(roll_auth::SignedIn { user: "usr_v".into(), handle: "v".into(), admin: false, code_owner: None, session: "ses_v".into() });
    req.extensions_mut().insert(roll_auth::Grant { id: "ses_v".into(), name: "session".into(), kind: roll_auth::GrantKind::Session, scope: std::sync::Arc::new(roll_auth::scope::Scope(Vec::new())), user: Some("usr_v".into()), ask_otherwise: false });
    assert_eq!(roll_previews::api::router(e.engine.clone()).oneshot(req).await.unwrap().status().as_u16(), 200, "decided by their role");
    // A token (no signed-in person) and an admin see it.
    assert_eq!(call(None, format!("/v1/previews/{}", p.id)).await.0, 200);
    assert_eq!(call(Some(("usr_c", true)), format!("/v1/previews/{}", p.id)).await.0, 200);
    let (status, png) = call(None, format!("/v1/previews/{}/qr.png", p.id)).await;
    assert_eq!((status, png.contains("PNG")), (200, true), "a phone opens it by scanning");
    // Each branch's own connection, as its services get it.
    let (status, body) = call(None, format!("/v1/previews/{}/connect", p.id)).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(v["branches"].as_array().unwrap().iter().any(|b| b["kind"] == "postgres" && b["vars"]["DATABASE_URL"].as_str().is_some_and(|u| u.starts_with("fake://"))), "{body}");
}

/// A project rule naming another environment's key (`roll://ENV/KEY`) routes its values into code
/// whoever pushes a branch controls: saving one needs its writer's reveal there (roll secrets
/// decides); the environment its previews follow needs none. Each preview then gets it only while
/// its writer still may (roll secrets reads it as them at every converge).
#[tokio::test]
async fn a_rules_reference_to_another_environment_needs_its_writers_reveal_there() {
    use tower::ServiceExt;
    #[derive(Default)]
    struct StagingOnly(parking_lot::Mutex<Vec<(roll_workspace::Kind, bool)>>);
    impl roll_previews::ports::Reveal for StagingOnly {
        fn may_reveal(&self, who: &roll_previews::ports::Asker<'_>, _: &str, env: &str, folder: &str) -> bool {
            who.user.is_some_and(|u| u.user == "usr_b") && ((env == "staging" && folder == "/payments") || (env == "dev2" && folder == "/"))
        }
        fn place(&self, reference: &str) -> Result<(String, String), String> {
            let parts: Vec<&str> = reference.strip_prefix("roll://").ok_or("not one")?.split('/').collect();
            Ok((parts[0].to_string(), if parts.len() > 2 { format!("/{}", parts[1..parts.len() - 1].join("/")) } else { "/".into() }))
        }
        fn still(&self, _: &str, (kind, _): (roll_workspace::Kind, &str), untrusted: bool, _: &str, maker: &str) -> Result<(), String> {
            self.0.lock().push((kind, untrusted));
            if maker == "usr_b" { Ok(()) } else { Err(format!("{maker} may not reveal staging:/payments")) }
        }
    }
    let e = env(json!({ "enabled": true }));
    let reveal = Arc::new(StagingOnly::default());
    let _ = e.engine.reveal.set(reveal.clone());
    let mine = e.ws.create("usr_b", "mine", "", None).unwrap();
    let put = |env: serde_json::Value| {
        let router = roll_previews::api::router(e.engine.clone());
        let mut req = axum::http::Request::put(format!("/v1/previews/projects/{}/settings", mine.id)).header("content-type", "application/json").body(axum::body::Body::from(json!({ "env": env }).to_string())).unwrap();
        req.extensions_mut().insert(roll_auth::SignedIn { user: "usr_b".into(), handle: "b".into(), admin: false, code_owner: None, session: "ses_1".into() });
        async move { router.oneshot(req).await.unwrap().status().as_u16() }
    };
    assert_eq!(put(json!({ "K": "roll://qa/KEY" })).await, 403, "no reveal on qa");
    assert_eq!(put(json!({ "K": "roll://staging/KEY" })).await, 403, "reveal on staging's /payments only");
    assert_eq!(put(json!({ "K": " roll://qa/KEY" })).await, 400, "a look-alike isn't kept as text either");
    assert_eq!(put(json!({ "K": "roll://staging/payments/KEY" })).await, 200);
    let base = e.engine.store.settings(&mine.id).unwrap().secrets.base;
    assert_eq!(put(json!({ "K": "roll://staging/payments/KEY", "B": format!("roll://{base}/OTHER") })).await, 200, "the environment it follows needs nothing more");
    // Following another environment is the same reach: it needs reveal there too.
    let router = roll_previews::api::router(e.engine.clone());
    let mut req = axum::http::Request::put(format!("/v1/previews/projects/{}/settings", mine.id)).header("content-type", "application/json").body(axum::body::Body::from(json!({ "secrets": { "base": "qa" } }).to_string())).unwrap();
    req.extensions_mut().insert(roll_auth::SignedIn { user: "usr_b".into(), handle: "b".into(), admin: false, code_owner: None, session: "ses_1".into() });
    assert_eq!(router.oneshot(req).await.unwrap().status().as_u16(), 403, "no reveal on qa");
    // A reference to the base, free while it's the base, is checked as new once the base moves.
    let router = roll_previews::api::router(e.engine.clone());
    let mut req = axum::http::Request::put(format!("/v1/previews/projects/{}/settings", mine.id)).header("content-type", "application/json").body(axum::body::Body::from(json!({ "secrets": { "base": "dev2" } }).to_string())).unwrap();
    req.extensions_mut().insert(roll_auth::SignedIn { user: "usr_b".into(), handle: "b".into(), admin: false, code_owner: None, session: "ses_1".into() });
    assert_eq!(router.oneshot(req).await.unwrap().status().as_u16(), 403, "B names the old base, which usr_b can't reveal");

    // A preview gets a reference only while who wrote it still may: one by someone who can't now
    // is left out (said in its activity), the base's always goes.
    let mut st = e.engine.store.settings(&e.project).unwrap();
    let base = st.secrets.base.clone();
    st.env = [("K".to_string(), "roll://staging/payments/KEY".to_string()), ("B".to_string(), format!("roll://{base}/OTHER"))].into();
    e.engine.store.put_settings(&e.project, &st).unwrap();
    e.engine.store.set_env_refs(&e.project, &roll_previews::settings::references(&st), "usr_gone").unwrap();
    e.code.open(change("chg_refs", 3));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_refs").unwrap().unwrap();
    assert_eq!(e.secrets.refs.lock().clone(), vec![("B".to_string(), format!("roll://{base}/OTHER"))]);
    let said: Vec<String> = e.engine.store.activity(&p.id).unwrap().into_iter().map(|a| a.message).collect();
    assert!(said.iter().any(|m| m.contains("K: usr_gone may not reveal staging:/payments")), "{said:?}");
    // Written again by someone who may: it reaches the preview.
    e.engine.store.set_env_refs(&e.project, &[], "x").unwrap();
    e.engine.store.set_env_refs(&e.project, &roll_previews::settings::references(&st), "usr_b").unwrap();
    e.engine.store.update(&p.id, |row| row.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    assert!(e.secrets.refs.lock().iter().any(|(k, _)| k == "K"));

    // A preview's own variable may be one too: its setter's reveal there, read as them.
    let signed = |user: &str| roll_auth::SignedIn { user: user.into(), handle: user.into(), admin: false, code_owner: None, session: "ses_1".into() };
    let (b, c) = (signed("usr_b"), signed("usr_c"));
    let own = |v: &str| std::collections::BTreeMap::from([("OWN".to_string(), Some(v.to_string()))]);
    let by = |u| roll_previews::ports::Asker { operator: false, user: Some(u), grant: None };
    assert!(matches!(e.engine.set_vars_as(&p.id, own("roll://staging/payments/OWN"), &by(&c)).await, Err(roll_previews::Error::Forbidden(_))), "no reveal there");
    e.engine.set_vars_as(&p.id, own("roll://staging/payments/OWN"), &by(&b)).await.unwrap();
    e.engine.pass().await.unwrap();
    assert!(e.secrets.refs.lock().iter().any(|(k, _)| k == "OWN"), "{:?}", e.secrets.refs.lock());
    // Saving the rules keeps it its setter's; a setter who can't now leaves it out.
    e.engine.store.set_env_refs(&e.project, &roll_previews::settings::references(&st), "usr_b").unwrap();
    assert!(e.engine.store.env_ref_writers(&e.project).unwrap().iter().any(|(scope, name, _, by)| *scope == format!("preview:{}", p.id) && name == "OWN" && by == "usr_b"));
    e.engine.store.set_var_ref(&e.project, &p.id, "OWN", Some(("roll://staging/payments/OWN", "usr_gone"))).unwrap();
    e.engine.store.update(&p.id, |row| row.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    assert!(!e.secrets.refs.lock().iter().any(|(k, _)| k == "OWN"));
    // A start between converges never gives a reference whose writer lost it since: it waits for
    // the converge that leaves it out.
    use roll_previews::ports::EnvSource;
    e.engine.store.set_env_refs(&e.project, &[], "x").unwrap();
    e.engine.store.set_env_refs(&e.project, &roll_previews::settings::references(&st), "usr_gone").unwrap();
    let said = |r: anyhow::Result<_>| r.err().map(|e| e.to_string()).unwrap_or_default();
    let first = said(e.engine.env(&p.id, "web").await);
    assert!(first.contains("converges without it"), "{first}");
    e.engine.pass().await.unwrap();
    assert!(!said(e.engine.env(&p.id, "web").await).contains("converges without it"), "once left out, starts go on");
    // Read as the preview's repo, untrusted unless the project trusts its changes with production.
    assert!(reveal.0.lock().iter().all(|(k, u)| *k == roll_workspace::Kind::Repo && *u), "{:?}", reveal.0.lock());
    let mut st = e.engine.store.settings(&e.project).unwrap();
    st.secrets.allow_production = true;
    e.engine.store.put_settings(&e.project, &st).unwrap();
    reveal.0.lock().clear();
    e.engine.store.update(&p.id, |row| row.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    assert!(reveal.0.lock().iter().any(|(_, u)| !*u), "a trusted change's preview is asked as trusted");
}

/// A variable set while a deploy is still going, then removed, is removed from the backend too:
/// what was sent counts, not only what went live.
#[tokio::test]
async fn a_change_made_while_a_deploy_is_still_going_is_sent_again() {
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_s", 6));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_s").unwrap().unwrap();
    let sent = || e.apps.vars.lock().get(&(p.id.clone(), "web".to_string())).map(|v| v.iter().any(|(n, _)| n == "FEATURE")).unwrap_or(false);
    let set = |v: Option<&str>| std::collections::BTreeMap::from([("FEATURE".to_string(), v.map(String::from))]);
    e.apps.still.store(1, Ordering::SeqCst);
    e.engine.set_vars(&p.id, set(Some("on"))).await.unwrap();
    e.engine.pass().await.unwrap();
    assert!(sent(), "sent, still deploying");
    e.engine.set_vars(&p.id, set(None)).await.unwrap();
    e.engine.pass().await.unwrap();
    assert!(!sent(), "back to how it was before the deploy finished: sent again");
}

/// A variable removed while the deploy that sent it is under way is removed from the backend on
/// the next converge.
#[tokio::test]
async fn a_variable_removed_during_its_deploy_is_removed_after() {
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_d2", 7));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_d2").unwrap().unwrap();
    let sent = || e.apps.vars.lock().get(&(p.id.clone(), "web".to_string())).map(|v| v.iter().any(|(n, _)| n == "FEATURE")).unwrap_or(false);
    e.engine.set_vars(&p.id, std::collections::BTreeMap::from([("FEATURE".to_string(), Some("on".to_string()))])).await.unwrap();
    let (store, id) = (e.engine.store.clone(), p.id.clone());
    *e.apps.on_deploy.lock() = Some(Box::new(move || {
        store.update_vars(&id, |v| {
            v.clear();
            Ok(())
        })
        .unwrap();
        store.update(&id, |r| r.next_at = 0).unwrap();
    }));
    e.engine.pass().await.unwrap();
    *e.apps.on_deploy.lock() = None;
    assert!(sent(), "the deploy that sent it");
    e.engine.pass().await.unwrap();
    assert!(!sent(), "removed on the next converge");
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

/// A roll apps service's preview costs what its preview environment runs with (roll apps' own
/// preview size), not the service's production size.
#[tokio::test]
async fn a_roll_apps_preview_costs_what_it_runs_with() {
    let e = env(json!({ "enabled": true }));
    *e.apps.catalog.lock() = Some(vec![roll_previews::ports::CatalogService { name: "web".into(), repo: Some("acme/web".into()), root: String::new(), memory_mb: 2048 }]);
    *e.apps.memory.lock() = Some(256);
    e.code.open(change("chg_m", 9));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_m").unwrap().unwrap();
    let web = e.engine.store.services(&p.id).unwrap().into_iter().find(|s| s.name == "web").unwrap();
    assert_eq!(web.memory_mb, 256);
    // Its feedback button rides along (link access, a change's), and turning it off redeploys.
    assert_eq!(e.apps.feedback.lock().get(&(p.id.clone(), "web".to_string())), Some(&true));
    let s = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "feedback": false })).unwrap();
    e.engine.store.put_settings(&e.project, &s).unwrap();
    e.engine.store.update(&p.id, |r| r.next_at = 0).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.apps.feedback.lock().get(&(p.id.clone(), "web".to_string())), Some(&false));
}

/// The notes on a page, by the preview's environment name (how roll apps' proxy asks for them):
/// that page's only, nothing for an environment that isn't a preview's.
/// Only the pages a push can change are taken again (Chromatic's TurboSnap): another service that
/// runs what it ran at the push before keeps its pages without a browser run; once its data is
/// reset they're taken again.
#[tokio::test]
async fn a_service_that_didnt_change_keeps_its_pages() {
    // A change in acme/api: api is the change's (taken every push), web the project's other
    // service, running acme/web's default branch whatever the change pushes.
    let e = env(json!({ "enabled": true, "github": { "screenshot": true, "pages": ["web:/"] } }));
    let mut png = Vec::new();
    image::RgbaImage::new(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    *e.apps.shot.lock() = Some(png);
    let runs = |svc: &str| e.apps.shot_services.lock().iter().filter(|s| *s == svc).count();
    async fn taken(apps: &common::FakeApps, svc: &str, n: usize) {
        for _ in 0..3000 {
            if apps.shot_services.lock().iter().filter(|s| *s == svc).count() >= n {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    let mut c = change("chg_ts", 3);
    c.repo = "acme/api".into();
    e.code.open(c);
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_ts").unwrap().unwrap();
    // The change's run takes api's page, then web's.
    taken(&e.apps, "web", 1).await;
    let web_before = runs("web");
    assert!(web_before >= 1, "web's page taken at first");
    let api_before = runs("api");
    // A push to api: web runs what it ran, so its page is kept, not taken again.
    e.code.edit("chg_ts", |c| c.head = "cs_4a".into());
    e.engine.pass().await.unwrap();
    taken(&e.apps, "api", api_before + 1).await;
    for _ in 0..3000 {
        if e.engine.store.screenshot_head(&p.id).unwrap().as_deref() == Some("cs_4a") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(runs("web"), web_before, "an unchanged service's pages are kept");
    assert!(e.engine.store.preview(&p.id).unwrap().pages.iter().any(|x| x.path == "web:/"), "and still listed");
    // Its data reset since: taken again.
    let now = e.engine.store.now_ms().unwrap();
    e.engine.store.update(&p.id, |r| r.last_reset_at = Some(now)).unwrap();
    e.code.edit("chg_ts", |c| c.head = "cs_5a".into());
    e.engine.pass().await.unwrap();
    taken(&e.apps, "web", web_before + 1).await;
    assert_eq!(runs("web"), web_before + 1, "after a reset its pages are taken again");
}

/// The first page in the project's modes (`github.modes`: dark, another language): each a page of
/// its own (`mode:<name>`), kept and served by the sheet's picture route, as a state's page is.
#[tokio::test]
async fn the_first_page_in_each_mode_is_a_page_of_its_own() {
    use tower::ServiceExt;
    let e = env(json!({ "enabled": true, "github": { "screenshot": true, "modes": [{ "name": "dark", "color_scheme": "dark" }], "states": [{ "name": "tab", "page": "/", "steps": [{ "press": "Tab" }] }] } }));
    let mut png = Vec::new();
    image::RgbaImage::new(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    *e.apps.shot.lock() = Some(png);
    e.code.open(change("chg_md", 3));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_md").unwrap().unwrap();
    let pages = |e: &Env| e.engine.store.preview(&p.id).unwrap().pages.iter().map(|x| x.path.clone()).collect::<Vec<_>>();
    for _ in 0..3000 {
        if pages(&e).contains(&"mode:dark".to_string()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(pages(&e).contains(&"mode:dark".to_string()), "{:?}", pages(&e));
    for key in ["mode:dark", "state:tab"] {
        let req = axum::http::Request::get(format!("/v1/previews/{}/screenshot?page={key}", p.id)).body(axum::body::Body::empty()).unwrap();
        let resp = roll_previews::api::router(e.engine.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200, "the sheet's picture of {key}");
    }
}

/// A Storybook another service serves (`github.storybook`): its stories are pages of their own
/// (`story:<id>`), kept with the rest of that service's pages while it runs what it ran.
#[tokio::test]
async fn a_storybooks_stories_are_pages_kept_while_its_service_doesnt_change() {
    let e = env(json!({ "enabled": true, "github": { "screenshot": true, "storybook": "web:/sb/" } }));
    let mut png = Vec::new();
    image::RgbaImage::new(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    *e.apps.shot.lock() = Some(png);
    let runs = |svc: &str| e.apps.shot_services.lock().iter().filter(|s| *s == svc).count();
    let mut c = change("chg_sb", 3);
    c.repo = "acme/api".into();
    e.code.open(c);
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_sb").unwrap().unwrap();
    let stories = |e: &Env| e.engine.store.preview(&p.id).unwrap().pages.iter().filter(|x| x.path.starts_with("story:")).map(|x| x.path.clone()).collect::<Vec<_>>();
    for _ in 0..3000 {
        if stories(&e).len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(stories(&e), ["story:button--primary", "story:button--secondary"]);
    let web_before = runs("web");
    // A push to api: web runs what it ran, so its stories are kept, not taken again.
    e.code.edit("chg_sb", |c| c.head = "cs_4b".into());
    e.engine.pass().await.unwrap();
    for _ in 0..3000 {
        if e.engine.store.screenshot_head(&p.id).unwrap().as_deref() == Some("cs_4b") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!((runs("web"), stories(&e).len()), (web_before, 2), "kept with web's other pages");
}

/// A note left with the page's Feedback button gets a picture of its element: the next screenshot
/// run crops it by its selector (only selectors roll accepts, from the preview's code), keeps it
/// with the note and lists it; a note already cropped isn't asked again, and one whose element
/// isn't on the page has none.
#[tokio::test]
async fn a_notes_element_is_cropped_by_the_next_screenshot() {
    let e = env(json!({ "enabled": true, "github": { "screenshot": true } }));
    let mut png = Vec::new();
    image::RgbaImage::new(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    *e.apps.shot.lock() = Some(png.clone());
    let note = |id: &str, selector: &str| roll_previews::notes::Note { id: id.into(), path: "/".into(), selector: selector.into(), note: "look".into(), by: "ada".into(), at: 1, replies: Vec::new(), crop: false };
    e.code.feedback.lock().insert("chg_cr".into(), vec![note("rv_h1", "body > h1"), note("rv_gone", "#missing"), note("rv_bad", "h1 { color: red }")]);
    e.code.open(change("chg_cr", 4));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_cr").unwrap().unwrap();
    let mut listed = Vec::new();
    for _ in 0..3000 {
        listed = e.engine.open_notes(&p.id).await.unwrap();
        if listed.iter().any(|n| n.crop) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let crop_of = |id: &str| listed.iter().find(|n| n.id == id).map(|n| n.crop);
    assert_eq!((crop_of("rv_h1"), crop_of("rv_gone"), crop_of("rv_bad")), (Some(true), Some(false), Some(false)));
    assert_eq!(e.engine.store.note_crop(&p.id, "rv_h1").unwrap(), Some(png));
    assert_eq!(*e.apps.crops_asked.lock(), ["rv_h1", "rv_gone"], "a selector roll refuses is never sent to the page");
    // The next head's run asks only for what has no picture yet.
    e.code.edit("chg_cr", |c| c.head = "cs_5a".into());
    e.engine.pass().await.unwrap();
    for _ in 0..3000 {
        if e.apps.crops_asked.lock().len() > 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(*e.apps.crops_asked.lock(), ["rv_h1", "rv_gone", "rv_gone"]);
}

/// A push to a branch of a Git URL service on roll apps makes that branch's preview in each
/// project whose canvas holds the service (trusted: only the repository's writers move its
/// branches); the same push again changes nothing, a new one moves it, `[skip preview]` skips
/// one, and deleting the branch deletes its preview.
#[tokio::test]
async fn a_git_url_services_branch_push_makes_its_preview() {
    let e = env(json!({ "enabled": true }));
    e.ws.place(roll_workspace::Scope::All, &e.project, roll_workspace::Kind::Service, "svc_shop", 0.0, 0.0).unwrap();
    let push = |branch: &str, sha: Option<&str>, message: &str| roll_previews::ports::GitPush {
        service: "svc_shop".into(),
        url: "https://github.com/acme/shop.git".into(),
        branch: branch.into(),
        sha: sha.map(Into::into),
        message: message.into(),
        pusher: None,
        pull: None,
    };
    let rows = || e.engine.store.rows(Some(&e.project)).unwrap().into_iter().filter(|r| r.repo == "git:svc_shop").collect::<Vec<_>>();
    // The project's services are roll apps': the Git URL one is `git:<id>`.
    *e.apps.catalog.lock() = Some(vec![roll_previews::ports::CatalogService { name: "shop".into(), repo: Some("git:svc_shop".into()), root: String::new(), memory_mb: 256 }]);
    // Someone else's project with the service on its canvas: it doesn't stand for it
    // (`Workspace::trusts`), so it gets none of its pushes.
    let theirs = e.ws.create("usr_b", "theirs", "", None).unwrap();
    e.ws.place(roll_workspace::Scope::All, &theirs.id, roll_workspace::Kind::Service, "svc_shop", 0.0, 0.0).unwrap();
    e.engine.store.put_settings(&theirs.id, &serde_json::from_value(json!({ "enabled": true })).unwrap()).unwrap();
    e.engine.git_push(&push("feat/cart", Some("a1"), "cart")).await.unwrap();
    e.engine.git_push(&push("feat/cart", Some("a1"), "cart")).await.unwrap();
    assert!(e.engine.store.rows(Some(&theirs.id)).unwrap().is_empty(), "a project that doesn't stand for the service gets no preview of it");
    let r = rows();
    assert_eq!(r.iter().map(|x| (x.branch.clone(), x.head.clone(), x.trusted)).collect::<Vec<_>>(), [(Some("feat/cart".to_string()), "a1".to_string(), true)], "made once");
    // A pass deploys the service at the pushed commit, and discovery (roll code's branches)
    // leaves it alone.
    e.engine.pass().await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.row(&r[0].id).unwrap().desired, Desired::Active, "not roll code's branch to delete");
    assert_eq!(e.apps.deployed.lock().get(&(r[0].id.clone(), "shop".to_string())).map(String::as_str), Some("a1"));
    e.engine.git_push(&push("feat/cart", Some("b2"), "more")).await.unwrap();
    assert_eq!(rows()[0].head, "b2", "a new push moves it");
    e.engine.pass().await.unwrap();
    assert_eq!(e.apps.deployed.lock().get(&(r[0].id.clone(), "shop".to_string())).map(String::as_str), Some("b2"), "and deploys it");
    e.engine.git_push(&push("feat/docs", Some("c3"), "typo [skip preview]")).await.unwrap();
    assert_eq!(rows().len(), 1, "[skip preview] skips one");
    // Its pull request (same repo) names the branch's preview and links it, still trusted.
    let pull = roll_previews::ports::GitPull { number: 3, title: "Cart\nbadge".into(), url: "https://github.com/acme/shop/pull/3".into(), fork: false, author: None };
    e.engine.git_push(&roll_previews::ports::GitPush { pull: Some(pull), ..push("feat/cart", Some("b2"), "") }).await.unwrap();
    let v = e.engine.store.view(e.engine.store.row(&r[0].id).unwrap()).unwrap();
    assert_eq!((v.title.as_str(), v.pull.as_ref().map(|p| p.number), v.trusted, rows().len()), ("Cart badge", Some(3), true, 1));
    e.engine.git_push(&push("feat/cart", None, "")).await.unwrap();
    assert_eq!(e.engine.store.row(&r[0].id).unwrap().desired, Desired::Deleted, "the branch went: so does its preview");
}

/// A fork's pull request on a Git URL service (roll apps' `GitPull`, `fork: true`): its
/// preview (`pull/<n>`, untrusted) runs nothing until a writer approves its head, only that head;
/// the next push waits again, and closing the pull request deletes it.
#[tokio::test]
async fn a_forks_pull_request_on_a_git_url_service_runs_only_its_approved_head() {
    // Forks reach roll only (the default): roll apps holds its instance and its build to it.
    let e = env(json!({ "enabled": true }));
    e.ws.place(roll_workspace::Scope::All, &e.project, roll_workspace::Kind::Service, "svc_shop", 0.0, 0.0).unwrap();
    *e.apps.catalog.lock() = Some(vec![roll_previews::ports::CatalogService { name: "shop".into(), repo: Some("git:svc_shop".into()), root: String::new(), memory_mb: 256 }]);
    let push = |sha: Option<&str>| roll_previews::ports::GitPush {
        service: "svc_shop".into(),
        url: "https://github.com/acme/shop.git".into(),
        branch: "pull/7".into(),
        sha: sha.map(Into::into),
        message: String::new(),
        pusher: None,
        pull: Some(roll_previews::ports::GitPull { number: 7, title: "Faster cart".into(), url: "https://github.com/acme/shop/pull/7".into(), fork: true, author: None }),
    };
    let deployed = |id: &str| e.apps.deployed.lock().get(&(id.to_string(), "shop".to_string())).cloned();
    e.engine.git_push(&push(Some("f1"))).await.unwrap();
    e.engine.pass().await.unwrap();
    let r = e.engine.store.rows(Some(&e.project)).unwrap().into_iter().find(|r| r.repo == "git:svc_shop").unwrap();
    assert_eq!((r.state, r.trusted, r.title.as_str(), deployed(&r.id)), (State::AwaitingApproval, false, "Faster cart", None), "nothing runs before an approval");
    assert!(matches!(e.engine.approve(&r.id, "f0", "ada"), Err(roll_previews::Error::Conflict(_))), "only the head it's at");
    e.engine.approve(&r.id, "f1", "ada").unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(deployed(&r.id).as_deref(), Some("f1"), "the approved head runs");
    e.engine.git_push(&push(Some("f2"))).await.unwrap();
    e.engine.pass().await.unwrap();
    let now = e.engine.store.row(&r.id).unwrap();
    assert_eq!((now.state, deployed(&r.id).as_deref()), (State::AwaitingApproval, Some("f1")), "a new head waits again, never deployed");
    assert!(e.apps.slept.lock().iter().any(|(p, s)| *p == r.id && s == "shop"), "and what ran sleeps meanwhile");
    // A writer's own branch named `pull/7` is another preview: the fork's close leaves it.
    let branch = roll_previews::ports::GitPush { pull: None, ..push(Some("w1")) };
    e.engine.git_push(&branch).await.unwrap();
    let writers = e.engine.store.rows(Some(&e.project)).unwrap().into_iter().find(|x| x.repo == "git:svc_shop" && x.id != r.id).unwrap();
    assert!(writers.trusted, "a writer's branch: trusted, not the fork's row");
    e.engine.git_push(&push(None)).await.unwrap();
    assert_eq!(e.engine.store.row(&r.id).unwrap().desired, Desired::Deleted, "a close deletes it");
    assert_eq!(e.engine.store.row(&writers.id).unwrap().desired, Desired::Active, "and only the fork's");
}

/// A preview's secrets environment is told what the preview branched; a link there (or in its
/// base) to one of those parents comes back followed, never the parent's value, and the
/// service gets the branch's value under the link's own name (renamed or not), or blank when the
/// branch has none for that field.
#[tokio::test]
async fn a_secrets_link_to_a_branched_parent_gives_the_branch() {
    use roll_previews::ports::EnvSource;
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_f", 9));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_f").unwrap().unwrap();
    let pg = e.engine.store.resources(&p.id).unwrap().into_iter().find(|r| r.kind == Kind::Postgres).unwrap();
    assert!(e.secrets.branched.lock().contains(&(Kind::Postgres, pg.parent.clone())), "ensure names what it branched");
    *e.secrets.followed.lock() = vec![
        ("STAGING_DB".into(), Kind::Postgres, pg.parent.clone(), "url".into()),
        ("STAGING_TOKEN".into(), Kind::Postgres, pg.parent.clone(), "http_token".into()),
    ];
    let (_, vars) = e.engine.env(&p.id, "web").await.unwrap();
    let get = |k: &str| vars.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(get("STAGING_DB"), Some(format!("fake://{}", pg.r#ref.clone().unwrap())), "the branch's, under the link's name");
    assert_eq!(get("STAGING_TOKEN").as_deref(), Some(""), "nothing for that field: blank");
    // And as the environment's own key, from the base's links (nothing resolved): what roll
    // apps reads when it resolves the environment itself.
    *e.secrets.links.lock() = vec![("STAGING_PG_URL".into(), Kind::Postgres, pg.parent.clone(), "url".into()), ("OTHER".into(), Kind::Postgres, "pg_elsewhere".into(), "url".into())];
    e.engine.refresh(&p.id).unwrap();
    e.engine.pass().await.unwrap();
    let own = e.secrets.own.lock().clone();
    assert_eq!(own.iter().find(|(k, _)| k == "STAGING_PG_URL").map(|(_, v)| v.clone()), Some(format!("fake://{}", pg.r#ref.clone().unwrap())), "the branch's, as its own key");
    assert!(!own.iter().any(|(k, _)| k == "OTHER"), "a link to a parent it didn't branch is left to roll secrets");
}

/// A Git URL service's preview says itself on its repository (Vercel's status and comment):
/// `roll/preview` on its head, posted only when it changes; a pull request's comment made once,
/// then edited in place; a fork waiting for approval is pending with no link to what hasn't run;
/// a service that isn't connected gets nothing.
#[tokio::test]
async fn a_git_url_preview_reports_its_status_and_pull_request_comment() {
    let e = env(json!({ "enabled": true }));
    e.ws.place(roll_workspace::Scope::All, &e.project, roll_workspace::Kind::Service, "svc_shop", 0.0, 0.0).unwrap();
    *e.apps.catalog.lock() = Some(vec![roll_previews::ports::CatalogService { name: "shop".into(), repo: Some("git:svc_shop".into()), root: String::new(), memory_mb: 256 }]);
    let gr = Arc::new(FakeGitReport::default());
    let _ = e.engine.git_report.set(gr.clone());
    let push = |branch: &str, sha: &str, number: u64, fork: bool| roll_previews::ports::GitPush {
        service: "svc_shop".into(),
        url: "https://github.com/acme/shop.git".into(),
        branch: branch.into(),
        sha: Some(sha.into()),
        message: String::new(),
        pusher: None,
        pull: Some(roll_previews::ports::GitPull { number, title: "Cart".into(), url: format!("https://github.com/acme/shop/pull/{number}"), fork, author: None }),
    };
    e.engine.git_push(&push("feat/cart", "a1", 3, false)).await.unwrap();
    for _ in 0..3 {
        e.engine.pass().await.unwrap();
    }
    let last = gr.statuses.lock().last().cloned().unwrap();
    assert_eq!((last.0.as_str(), last.1.as_str(), last.2.as_str()), ("svc_shop", "a1", "success"), "ready on its head");
    assert!(last.4.is_some(), "linked to the preview");
    let (n, c) = (gr.statuses.lock().len(), gr.comments.lock().len());
    e.engine.pass().await.unwrap();
    assert_eq!((gr.statuses.lock().len(), gr.comments.lock().len()), (n, c), "nothing new: nothing posted");
    let comments = gr.comments.lock().clone();
    assert_eq!(comments[0].1, None, "the first comment is made");
    assert!(comments.iter().skip(1).all(|x| x.1 == Some(77)) && comments.iter().all(|x| x.0 == 3), "then edited in place");
    // A fork's: pending, linked to roll (not to what hasn't run).
    e.engine.git_push(&push("pull/7", "f1", 7, true)).await.unwrap();
    e.engine.pass().await.unwrap();
    let fork = gr.statuses.lock().iter().rev().find(|s| s.1 == "f1").cloned().unwrap();
    assert_eq!(fork.2, "pending");
    assert!(fork.3.contains("approve") && !fork.4.as_deref().unwrap_or("").contains("shop."), "waits for a writer, no service link: {fork:?}");
    // The project turns them off (`github.status`, `.comment`): nothing, as for a roll code change.
    let off = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "github": { "status": false, "comment": false } })).unwrap();
    e.engine.store.put_settings(&e.project, &off).unwrap();
    let (n, c) = (gr.statuses.lock().len(), gr.comments.lock().len());
    e.engine.git_push(&push("feat/cart", "a3", 3, false)).await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!((gr.statuses.lock().len(), gr.comments.lock().len()), (n, c), "the project's switches hold");
    let on = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "github": { "status": true, "comment": true } })).unwrap();
    e.engine.store.put_settings(&e.project, &on).unwrap();
    // Not connected: nothing.
    gr.connected.store(false, std::sync::atomic::Ordering::SeqCst);
    let n = gr.statuses.lock().len();
    e.engine.git_push(&push("feat/cart", "a2", 3, false)).await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(gr.statuses.lock().len(), n);
    gr.connected.store(true, std::sync::atomic::Ordering::SeqCst);
    // A dependency bot's pull request (`skip_authors`) gets no preview, unless its title asks.
    let bots = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "skip_authors": ["*[bot]"] })).unwrap();
    e.engine.store.put_settings(&e.project, &bots).unwrap();
    let bump = |title: &str| roll_previews::ports::GitPush {
        pull: Some(roll_previews::ports::GitPull { number: 9, title: title.into(), url: "https://github.com/acme/shop/pull/9".into(), fork: false, author: Some("dependabot[bot]".into()) }),
        ..push("deps/lodash", "d1", 9, false)
    };
    let of_bump = || e.engine.store.rows(Some(&e.project)).unwrap().into_iter().any(|r| r.branch.as_deref() == Some("deps/lodash") && r.desired == Desired::Active);
    // The bot pushes its branch first (no pull request yet, no opener): that makes a preview…
    let branch_push = || roll_previews::ports::GitPush { pull: None, ..push("deps/lodash", "d1", 9, false) };
    e.engine.git_push(&branch_push()).await.unwrap();
    assert!(of_bump());
    // (With the pusher known, a bot's branch push makes none at all.)
    let deps = |b: &str| roll_previews::ports::GitPush { pusher: Some("dependabot[bot]".into()), ..roll_previews::ports::GitPush { pull: None, ..push(b, "e1", 10, false) } };
    e.engine.git_push(&deps("deps/react")).await.unwrap();
    assert!(!e.engine.store.rows(Some(&e.project)).unwrap().iter().any(|r| r.branch.as_deref() == Some("deps/react")), "a bot's branch push");
    // …which its pull request takes back, and the branch's next push makes none.
    e.engine.git_push(&bump("Bump lodash")).await.unwrap();
    assert!(!of_bump(), "a bot's pull request is skipped");
    e.engine.git_push(&roll_previews::ports::GitPush { sha: Some("d2".into()), ..branch_push() }).await.unwrap();
    assert!(!of_bump(), "and so are its branch's later pushes");
    // A later event that names no opener (GitLab's when someone else acted) keeps the skip.
    let unknown = roll_previews::ports::GitPush { pull: Some(roll_previews::ports::GitPull { author: None, ..bump("Bump lodash").pull.unwrap() }), sha: Some("d3".into()), ..branch_push() };
    e.engine.git_push(&unknown).await.unwrap();
    assert!(!of_bump(), "an unknown opener isn't a person");
    e.engine.git_push(&bump("Bump lodash [preview]")).await.unwrap();
    assert!(of_bump(), "unless its title asks");
}

#[tokio::test]
async fn a_pages_notes_by_its_environment_name() {
    let e = env(json!({ "enabled": true }));
    e.code.open(change("chg_n", 4));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_n").unwrap().unwrap();
    let note = |path: &str, text: &str| roll_previews::notes::Note { id: format!("rv_{text}"), path: path.into(), selector: "h1".into(), note: text.into(), by: "ada".into(), at: 1, replies: Vec::new(), crop: false };
    e.code.feedback.lock().insert("chg_n".into(), vec![note("/", "Too big"), note("/about", "Typo")]);
    let env_name = roll_previews::branch_name(&p.id);
    let got = e.engine.notes_for(&env_name, "/").await.unwrap();
    assert_eq!(got.iter().map(|n| n.note.as_str()).collect::<Vec<_>>(), ["Too big"]);
    assert!(e.engine.notes_for("production", "/").await.unwrap().is_empty());
    assert!(e.engine.notes_for("pv-nope", "/").await.unwrap().is_empty());
    // Resolved (from the page's Resolve, in roll), it folds away; only the change's notes resolve.
    assert_eq!(e.engine.open_notes(&p.id).await.unwrap().iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["rv_Too big", "rv_Typo"], "every page's, with ids");
    e.engine.resolve_note(&p.id, "rv_Too big", "bob").await.unwrap();
    assert!(e.engine.notes_for(&env_name, "/").await.unwrap().is_empty());
    assert_eq!(e.engine.open_notes(&p.id).await.unwrap().len(), 1);
    assert!(e.engine.resolve_note(&p.id, "rv_someone else's", "bob").await.is_err());
}

/// A start's dynamic credentials (roll secrets' leases) live as long as it runs: renewed near
/// their end, the previous start's revoked by the next, revoked once the service stops; one that
/// can't be renewed puts the service to sleep, so its next request starts it with a new one.
#[tokio::test]
async fn a_start_s_dynamic_credentials_live_as_long_as_it_runs() {
    use roll_previews::ports::EnvSource;
    let e = env(json!({ "enabled": true }));
    e.secrets.dynamic.store(true, Ordering::SeqCst);
    e.code.open(change("chg_d", 7));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_d").unwrap().unwrap();
    let web = (p.id.clone(), "web".to_string());
    let password = |vars: &[(String, String)]| vars.iter().find(|(k, _)| k == "DB_PASSWORD").map(|(_, v)| v.clone());
    let held = || e.engine.store.leases(Some(&p.id)).unwrap().into_iter().map(|l| l.lease).collect::<Vec<_>>();
    let (_, vars) = e.engine.env(&p.id, "web").await.unwrap();
    assert_eq!((password(&vars).as_deref(), held()), (Some("pw-1"), vec!["lease-1".to_string()]));
    e.apps.running.lock().insert(web.clone());
    e.engine.pass().await.unwrap();
    assert!(e.secrets.renewed.lock().is_empty() && e.secrets.revoked.lock().is_empty(), "far from its end: left alone");
    let soon = e.engine.store.now_ms().unwrap() + 60_000;
    e.engine.store.set_lease_expiry(&p.id, "lease-1", soon).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(*e.secrets.renewed.lock(), ["lease-1"]);
    assert!(e.engine.store.leases(Some(&p.id)).unwrap()[0].expires_at > soon, "its new end kept");
    // A new start (a redeploy, a wake): the one before's credential goes.
    let (_, vars) = e.engine.env(&p.id, "web").await.unwrap();
    assert_eq!((password(&vars).as_deref(), held(), e.secrets.revoked.lock().clone()), (Some("pw-2"), vec!["lease-2".to_string()], vec!["lease-1".to_string()]));
    // Ended (revoked in roll secrets, at its longest life): the service sleeps.
    e.secrets.ended.lock().insert("lease-2".into());
    e.engine.store.set_lease_expiry(&p.id, "lease-2", soon).unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!((e.apps.slept.lock().clone(), held()), (vec![web.clone()], Vec::<String>::new()));
    // Asleep or gone, it holds none.
    e.engine.env(&p.id, "web").await.unwrap();
    e.apps.running.lock().remove(&web);
    e.engine.pass().await.unwrap();
    assert_eq!((e.secrets.revoked.lock().last().map(String::as_str), held()), (Some("lease-3"), Vec::<String>::new()));
}

/// A preview's services send mail to roll's catcher, signed in as the preview, never with the
/// project's own mail settings or a provider's API key (its data is production's people); what
/// it caught is listed with it. `mail: "deliver"` hands the project's settings through.
#[tokio::test]
async fn a_preview_s_mail_is_caught_never_sent() {
    use roll_previews::mail::Mailbox;
    use roll_previews::ports::EnvSource;
    let e = env(json!({ "enabled": true, "env": { "SENDGRID_API_KEY": "SG.real", "SMTP_HOST": "smtp.sendgrid.net" } }));
    e.code.open(change("chg_m", 7));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_m").unwrap().unwrap();
    let (_, vars) = e.engine.env(&p.id, "web").await.unwrap();
    let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(get("SENDGRID_API_KEY"), None, "no way to send through the provider");
    assert_ne!(get("SMTP_HOST").as_deref(), Some("smtp.sendgrid.net"));
    assert_eq!(get("SMTP_USER").as_deref(), Some(p.id.as_str()));
    let pass = get("SMTP_PASSWORD").unwrap();
    assert!(e.engine.store.mail_signs_in(&p.id, &pass).unwrap() && !e.engine.store.mail_signs_in(&p.id, "guess").unwrap());
    // A deploy elsewhere (roll apps) gets the same: roll's SMTP, and the provider's key replaced
    // over what roll secrets would give it.
    let deployed = e.apps.vars.lock().get(&(p.id.clone(), "web".to_string())).cloned().unwrap_or_default();
    let dget = |k: &str| deployed.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!((dget("SMTP_USER").as_deref(), dget("SENDGRID_API_KEY").as_deref()), (Some(p.id.as_str()), Some("roll-previews-mail-caught")));
    // Through the catcher's mailbox: kept with the preview, listed.
    let mb = roll_previews::mail::Store(e.engine.store.clone());
    assert_eq!(mb.authorize(&p.id, &pass).await.as_deref(), Some(p.id.as_str()));
    mb.keep(&p.id, roll_previews::mail::Mail { from: "shop@x".into(), to: vec!["ada@corp.example".into()], raw: b"Subject: Hi\r\n\r\nbody\r\n".to_vec() }).await;
    let list = e.engine.store.mails(&p.id).unwrap();
    assert_eq!((list.len(), list[0].subject.as_deref(), list[0].to.clone()), (1, Some("Hi"), vec!["ada@corp.example".to_string()]));
    assert_eq!(e.engine.store.mail_raw(&p.id, &list[0].id).unwrap().as_deref(), Some(&b"Subject: Hi\r\n\r\nbody\r\n"[..]));
    // Bounded by bytes as well as by count: twelve 1 MiB messages keep the newest ten.
    for i in 0..12 {
        let raw = format!("Subject: big {i}\r\n\r\n{}", "x".repeat((1 << 20) - 40)).into_bytes();
        e.engine.store.keep_mail(&p.id, &roll_previews::mail::Mail { from: "a@b".into(), to: vec!["c@d".into()], raw }).unwrap();
    }
    let kept = e.engine.store.mails(&p.id).unwrap();
    assert!(kept.iter().map(|m| m.size).sum::<i64>() <= roll_previews::store::MAIL_BYTES, "{} kept", kept.len());
    assert_eq!(kept[0].subject.as_deref(), Some("big 11"), "the newest stays");
    // A project that wants real mail says so.
    let s = e.engine.store.settings(&e.project).unwrap().patched(&json!({ "mail": "deliver" })).unwrap();
    e.engine.store.put_settings(&e.project, &s).unwrap();
    let (_, vars) = e.engine.env(&p.id, "web").await.unwrap();
    assert!(vars.iter().any(|(k, v)| k == "SENDGRID_API_KEY" && v == "SG.real"));
}

/// `data_at` branches the data as the parent was then, where the product keeps history; one that
/// doesn't branches as of now and says so.
#[tokio::test]
async fn data_at_branches_from_history_where_there_is_some() {
    let e = env(json!({ "enabled": true, "data_at": "-24h" }));
    e.code.open(change("chg_t", 8));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_t").unwrap().unwrap();
    let now = e.engine.store.now_ms().unwrap() / 1000;
    let at = e.pg.at.lock().clone();
    assert!(at.len() == 1 && (now - 86_400 - at[0]).abs() < 60, "Postgres as of a day ago: {at:?}");
    let said: Vec<String> = e.engine.store.activity(&p.id).unwrap().into_iter().map(|a| a.message).collect();
    assert!(said.iter().any(|m| m.contains("keeps no history: branched as of now")), "{said:?}");
}


/// A Git URL service on two projects' canvases has a preview of each branch in both, but only the
/// project that has had it longest reports on GitHub; the other's preview says another project
/// does (never naming another owner's). A branch the first project's rules skip is the other's to
/// report. Once the service leaves the first canvas, the next project takes over, editing the
/// same pull request comment, and the first never says its preview went.
#[tokio::test]
async fn the_project_that_has_had_a_service_longest_speaks_for_its_branches() {
    use roll_workspace::{Kind as Res, Scope};
    struct Ada;
    impl roll_workspace::Owners for Ada {
        fn trusts(&self, user: &str, _kind: Res, r: &str) -> bool {
            user == "usr_ada" && r == "svc_shop"
        }
        fn owns(&self, user: &str, kind: Res, r: &str) -> bool {
            self.trusts(user, kind, r)
        }
    }
    let e = env(json!({ "enabled": true, "branches": ["feat/*"] }));
    e.ws.set_owners(Arc::new(Ada));
    e.ws.place(Scope::All, &e.project, Res::Service, "svc_shop", 0.0, 0.0).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let theirs = e.ws.create("usr_ada", "ada-shop", "", None).unwrap();
    e.ws.place(Scope::All, &theirs.id, Res::Service, "svc_shop", 0.0, 0.0).unwrap();
    e.engine.store.put_settings(&theirs.id, &roll_previews::Settings::default().patched(&json!({ "enabled": true })).unwrap()).unwrap();
    *e.apps.catalog.lock() = Some(vec![roll_previews::ports::CatalogService { name: "shop".into(), repo: Some("git:svc_shop".into()), root: String::new(), memory_mb: 256 }]);
    let gr = Arc::new(FakeGitReport::default());
    let _ = e.engine.git_report.set(gr.clone());
    let push = |branch: &str, number: u64| roll_previews::ports::GitPush {
        service: "svc_shop".into(),
        url: "https://github.com/acme/shop.git".into(),
        branch: branch.into(),
        sha: Some("a1".into()),
        message: String::new(),
        pusher: None,
        pull: Some(roll_previews::ports::GitPull { number, title: "Cart".into(), url: format!("https://github.com/acme/shop/pull/{number}"), fork: false, author: None }),
    };
    e.engine.git_push(&push("feat/cart", 3)).await.unwrap();
    e.engine.git_push(&push("wip/x", 4)).await.unwrap();
    for _ in 0..3 {
        e.engine.pass().await.unwrap();
    }
    let branch_of = |project: &str, branch: &str| e.engine.store.rows(Some(project)).unwrap().into_iter().find(|r| r.repo == "git:svc_shop" && r.branch.as_deref() == Some(branch));
    let posted_by = |slug: &str, n: u64| gr.statuses.lock().iter().any(|s| s.4.as_deref().is_some_and(|u| u.contains(slug))) && gr.comments.lock().iter().any(|c| c.0 == n);
    let ours = branch_of(&e.project, "feat/cart").expect("the first project's preview");
    let quiet = branch_of(&theirs.id, "feat/cart").expect("the other's too");
    assert_eq!(e.engine.store.preview(&ours.id).unwrap().reported_by, None);
    assert_eq!(e.engine.store.preview(&quiet.id).unwrap().reported_by.as_deref(), Some("another project"), "says another does, without naming another owner's project");
    let cart_theirs = || gr.statuses.lock().iter().any(|s| s.4.as_deref().is_some_and(|u| u.contains("cart") && u.contains("ada-shop")));
    assert!(!cart_theirs(), "only the first project's preview of the branch posts");
    // A branch the first project's rules skip: the other project's preview speaks for it.
    assert!(branch_of(&e.project, "wip/x").is_none() && posted_by("ada-shop", 4), "a skipped branch is the other's to report");

    // The service leaves the first project's canvas: the next one edits the same comment.
    let made = gr.comments.lock().len();
    e.ws.remove(Scope::All, &e.project, Res::Service, "svc_shop").unwrap();
    for _ in 0..3 {
        e.engine.pass().await.unwrap();
    }
    assert_eq!(e.engine.store.preview(&quiet.id).unwrap().reported_by, None);
    assert!(cart_theirs(), "the next project's preview speaks");
    let after: Vec<_> = gr.comments.lock()[made..].iter().filter(|c| c.0 == 3).cloned().collect();
    assert!(!after.is_empty() && after.iter().all(|c| c.1 == Some(77) && !c.2.contains("deleted")), "the same comment, never saying the preview went: {after:?}");
}

/// A preview made by hand (`POST /v1/previews`, `roll preview create`) is only of a repo the
/// project stands for, as the ones it makes itself: anyone may put any repo on their own canvas,
/// and a preview builds and runs it. And a change's preview in another project is never answered.
#[tokio::test]
async fn a_preview_made_by_hand_is_only_of_a_repo_the_project_stands_for() {
    use roll_workspace::{Kind as Res, Scope};
    struct Ada;
    impl roll_workspace::Owners for Ada {
        fn trusts(&self, user: &str, _kind: Res, r: &str) -> bool {
            user == "usr_ada" && r == "acme/api"
        }
        fn owns(&self, user: &str, kind: Res, r: &str) -> bool {
            self.trusts(user, kind, r)
        }
    }
    let e = env(json!({ "enabled": true }));
    e.ws.set_owners(Arc::new(Ada));
    // A change of the repo Ada owns, previewed in the operator's project before it's on her canvas.
    e.code.open(roll_previews::ports::ChangeInfo { repo: "acme/api".into(), ..change("chg_a", 5) });
    e.engine.pass().await.unwrap();
    let ours = e.engine.store.row_by_change("chg_a").unwrap().unwrap();
    let theirs = e.ws.create("usr_ada", "ada-shop", "", None).unwrap();
    // Ada's canvas has a repo she doesn't own, and the one she does.
    for r in ["acme/web", "acme/api"] {
        e.ws.place(Scope::All, &theirs.id, Res::Repo, r, 0.0, 0.0).unwrap();
    }
    e.engine.store.put_settings(&theirs.id, &roll_previews::Settings::default().patched(&json!({ "enabled": true })).unwrap()).unwrap();
    e.code.heads.lock().insert("acme/web@feat/x".into(), "cs_x".into());
    let err = e.engine.create(&theirs.id, "acme/web", Some("feat/x"), None, "ada").await.unwrap_err();
    assert!(matches!(err, roll_previews::Error::NotFound(_)), "a repo she doesn't stand for: {err}");
    // The change's preview is the operator's project's: never handed to her.
    let err = e.engine.create(&theirs.id, "acme/api", None, Some("chg_a"), "ada").await.unwrap_err();
    assert!(matches!(err, roll_previews::Error::Conflict(_)) && !err.to_string().contains(&ours.id), "{err}");
}

/// `roll preview up`'s branch (`preview/<who>/<name>`) has a preview of its own: a change of that
/// branch gets no second, and one the rules made just before it goes. A preview made by hand of a
/// pull request's own branch never takes that pull request's preview away.
#[tokio::test]
async fn a_change_of_a_branch_with_its_own_preview_gets_none() {
    let e = env(json!({ "enabled": true }));
    // `roll preview up`'s order: the push opens the change and nudges a pass, then the branch's
    // preview is asked for.
    e.code.heads.lock().insert("acme/web@preview/ana/local".into(), "cs_3a".into());
    e.code.open(ChangeInfo { branch: Some("preview/ana/local".into()), ..change("chg_3", 3) });
    e.engine.pass().await.unwrap();
    let made = e.engine.store.row_by_change("chg_3").unwrap().unwrap();
    let (branch, _) = e.engine.create(&e.project, "acme/web", Some("preview/ana/local"), None, "ana").await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.row(&made.id).map(|r| r.desired).unwrap_or(Desired::Deleted), Desired::Deleted);
    e.engine.pass().await.unwrap();
    assert!(e.engine.store.row_by_change("chg_3").unwrap().is_none_or(|r| r.desired == Desired::Deleted), "no second one");
    assert_eq!(e.engine.store.row(&branch.id).unwrap().desired, Desired::Active);
    // A pull request's own branch, previewed by hand too: its own preview stays.
    e.code.heads.lock().insert("acme/web@feat/4".into(), "cs_4a".into());
    e.code.open(change("chg_4", 4));
    e.engine.pass().await.unwrap();
    e.engine.create(&e.project, "acme/web", Some("feat/4"), None, "ana").await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.engine.store.row_by_change("chg_4").unwrap().map(|r| r.desired), Some(Desired::Active), "the pull request keeps its preview");
}

/// A deploy hook (Vercel's and Netlify's, for a headless CMS's publish webhook) builds the
/// project's previews of its branches again at their heads, each service with nothing cached; a
/// second call within a minute makes no second rebuild; anything but a live hook is a 404; a
/// leaked one stops working.
#[tokio::test]
async fn a_deploy_hook_builds_its_branches_previews_again() {
    let e = env(json!({ "enabled": true }));
    e.code.open(ChangeInfo { branch: Some("feat/cart".into()), ..change("chg_1", 1) });
    e.code.open(ChangeInfo { branch: Some("fix/typo".into()), ..change("chg_2", 2) });
    e.engine.pass().await.unwrap();
    let cart = e.engine.store.row_by_change("chg_1").unwrap().unwrap();
    let (hook, path) = e.engine.create_hook(&e.project, "CMS", &["feat/*".into()]).unwrap();
    let secret = path.strip_prefix("/v1/previews/hooks/").unwrap();
    assert!(secret.starts_with("rlb_") && roll_auth::credentials::checksum_ok(secret), "a roll secret leak scanning finds: {secret}");
    assert!(e.engine.store.hooks(&e.project).unwrap().iter().all(|h| h.id == hook.id), "listed, without its secret");

    let (rebuilt, coalesced) = e.engine.run_hook(secret, Some("203.0.113.7")).unwrap();
    assert_eq!((rebuilt, coalesced), (vec![cart.id.clone()], false), "only its branches' previews");
    e.engine.pass().await.unwrap();
    let services = e.engine.store.services(&cart.id).unwrap().len();
    let fresh: Vec<_> = e.apps.rebuilt.lock().clone();
    assert_eq!(fresh.len(), services, "every service built again: {fresh:?}");
    assert!(fresh.iter().all(|(p, _, n)| *p == cart.id && *n == 1));
    let said = e.engine.store.activity(&cart.id).unwrap().into_iter().map(|a| a.message).collect::<Vec<_>>();
    assert!(said.iter().any(|m| m.contains("deploy hook \"CMS\" (from 203.0.113.7)")), "{said:?}");
    // Converging again builds nothing more: one rebuild per call.
    e.engine.pass().await.unwrap();
    assert_eq!(e.apps.rebuilt.lock().len(), services);
    // Called again within the minute: coalesced.
    assert_eq!(e.engine.run_hook(secret, None).unwrap(), (Vec::new(), true));
    // A guess, or a leaked hook once revoked: the same 404.
    assert!(matches!(e.engine.run_hook("rlb_nope", None), Err(roll_previews::Error::NotFound(_))));
    assert!(e.engine.revoke_leaked_hook(secret).is_some());
    assert!(matches!(e.engine.run_hook(secret, None), Err(roll_previews::Error::NotFound(_))));
}

/// A project with its own preview domain (roll apps' verified wildcard, `*.preview.acme.dev`)
/// has its previews' services answer under it, and only its previews: a label of another
/// project's preview under that domain names nothing.
#[tokio::test]
async fn a_projects_own_preview_domain_serves_only_its_previews() {
    let e = env(json!({ "enabled": true }));
    *e.apps.domain.lock() = Some(("preview.acme.dev".into(), false));
    e.code.open(change("chg_d", 7));
    e.engine.pass().await.unwrap();
    let p = e.engine.store.row_by_change("chg_d").unwrap().unwrap();
    let web = e.engine.store.services(&p.id).unwrap().into_iter().find(|s| s.name == "web").unwrap();
    assert_eq!(web.url, format!("http://{}.preview.acme.dev", web.host), "its URL is on the project's domain");
    assert_eq!(e.engine.store.service_for_domain_host(&web.host, "preview.acme.dev").unwrap(), Some((p.id.clone(), "web".to_string())));
    // Another project's own domain doesn't reach this project's previews.
    let none = |_: &str| String::new();
    e.engine.store.set_project_domain("prj_other", Some(("preview.other.dev", false)), &none, &none).unwrap();
    assert_eq!(e.engine.store.service_for_domain_host(&web.host, "preview.other.dev").unwrap(), None, "only the domain's own project's previews");
    assert_eq!(e.engine.store.service_for_domain_host(&web.host, "preview.unknown.dev").unwrap(), None);
}
