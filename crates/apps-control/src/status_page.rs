//! A project's status page (Better Stack's, Instatus'; docs/apps/README.md, "Status pages"): a
//! public page of its uptime checks and heartbeats, each up or down now and its last 90 days by
//! day, at `status-<slug>.<domain>` (and `/status.json` there). The days are kept as each check
//! runs and each heartbeat pings or is missed (`status_days`); nothing private is on it: a
//! service's and a check's names, never an address, a path, an error or a log line.

use crate::{Apps, Error, Result};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::sync::Arc;

pub const DAYS: i64 = 90;

/// What a page shows: one environment's check or heartbeat, by name.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub service: String,
    #[serde(default = "Item::production")]
    pub environment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<String>,
}

impl Item {
    fn production() -> String {
        crate::model::PRODUCTION.into()
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Page {
    pub project: String,
    pub slug: String,
    pub title: String,
    pub items: Vec<Item>,
}

pub(crate) fn check_key(service: &str, env: &str, name: &str) -> String {
    format!("check:{service}/{env}/{name}")
}

pub(crate) fn heartbeat_key(service: &str, env: &str, name: &str) -> String {
    format!("hb:{service}/{env}/{name}")
}

fn today() -> i64 {
    code_sql::now_ms() / 86_400_000
}

/// One thing shown, as the page has it: who it is, whether it's up now, and its days.
struct Row {
    label: String,
    up: Option<bool>,
    /// (day, checks or pings that passed, that failed or were missed).
    days: Vec<(i64, i64, i64)>,
}

impl Apps {
    /// A check's run, or a heartbeat's ping (true) or miss (false), counted on today.
    pub(crate) async fn status_count(self: &Arc<Self>, key: String, ok: bool) {
        let day = today();
        if let Err(e) = self.db(move |s| s.status_day_add(&key, day, ok)).await {
            tracing::warn!(error = %e, "apps: a status page's day wasn't counted");
        }
    }

    /// `PUT /v1/apps/projects/{project}/status-page {title?, items?}`: made (its address kept)
    /// or changed; no items: every check and heartbeat of the project's environments.
    pub async fn put_status_page(self: &Arc<Self>, project: &str, body: Value) -> Result<Value> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["title", "items"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let title = match o.get("title") {
            None | Some(Value::Null) => "Status".to_string(),
            Some(Value::String(t)) if (1..=80).contains(&t.chars().count()) && !t.contains(['\n', '\r']) => t.clone(),
            Some(_) => return Err(Error::BadRequest("title is 1–80 characters on one line".into())),
        };
        let items: Vec<Item> = match o.get("items") {
            None | Some(Value::Null) => Vec::new(),
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| Error::BadRequest(format!("items: {e}")))?,
        };
        if items.len() > 50 || items.iter().any(|i| i.check.is_some() == i.heartbeat.is_some()) {
            return Err(Error::BadRequest("items are at most 50, each {service, environment?, check | heartbeat}".into()));
        }
        let p = project.to_string();
        let services = self.db(move |s| s.services(Some(&p))).await?;
        for i in &items {
            if !services.iter().any(|s| s.id == i.service) {
                return Err(Error::BadRequest(format!("items: {} isn't this project's service", i.service)));
            }
        }
        let (p, slug, j) = (project.to_string(), crate::model::random(10).to_ascii_lowercase(), serde_json::to_string(&items).expect("items serialize"));
        let page = self.db(move |s| s.put_status_page(&p, &slug, &title, &j)).await?;
        self.status_page_view(page)
    }

    pub async fn get_status_page(self: &Arc<Self>, project: &str) -> Result<Value> {
        let p = project.to_string();
        let page = self.db(move |s| s.status_page(&p)).await?.ok_or_else(|| Error::NotFound(format!("{project} has no status page")))?;
        self.status_page_view(page)
    }

    pub async fn delete_status_page(self: &Arc<Self>, project: &str) -> Result<()> {
        let p = project.to_string();
        self.db(move |s| s.delete_status_page(&p)).await
    }

    fn status_page_view(&self, page: Page) -> Result<Value> {
        Ok(json!({ "title": page.title, "items": page.items, "url": self.url(&format!("status-{}", page.slug)) }))
    }

    /// The page for a host (`status-<slug>.<domain>`), if one has it.
    pub(crate) async fn status_page_for_host(self: &Arc<Self>, host: &str) -> Option<Page> {
        let name = host.split(':').next()?.to_ascii_lowercase();
        let slug = name.strip_suffix(&format!(".{}", self.cfg.domain))?.strip_prefix("status-")?.to_string();
        if slug.is_empty() || !slug.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        self.db(move |s| s.status_page_by_slug(&slug)).await.ok().flatten()
    }

    /// What a page shows now: its items (or everything), each up or down and its days.
    async fn status_rows(self: &Arc<Self>, page: &Page) -> Vec<Row> {
        let p = page.project.clone();
        let Ok((services, envs)) = self.db(move |s| Ok((s.services(Some(&p))?, s.all_environments()?))).await else { return Vec::new() };
        let mut wanted: Vec<(String, String, String, bool)> = Vec::new();
        for svc in &services {
            for e in envs.iter().filter(|e| e.service == svc.id && !e.ephemeral) {
                for c in &e.checks {
                    wanted.push((svc.id.clone(), e.name.clone(), c.name.clone(), true));
                }
                for h in &e.heartbeats {
                    wanted.push((svc.id.clone(), e.name.clone(), h.name.clone(), false));
                }
            }
        }
        if !page.items.is_empty() {
            wanted.retain(|(s, e, n, check)| page.items.iter().any(|i| &i.service == s && &i.environment == e && (if *check { i.check.as_ref() } else { i.heartbeat.as_ref() }) == Some(n)));
        }
        let since = today() - DAYS + 1;
        let mut rows = Vec::new();
        for (sid, env, name, check) in wanted {
            let key = if check { check_key(&sid, &env, &name) } else { heartbeat_key(&sid, &env, &name) };
            let (k, k2) = (key.clone(), format!("status-last:{key}"));
            let (days, last) = self.db(move |s| Ok((s.status_days(&k, since)?, s.get_state(&k2)?))).await.unwrap_or_default();
            let svc_name = services.iter().find(|s| s.id == sid).map(|s| s.name.clone()).unwrap_or_default();
            let label = if env == crate::model::PRODUCTION { format!("{svc_name} · {name}") } else { format!("{svc_name} ({env}) · {name}") };
            rows.push(Row { label, up: last.map(|l| l == "up"), days });
        }
        rows
    }
}

/// The page on its own host: its HTML, or `/status.json`.
pub(crate) async fn serve(apps: Arc<Apps>, page: Page, path: &str) -> Response {
    let rows = apps.status_rows(&page).await;
    let since = today() - DAYS + 1;
    let uptime = |days: &[(i64, i64, i64)]| {
        let (ok, all) = days.iter().fold((0, 0), |(o, a), (_, g, b)| (o + g, a + g + b));
        (all > 0).then(|| (ok as f64 * 1000.0 / all as f64).round() / 10.0)
    };
    if path == "/status.json" {
        let items: Vec<Value> = rows.iter().map(|r| json!({ "name": r.label, "up": r.up, "uptime": uptime(&r.days), "days": (since..=today()).map(|d| r.day(d)).collect::<Vec<_>>() })).collect();
        return (StatusCode::OK, [(header::CACHE_CONTROL, "public, max-age=30")], axum::Json(json!({ "title": page.title, "up": rows.iter().all(|r| r.up != Some(false)), "items": items }))).into_response();
    }
    if path != "/" {
        return (StatusCode::NOT_FOUND, "").into_response();
    }
    let all_up = rows.iter().all(|r| r.up != Some(false));
    let esc = crate::public::escape;
    let mut list = String::new();
    for r in &rows {
        let state = match r.up {
            Some(true) => "<span class=up>up</span>",
            Some(false) => "<span class=down>down</span>",
            None => "<span class=none>no data</span>",
        };
        let bars: String = (since..=today())
            .map(|d| match r.days.iter().find(|x| x.0 == d) {
                None => "<i></i>",
                Some((_, _, b)) if *b > 0 => "<i class=b></i>",
                Some(_) => "<i class=g></i>",
            })
            .collect();
        let pct = uptime(&r.days).map(|u| format!("{u}%")).unwrap_or_else(|| "–".into());
        list.push_str(&format!("<li><div class=row><b>{}</b>{state}</div><div class=bars>{bars}</div><div class=foot><span>{DAYS} days ago</span><span>{pct} up</span><span>today</span></div></li>", esc(&r.label)));
    }
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>{t}</title>\
         <style>:root{{color-scheme:dark light;--bg:#000;--fg:#ededed;--muted:#a1a1a1;--line:#ffffff24;--accent:#4c8dff;--danger:#e5484d;--none:#ffffff1a}}\
         @media (prefers-color-scheme:light){{:root{{--bg:#fff;--fg:#0a0a0a;--muted:#666;--line:#00000024;--none:#0000001a}}}}\
         body{{margin:0;background:var(--bg);color:var(--fg);font:14px system-ui,sans-serif}}main{{max-width:720px;margin:0 auto;padding:40px 16px}}\
         h1{{font-size:22px;font-weight:600;letter-spacing:-.04em;margin:0 0 6px}}.all{{color:var(--muted);margin:0 0 28px}}ul{{list-style:none;margin:0;padding:0}}\
         li{{border:1px solid var(--line);border-radius:10px;padding:14px 16px;margin:0 0 12px}}.row{{display:flex;justify-content:space-between;gap:12px;margin:0 0 10px}}\
         .up{{color:var(--accent)}}.down{{color:var(--danger)}}.none{{color:var(--muted)}}.bars{{display:flex;gap:2px}}.bars i{{flex:1;height:28px;border-radius:2px;background:var(--none)}}\
         .bars i.g{{background:var(--accent)}}.bars i.b{{background:var(--danger)}}.foot{{display:flex;justify-content:space-between;color:var(--muted);font-size:12px;margin-top:6px}}\
         footer{{color:var(--muted);font-size:12px;margin-top:24px}}</style><main><h1>{t}</h1><p class=all>{all}</p><ul>{list}</ul><footer>Kept by roll</footer></main>",
        t = esc(&page.title),
        all = if rows.is_empty() { "Nothing to show yet." } else if all_up { "Everything is up." } else { "Something is down." },
    );
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=30")], html).into_response()
}

impl Row {
    /// A day as `/status.json` has it: null with nothing that day, else its counts.
    fn day(&self, d: i64) -> Value {
        self.days.iter().find(|x| x.0 == d).map_or(Value::Null, |(_, ok, bad)| json!({ "ok": ok, "bad": bad }))
    }
}
