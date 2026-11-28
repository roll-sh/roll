//! A change's preview's recent requests replayed against main's preview (Signadot's SmartTests,
//! Speedscale's replays, without capturing production): the GETs both previews have served
//! lately, asked of each again, and their answers compared (the status, and a JSON answer's
//! top-level fields). Only the previews' own request logs are read: nothing of production's.
//! Where both run on roll's runtime, each preview's services run again as `<name>@replay-<change's
//! preview>` on fresh copies of its branches (deleted after) and are asked there, never on the
//! branches they serve from (a GET can act too); with the project's `replay_writes`, the writes
//! both kept (runtime.rs keeps them, sealed) are replayed there as well.

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
/// The most writes replayed (the newest; replayed oldest first, as state builds up).
const WRITES_MAX: usize = 20;

/// What a replay of writes made, to undo whatever happened (an error, the time running out).
#[derive(Default)]
struct OnCopies {
    copies: Vec<super::main_on_change::Copy>,
    /// (preview, service as it runs: `web@replay-…`).
    runs: Vec<(String, String)>,
}

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
        let asked = crate::Replay { head: r.build().to_string(), by: by.to_string(), at: self.store.now_ms()?, ok: None, requests: Vec::new(), error: None, copies_error: None };
        self.store.set_runtime_fact(id, "replay", Some(&asked))?;
        self.store.log(id, &format!("{by} asked for its recent requests to be replayed against main's preview"))?;
        let (me, pid, base) = (self.clone(), id.to_string(), asked.clone());
        tokio::spawn(async move {
            let asked = base;
            let on_copies = Mutex::new(OnCopies::default());
            let ran = tokio::time::timeout(REPLAY_FOR, me.replayed(&pid, &on_copies)).await.unwrap_or_else(|_| Err(anyhow::anyhow!("it didn't finish within {} s", REPLAY_FOR.as_secs())));
            // The runs on copies stop and the copies go, whatever happened.
            me.off_copies(&pid, on_copies.into_inner()).await;
            let one_line = |e: &anyhow::Error| format!("{e:#}").lines().next().unwrap_or("").chars().take(300).collect::<String>();
            let done = match ran {
                Ok((requests, copies_error)) => {
                    let copies_error = match copies_error {
                        Some(e) => Some(me.mask_text(&pid, &one_line(&e)).await.unwrap_or_default()),
                        None => None,
                    };
                    crate::Replay { ok: Some(!requests.iter().any(|q| q.differs())), requests, copies_error, ..asked.clone() }
                }
                Err(e) => crate::Replay { ok: Some(false), error: Some(me.mask_text(&pid, &one_line(&e)).await.unwrap_or_default()), ..asked.clone() },
            };
            let differ = done.requests.iter().filter(|q| q.differs()).count();
            let writes = done.requests.iter().filter(|q| q.method != "GET").count();
            let on = match (&done.copies_error, done.error.is_some()) {
                (None, false) => format!(" on copies of both previews' data ({writes} write(s))"),
                _ => String::new(),
            };
            let _ = me.store.log(&pid, &format!("replayed {} request(s) against main's preview{on}: {differ} answered differently", done.requests.len()));
            if let Some(e) = &done.copies_error {
                let _ = me.store.log(&pid, &format!("not on copies of the data (GETs asked as they serve, kept writes not replayed): {e}"));
            }
            let _ = me.store.set_runtime_fact(&pid, "replay", Some(&done));
        });
        Ok(asked)
    }

    /// The GETs, then the writes; `Some` error when it couldn't run on copies of both previews'
    /// data (the GETs then asked of the previews as they serve, kept writes not replayed).
    async fn replayed(&self, id: &str, on_copies: &Mutex<OnCopies>) -> anyhow::Result<(Vec<crate::ReplayedRequest>, Option<anyhow::Error>)> {
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
        let writes = self.writes_to_replay(&r, &main, &ours, &theirs)?;
        anyhow::ensure!(!asked.is_empty() || !writes.is_empty(), "neither preview has served a GET lately to replay");
        // Both previews' services on copies of their own data, where both run on roll's runtime:
        // a replayed GET can act too. Elsewhere GETs are asked of the previews as they serve
        // (never a path that acts), and kept writes aren't replayed.
        let mut names: Vec<&str> = asked.iter().map(|(s, _)| s.as_str()).chain(writes.iter().map(|w| w.service.as_str())).collect();
        names.sort();
        names.dedup();
        let (tag, copies_error) = match self.onto_copies(&r, &main, &ours, &theirs, &names, on_copies).await {
            Ok(tag) => (Some(tag), None),
            Err(e) => (None, Some(e)),
        };
        let own_access = self.store.access(id)?.filter(|a| a.required()).map(|a| a.token);
        let main_access = self.store.access(&main.id)?.filter(|a| a.required()).map(|a| a.token);
        let url_of = |list: &[crate::Service], name: &str| list.iter().find(|s| s.name == name).map(|s| s.url.clone()).unwrap_or_default();
        let mut out = Vec::new();
        for chunk in asked.chunks(REPLAY_AT_ONCE) {
            let answers = futures::future::join_all(chunk.iter().map(|(svc, path)| {
                let (ours_url, theirs_url) = (url_of(&ours, svc), url_of(&theirs, svc));
                let (own_access, main_access, main_id) = (own_access.as_deref(), main_access.as_deref(), main.id.as_str());
                let tag = tag.as_deref();
                async move {
                    let (a, b) = match tag {
                        // roll's runtime reads its own services past their gate.
                        Some(tag) => {
                            let run = format!("{svc}@{tag}");
                            tokio::join!(self.apps.fetch(id, &run, "", None, path, false), self.apps.fetch(main_id, &run, "", None, path, false))
                        }
                        None => tokio::join!(self.apps.fetch(id, svc, &ours_url, own_access, path, false), self.apps.fetch(main_id, svc, &theirs_url, main_access, path, false)),
                    };
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
                out.push(compared(service, "GET".into(), path, a, b));
            }
        }
        // The writes, one at a time on both (state builds up), only on the copies.
        if let Some(tag) = &tag {
            for w in &writes {
                use base64::Engine as _;
                let body = base64::engine::general_purpose::STANDARD.decode(&w.body).unwrap_or_default();
                let run = format!("{}@{tag}", w.service);
                let (a, b) = tokio::join!(
                    self.apps.send(id, &run, &w.method, &w.path, w.content_type.as_deref(), body.clone()),
                    self.apps.send(&main.id, &run, &w.method, &w.path, w.content_type.as_deref(), body),
                );
                let read = |x: anyhow::Result<Option<crate::ports::Fetched>>| x.unwrap_or_else(|_| Some(none())).ok_or_else(|| anyhow::anyhow!("this server can't send a write to {}", w.service));
                out.push(compared(w.service.clone(), w.method.clone(), w.path.clone(), read(a)?, read(b)?));
            }
        }
        // Said as its logs are: a path is its code's to choose.
        let masked = self.masked(id, out.iter().map(|q| q.path.clone()).collect()).await?;
        for (q, p) in out.iter_mut().zip(masked) {
            q.path = p;
        }
        Ok((out, copies_error))
    }

    /// The writes both previews kept (`replay_writes`) of a service both run, each once (by its
    /// service, method, path and body), the newest [`WRITES_MAX`], oldest first. With
    /// `replay_writes` off, what either kept goes.
    fn writes_to_replay(&self, r: &Row, main: &Row, ours: &[crate::Service], theirs: &[crate::Service]) -> anyhow::Result<Vec<crate::store::KeptWrite>> {
        if !self.store.settings(&r.project_id)?.replay_writes {
            self.store.drop_writes(&r.id)?;
            self.store.drop_writes(&main.id)?;
            return Ok(Vec::new());
        }
        let mut all = self.store.kept_writes(&r.id)?;
        all.extend(self.store.kept_writes(&main.id)?);
        all.sort_by_key(|w| w.at);
        let mut out: Vec<crate::store::KeptWrite> = Vec::new();
        for w in all.into_iter().rev() {
            let both = ours.iter().any(|s| s.name == w.service) && theirs.iter().any(|s| s.name == w.service);
            let seen = out.iter().any(|o| (&o.service, &o.method, &o.path, &o.body) == (&w.service, &w.method, &w.path, &w.body));
            if both && !seen && out.len() < WRITES_MAX {
                out.push(w);
            }
        }
        out.reverse();
        Ok(out)
    }

    /// `names`, each of both previews' services, on fresh copies of its own branches as
    /// `<name>@replay-<change's preview>` (its own revision; the copy's variables in place of the
    /// branches'): the tag. What's made goes in `on_copies`, for [`Self::off_copies`].
    async fn onto_copies(&self, r: &Row, main: &Row, ours: &[crate::Service], theirs: &[crate::Service], names: &[&str], on_copies: &Mutex<OnCopies>) -> anyhow::Result<String> {
        // roll's runtime runs every one of them on both (roll apps' proxy keeps no body and takes
        // no write from roll).
        for (p, list) in [(r, ours), (main, theirs)] {
            for name in names {
                let runtime = self.apps.live_build(&p.id, name).await.is_none() && self.apps.can_swap(&p.id, name).await.unwrap_or(false);
                anyhow::ensure!(runtime, "{name} runs on roll apps: a replay runs on copies of both previews' data only where both run on roll's runtime");
                anyhow::ensure!(list.iter().any(|s| s.name == *name && s.revision.is_some()), "{name} isn't deployed yet");
            }
        }
        // Both previews' branches copied, named by the change's preview (so two changes replaying
        // against main never share a copy); one a crash left is cleared by its name first.
        self.clear_replay_copies(r, Some(main)).await?;
        let tag = format!("replay-{}", r.id.trim_start_matches("pv_"));
        for (p, name) in replay_copies(r, main) {
            let list = if p.id == r.id { ours } else { theirs };
            let mut vars = self.resource_env(&p.id).await?;
            for res in self.store.resources(&p.id)? {
                let Some(branch) = &res.r#ref else { continue };
                let b = self.brancher(res.kind)?.clone();
                let copy = b.snapshot(&res.parent, branch, &name).await?.ok_or_else(|| anyhow::anyhow!("{}:{} can't be copied, so a replay would reach the preview's own", res.kind.as_str(), res.parent))?;
                on_copies.lock().copies.push(super::main_on_change::Copy { b: b.clone(), parent: res.parent.clone(), from: branch.clone(), r#ref: copy.r#ref.clone(), kept: copy.kept.clone(), name: name.clone() });
                let old = crate::names::with_standard(res.kind, b.connect(&res.parent, branch, res.kept.as_deref()).await?);
                let new = crate::names::with_standard(res.kind, b.connect(&res.parent, &copy.r#ref, copy.kept.as_deref()).await?);
                super::main_on_change::swap(&mut vars, &old, &new);
            }
            for n in names {
                let rev = list.iter().find(|s| s.name == *n).and_then(|s| s.revision.clone()).unwrap_or_default();
                let run = format!("{n}@{tag}");
                self.main_runs.lock().insert((p.id.clone(), run.clone()), (rev, vars.clone()));
                on_copies.lock().runs.push((p.id.clone(), run));
            }
        }
        Ok(tag)
    }

    /// The copies a replay of `r`'s requests would make (on it, and on `main`'s preview), gone where
    /// one is left (a crash before they went), found by their names.
    pub(super) async fn clear_replay_copies(&self, r: &Row, main: Option<&Row>) -> anyhow::Result<()> {
        let Some(main) = main else { return Ok(()) };
        for (p, name) in replay_copies(r, main) {
            for res in self.store.resources(&p.id)? {
                let Some(branch) = &res.r#ref else { continue };
                let b = self.brancher(res.kind)?.clone();
                if let Some(left) = b.find(&res.parent, &name).await? {
                    b.delete_snapshot(&res.parent, branch, &left, None, &name).await?;
                }
            }
        }
        Ok(())
    }

    /// What a replay of writes made, undone: its runs stopped (their variables gone with them),
    /// then the copies deleted as an earlier push's copy is (never `delete`: a product whose guard
    /// checks the direct parent checks `from`).
    async fn off_copies(&self, id: &str, made: OnCopies) {
        for (preview, run) in made.runs {
            self.main_runs.lock().remove(&(preview.clone(), run.clone()));
            if let Err(e) = self.apps.remove(&preview, &run).await {
                warn!(preview = %id, error = %e, "previews: stopping a replay's run on a copy");
            }
        }
        for super::main_on_change::Copy { b, parent, from, r#ref, kept, name } in made.copies {
            if let Err(e) = b.delete_snapshot(&parent, &from, &r#ref, kept.as_deref(), &name).await {
                warn!(preview = %id, error = %e, "previews: deleting a replay's copy");
            }
        }
    }
}

/// Where a replay of `r`'s requests copies data, and the copies' name: `r`'s own branches
/// (`<r>-w`), and main's preview's (`<r>-wm`), both named by the change's preview.
fn replay_copies<'a>(r: &'a Row, main: &'a Row) -> [(&'a Row, String); 2] {
    let base = branch_name(&r.id);
    [(r, format!("{base}-w")), (main, format!("{base}-wm"))]
}

/// A request's answers on both, compared: the status, and a JSON object answer's top-level fields.
fn compared(service: String, method: String, path: String, a: crate::ports::Fetched, b: crate::ports::Fetched) -> crate::ReplayedRequest {
    let (removed, added) = match (top_level_keys(&a), top_level_keys(&b)) {
        (Some(o), Some(m)) => (m.iter().filter(|k| !o.contains(k)).cloned().collect(), o.iter().filter(|k| !m.contains(k)).cloned().collect()),
        _ => (Vec::new(), Vec::new()),
    };
    crate::ReplayedRequest { service, method, path, status: a.status, main_status: b.status, removed, added }
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
