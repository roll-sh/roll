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
    assert!(e.engine.accept_visual(&base.id, "ada", None, true, None, None).await.is_err(), "only a change's");
    // Not by its own author.
    let err = e.engine.accept_visual(&chg.id, "carol", None, true, Some("cs_1b"), None).await.unwrap_err();
    assert!(err.to_string().contains("own author"), "{err}");
    let err = e.engine.accept_visual(&chg.id, "token:ci", Some("carol"), false, Some("cs_1b"), None).await.unwrap_err();
    assert!(err.to_string().contains("own author"), "nor with their own token: {err}");
    // What was seen: an older screenshot's head is refused, this one's accepted.
    let err = e.engine.accept_visual(&chg.id, "ada", None, true, Some("cs_1a"), None).await.unwrap_err();
    assert!(err.to_string().contains("a newer screenshot came in"), "{err}");
    // One page at a time: the change waits until every changed page is accepted.
    let p = e.engine.accept_visual(&chg.id, "ada", None, true, Some("cs_1b"), Some("/pricing")).await.unwrap();
    assert_eq!(p.visual_review.map(|v| v.state), Some(VisualState::Changed));
    assert_eq!(waiting(), Some(vec!["".to_string()]));
    assert!(e.engine.accept_visual(&chg.id, "ada", None, true, Some("cs_1b"), Some("/nowhere")).await.is_err(), "a page it has no screenshot of");
    let p = e.engine.accept_visual(&chg.id, "ada", None, true, Some("cs_1b"), Some("")).await.unwrap();
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
    e.engine.accept_visual(&chg.id, "ada", None, true, Some("cs_1d"), None).await.unwrap();
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
    let timed = |ms| roll_previews::PageSpeed { ttfb_ms: 1, fcp_ms: None, load_ms: 9, bytes: 1, lcp_ms: None, cls: None, endpoints: vec![roll_previews::EndpointTime { path: "/api/users".into(), ms, status: 200 }] };
    e.engine.store.set_screenshot(&base.id, "cs_main3", &png(255), Some(&timed(40)), None).unwrap();
    e.engine.store.set_screenshot(&chg.id, "cs_1f", &dot, Some(&timed(140)), None).unwrap();
    e.engine.store.compare_with_base(&chg.id, &base.id, "main", None).unwrap();
    let ep = e.engine.store.preview(&chg.id).unwrap().base_change.unwrap().endpoints;
    assert!(ep.len() == 1 && ep[0].main_ms == Some(40) && ep[0].slower(), "{ep:?}");
    // Main's API document read and the change's not: said so, nothing compared.
    e.engine.store.set_page_report(&base.id, None, None, None, None, Some(&["GET /users".to_string()])).unwrap();
    e.engine.store.set_page_report(&chg.id, None, None, None, None, None).unwrap();
    e.engine.store.compare_with_base(&chg.id, &base.id, "main", None).unwrap();
    let b = e.engine.store.preview(&chg.id).unwrap().base_change.unwrap();
    assert!(b.api_unread && b.api.is_none(), "{b:?}");
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
    assert_eq!(followed, ["listening on :3000", "charging with ***"]);
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
    e.code.edit("chg_r", |c| c.head = "cs_3b".into());
    e.engine.pass().await.unwrap();
    let runs = e.apps.runs.lock().clone();
    let heads: Vec<_> = runs.iter().map(|(_, rev, deployed)| (rev.as_str(), *deployed)).collect();
    assert_eq!(heads, [("cs_3a", 0), ("cs_3a", 0), ("cs_3b", 0)], "{runs:?}");
    let v = e.engine.store.view(e.engine.store.row(&p.id).unwrap()).unwrap();
    assert!(v.release.is_some_and(|x| x.head == "cs_3b" && x.before_ms.is_some()), "a new head's run, with the one before's time");
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
    assert_eq!(call(other, format!("/v1/previews/{}/qr.png", p.id)).await.0, 404, "its link with the access token, as a QR code");
    assert_eq!(call(other, format!("/v1/previews/{}/connect", p.id)).await.0, 404, "its branches' credentials");
    assert_eq!(call(other, format!("/v1/previews/projects/{}/settings", e.project)).await.0, 404);
    // Their own project's: there.
    let mine = e.ws.create("usr_b", "mine", "", None).unwrap();
    assert_eq!(call(other, format!("/v1/previews/projects/{}/settings", mine.id)).await.0, 200);
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
    let pull = roll_previews::ports::GitPull { number: 3, title: "Cart\nbadge".into(), url: "https://github.com/acme/shop/pull/3".into(), fork: false };
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
    // A fork's head on roll apps runs only where the project opened forks' egress (roll apps'
    // build isn't held to Roll only).
    let e = env(json!({ "enabled": true, "egress_forks": "open" }));
    e.ws.place(roll_workspace::Scope::All, &e.project, roll_workspace::Kind::Service, "svc_shop", 0.0, 0.0).unwrap();
    *e.apps.catalog.lock() = Some(vec![roll_previews::ports::CatalogService { name: "shop".into(), repo: Some("git:svc_shop".into()), root: String::new(), memory_mb: 256 }]);
    let push = |sha: Option<&str>| roll_previews::ports::GitPush {
        service: "svc_shop".into(),
        url: "https://github.com/acme/shop.git".into(),
        branch: "pull/7".into(),
        sha: sha.map(Into::into),
        message: String::new(),
        pull: Some(roll_previews::ports::GitPull { number: 7, title: "Faster cart".into(), url: "https://github.com/acme/shop/pull/7".into(), fork: true }),
    };
    let deployed = |id: &str| e.apps.deployed.lock().get(&(id.to_string(), "shop".to_string())).cloned();
    e.engine.git_push(&push(Some("f1"))).await.unwrap();
    e.engine.pass().await.unwrap();
    let r = e.engine.store.rows(Some(&e.project)).unwrap().into_iter().find(|r| r.repo == "git:svc_shop").unwrap();
    assert_eq!((r.state, r.trusted, r.title.as_str(), deployed(&r.id)), (State::AwaitingApproval, false, "Faster cart", None), "nothing runs before an approval");
    assert!(matches!(e.engine.approve(&r.id, "f0", "ada"), Err(roll_previews::Error::Conflict(_))), "only the head it's at");
    // Under Roll only, roll apps' build of it would still reach out: refused, until opened.
    e.engine.store.put_settings(&e.project, &e.engine.store.settings(&e.project).unwrap().patched(&json!({ "egress": "roll" })).unwrap()).unwrap();
    assert!(matches!(e.engine.approve(&r.id, "f1", "ada"), Err(roll_previews::Error::Conflict(_))), "not under Roll only");
    e.engine.store.put_settings(&e.project, &e.engine.store.settings(&e.project).unwrap().patched(&json!({ "egress": "open" })).unwrap()).unwrap();
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

