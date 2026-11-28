//! A change's preview's recent requests replayed against main's preview (Signadot's SmartTests,
//! Speedscale's replays, without capturing production): the GETs both previews have served
//! lately, asked of each again, and their answers compared (the status, and a JSON answer's
//! top-level fields). Only the previews' own request logs are read: nothing of production's.

use super::*;

/// The most requests replayed at once.
const REPLAY_MAX: usize = 40;
/// How many are asked at a time (each of both previews).
const REPLAY_AT_ONCE: usize = 8;
/// The longest a replay runs (an answer that streams never ends one); one asked longer ago that
/// never said how it went was lost (a restart) and may be asked again.
const REPLAY_FOR: std::time::Duration = std::time::Duration::from_secs(180);
/// Paths a GET acts on (signing out, unsubscribing, a token's one use): never replayed.
const ACTS: [&str; 6] = ["logout", "log-out", "signout", "sign-out", "unsubscribe", "confirm"];

impl Engine {
    /// `POST …/{id}/replay`: asked for now (202), replayed in the background; `replay` on the
    /// preview has it at its head once done. A trusted change's preview only: the paths a fork's
    /// visitors and code chose would be sent to main's preview, with its secrets.
    pub async fn replay(self: &Arc<Self>, id: &str, by: &str) -> Result<crate::Replay> {
        let r = self.store.row(id)?;
        if r.change_id.is_none() {
            return Err(Error::BadRequest("only a change's preview is replayed against main's".into()));
        }
        if !r.trusted {
            return Err(Error::Conflict("a preview of a pull request from a fork isn't replayed against main's: its paths are its code's".into()));
        }
        let now = self.store.now_ms()?;
        if self.store.runtime_fact::<crate::Replay>(id, "replay")?.is_some_and(|x| x.ok.is_none() && x.head == r.build() && now - x.at < REPLAY_FOR.as_millis() as i64 + 60_000) {
            return Err(Error::Conflict("it's being replayed".into()));
        }
        let asked = crate::Replay { head: r.build().to_string(), by: by.to_string(), at: self.store.now_ms()?, ok: None, requests: Vec::new(), error: None };
        self.store.set_runtime_fact(id, "replay", Some(&asked))?;
        self.store.log(id, &format!("{by} asked for its recent requests to be replayed against main's preview"))?;
        let (me, pid, base) = (self.clone(), id.to_string(), asked.clone());
        tokio::spawn(async move {
            let asked = base;
            let ran = tokio::time::timeout(REPLAY_FOR, me.replayed(&pid)).await.unwrap_or_else(|_| Err(anyhow::anyhow!("it didn't finish within {} s", REPLAY_FOR.as_secs())));
            let done = match ran {
                Ok(requests) => crate::Replay { ok: Some(!requests.iter().any(|q| q.differs())), requests, ..asked.clone() },
                Err(e) => crate::Replay { ok: Some(false), error: Some(me.mask_text(&pid, &format!("{e:#}").lines().next().unwrap_or("").chars().take(300).collect::<String>()).await.unwrap_or_default()), ..asked.clone() },
            };
            let differ = done.requests.iter().filter(|q| q.differs()).count();
            let _ = me.store.log(&pid, &format!("replayed {} request(s) against main's preview: {differ} answered differently", done.requests.len()));
            let _ = me.store.set_runtime_fact(&pid, "replay", Some(&done));
        });
        Ok(asked)
    }

    async fn replayed(&self, id: &str) -> anyhow::Result<Vec<crate::ReplayedRequest>> {
        let r = self.store.row(id)?;
        let main = self.main_preview(&r).await?.ok_or_else(|| anyhow::anyhow!("the project has no preview of main"))?;
        let ours = self.store.services(id)?;
        let theirs = self.store.services(&main.id)?;
        // The GETs both have served lately (newest first, each once), of a service both run.
        let mut asked: Vec<(String, String)> = Vec::new();
        for l in self.apps.requests(id).into_iter().chain(self.apps.requests(&main.id)) {
            let key = (l.service.clone(), l.path.clone());
            let acts = ACTS.iter().any(|w| l.path.to_ascii_lowercase().contains(w));
            if l.method != "GET" || l.path.starts_with("/__roll") || acts || !crate::settings::page_path_ok(&l.path) || asked.contains(&key) {
                continue;
            }
            if ours.iter().any(|s| s.name == l.service) && theirs.iter().any(|s| s.name == l.service) {
                asked.push(key);
            }
            if asked.len() >= REPLAY_MAX {
                break;
            }
        }
        anyhow::ensure!(!asked.is_empty(), "neither preview has served a GET lately to replay");
        let own_access = self.store.access(id)?.filter(|a| a.required()).map(|a| a.token);
        let main_access = self.store.access(&main.id)?.filter(|a| a.required()).map(|a| a.token);
        let url_of = |list: &[crate::Service], name: &str| list.iter().find(|s| s.name == name).map(|s| s.url.clone()).unwrap_or_default();
        let mut out = Vec::new();
        for chunk in asked.chunks(REPLAY_AT_ONCE) {
            let answers = futures::future::join_all(chunk.iter().map(|(svc, path)| {
                let (ours_url, theirs_url) = (url_of(&ours, svc), url_of(&theirs, svc));
                let (own_access, main_access, main_id) = (own_access.as_deref(), main_access.as_deref(), main.id.as_str());
                async move {
                    let a = self.apps.fetch(id, svc, &ours_url, own_access, path, false).await;
                    let b = self.apps.fetch(main_id, svc, &theirs_url, main_access, path, false).await;
                    (svc.clone(), path.clone(), a, b)
                }
            }))
            .await;
            for (service, path, a, b) in answers {
                // A backend that can't read it isn't replayed; no answer is 0.
                let read = |x: anyhow::Result<Option<crate::ports::Fetched>>| match x {
                    Ok(f) => f,
                    Err(_) => Some(none()),
                };
                let (Some(a), Some(b)) = (read(a), read(b)) else { continue };
                let (ours_keys, main_keys) = (top_level_keys(&a), top_level_keys(&b));
                let (removed, added) = match (ours_keys, main_keys) {
                    (Some(o), Some(m)) => (m.iter().filter(|k| !o.contains(k)).cloned().collect(), o.iter().filter(|k| !m.contains(k)).cloned().collect()),
                    _ => (Vec::new(), Vec::new()),
                };
                out.push(crate::ReplayedRequest { service, path, status: a.status, main_status: b.status, removed, added });
            }
        }
        // Said as its logs are: a path is its code's to choose.
        let masked = self.masked(id, out.iter().map(|q| q.path.clone()).collect()).await?;
        for (q, p) in out.iter_mut().zip(masked) {
            q.path = p;
        }
        Ok(out)
    }
}

/// No answer (refused, timed out).
fn none() -> crate::ports::Fetched {
    crate::ports::Fetched { status: 0, headers: Vec::new(), body: Vec::new(), truncated: false }
}

/// A JSON object answer's top-level field names (at most 50), sorted; `None` for anything else.
fn top_level_keys(f: &crate::ports::Fetched) -> Option<Vec<String>> {
    let json = f.headers.iter().any(|(k, v)| k == "content-type" && v.contains("json"));
    if !json || f.truncated {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&f.body).ok()?;
    let mut keys: Vec<String> = v.as_object()?.keys().take(50).cloned().collect();
    keys.sort();
    Some(keys)
}
