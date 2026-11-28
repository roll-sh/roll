//! The toolbar's Review (Vercel's toolbar, Chromatic's UI Review on the page): for the page a team
//! visitor is on, what the change's record already says against main's preview, page by page (the
//! first page with its widths, modes and browsers at `/`, another page by its path, a state by
//! the page it starts on). Read from the same view the sheet reads, so the page's words are
//! masked as they are there; accepting stays in roll.

use super::Engine;
use crate::Result;
use serde_json::{Value, json};

/// The most of a page's entries answered (its widths, modes, browsers and states).
const ENTRIES_MAX: usize = 12;

impl Engine {
    /// `path` on `service` of preview `id`, as `{branch, state, pages: [...]}`: each entry's key,
    /// share of pixels changed against main's, who accepted or refused it at this head, the
    /// problems it has that main's page doesn't, and its pictures on this host (main's only when
    /// the preview is trusted: a fork's page script reads whatever this answers).
    pub async fn page_review(&self, id: &str, service: &str, path: &str) -> Result<Value> {
        let path = path.split(['?', '#']).next().unwrap_or("");
        // A path of a page is one of its keys' (2000 at most); anything longer is no page's.
        let path = if path.len() <= 2000 && path.starts_with('/') { path } else { "" };
        let r = self.store.row(id)?;
        let trusted = r.trusted;
        let states = self.store.settings(&r.project_id)?.github.states;
        let p = self.store.view(r)?;
        let first = p.services.first().map(|s| s.name.as_str()) == Some(service);
        let (Some(review), Some(base)) = (&p.visual_review, &p.base_change) else {
            return Ok(json!({ "path": path, "compared": false, "pages": [] }));
        };
        // This page's entries, as screenshot runs keep them.
        let mine = |key: &str| -> bool {
            if !first {
                return key.strip_prefix(service).and_then(|k| k.strip_prefix(':')) == Some(path);
            }
            if path == "/" && (key.starts_with('@') || key.starts_with("mode:") || key.starts_with("browser:")) {
                return true;
            }
            match key.strip_prefix("state:") {
                Some(name) => states.iter().any(|s| s.name == name && s.page.split('?').next() == Some(path)),
                None => key.split('?').next() == Some(path),
            }
        };
        let reviewed = |key: &str| review.pages.iter().find(|pg| pg.path == key);
        let pictures = |key: &str| {
            let q = |pic: &str| format!("{}?page={}&pic={pic}", crate::runtime::REVIEW_PICTURE_PATH, crate::visit::encode(key));
            if trusted { json!({ "before": q("before"), "after": q("after"), "diff": q("diff") }) } else { json!({ "after": q("after") }) }
        };
        let entry = |key: &str, percent: Option<f64>, added: Value, problems: Value| {
            let pg = reviewed(key);
            json!({
                "page": key,
                "percent": percent,
                "accepted_by": pg.and_then(|pg| pg.by.clone()),
                "refused": pg.and_then(|pg| pg.refused.as_ref()).map(|f| json!({ "by": f.by, "reason": f.reason })),
                "unstable": pg.is_some_and(|pg| pg.unstable),
                "added": added,
                "problems": problems,
                "pictures": pictures(key),
            })
        };
        let a11y_kinds = |a: Option<&crate::PageA11y>| -> Vec<Value> { a.map(|a| a.issues.iter().take(10).map(|i| json!({ "kind": i.kind, "count": i.count })).collect()).unwrap_or_default() };
        let broken = |l: Option<&crate::PageLinks>| -> Vec<Value> { l.map(|l| l.broken.iter().take(5).map(|b| json!({ "path": b.path, "status": b.status })).collect()).unwrap_or_default() };
        let mut pages = Vec::new();
        if first && path == "/" {
            let h = p.page_health.as_ref();
            pages.push(entry(
                "",
                Some(base.percent.max(base.phone_percent.unwrap_or(0.0))),
                json!({ "a11y": base.a11y_added, "errors": base.errors_added, "links": base.links_added, "seo": base.seo_added }),
                json!({ "a11y": a11y_kinds(p.page_a11y.as_ref()), "links": broken(p.page_links.as_ref()), "first_error": h.and_then(|h| h.first_error.clone()), "first_failed": h.and_then(|h| h.first_failed.clone()) }),
            ));
        }
        for c in p.pages.iter().filter(|c| mine(&c.path)) {
            let a = c.against.as_ref();
            pages.push(entry(
                &c.path,
                c.base_percent,
                json!({ "a11y": a.and_then(|a| a.a11y_added), "links": a.and_then(|a| a.links_added), "seo": a.and_then(|a| a.seo_added) }),
                // Another page's errors aren't compared with main's: how many it had, as they are.
                json!({ "a11y": a11y_kinds(c.a11y.as_ref()), "links": broken(c.links.as_ref()), "errors": c.errors }),
            ));
        }
        pages.truncate(ENTRIES_MAX);
        Ok(json!({ "path": path, "compared": true, "branch": base.branch, "state": review.state, "head": review.head, "pages": pages }))
    }

    /// An entry's picture for the toolbar's Review: `after` (this preview's), `before` (main's it
    /// was compared with) or `diff` (where they differ); main's only for a trusted preview.
    pub async fn page_review_picture(&self, id: &str, key: &str, pic: &str) -> Result<Option<Vec<u8>>> {
        if !(key.is_empty() || crate::settings::page_key_ok(key)) {
            return Ok(None);
        }
        let r = self.store.row(id)?;
        if pic != "after" && !r.trusted {
            return Ok(None);
        }
        let page = (!key.is_empty()).then_some(key);
        let after = || match page {
            Some(k) => self.store.page_screenshot(id, k, false),
            None => Ok(self.store.screenshot(id)?.map(|(_, png)| png)),
        };
        Ok(match pic {
            "after" => after()?,
            "before" => self.store.base_screenshot(id, false, page)?,
            "diff" => match (self.store.base_screenshot(id, false, page)?, after()?) {
                (Some(before), Some(after)) => tokio::task::spawn_blocking(move || crate::visual::diff(&before, &after)).await.ok().flatten(),
                _ => None,
            },
            _ => None,
        }
        .filter(|png| !png.is_empty()))
    }
}
