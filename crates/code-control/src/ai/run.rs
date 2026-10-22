//! Running reviewer jobs: reviews and `@roll` replies.

use super::store::{Job, UsageRow};
use super::{Chosen, human_author, month_of};
use crate::model::{Change, ChangeState, RepoRow};
use crate::review::{Origin, Reviewer, ReviewerKind};
use crate::{Caller, Code, Error, Result, cs_id, new_id};
use code_review::engine::{self, ContextBlock, Existing, ReviewResult};
use code_review::pricing::{self, Kind, KeySource, Month, PriceInput};
use code_review::prompt::{self, GUIDELINE_FILES, Learning, RepoPrompt};
use code_review::settings::{Classifier, ReviewerSettings};
use code_review::{Engine, FileInput, Mode, Partial, RiskInput, Skipped, Speed, Usage, glob, mode};
use code_store::{ChangesetId, FileStatus};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// New content of a changed file is read for verification excerpts up to this size.
const MAX_CONTENT: usize = 512 << 10;
/// Attempts a failing review gets before roll gives up on it.
const MAX_ATTEMPTS: u32 = 3;

/// How a review's mode was chosen.
struct Choice {
    mode: Mode,
    chosen_by: &'static str,
    reasons: Vec<String>,
    classifier: Option<(String, Usage)>,
}

impl Code {
    pub(crate) async fn run_jobs(self: Arc<Self>) {
        let limit = self.config.review.concurrency.max(1);
        let slots = Arc::new(tokio::sync::Semaphore::new(limit));
        loop {
            let free = slots.available_permits();
            if free > 0 {
                match self.db.due_ai_jobs(free) {
                    Ok(jobs) => {
                        for j in jobs {
                            match self.db.claim_ai_job(&j.id) {
                                Ok(true) => {}
                                _ => continue,
                            }
                            let Ok(permit) = slots.clone().acquire_owned().await else { return };
                            let me = self.clone();
                            tokio::spawn(async move {
                                me.run_job(j).await;
                                drop(permit);
                                me.ai.wake.notify_one();
                            });
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "listing reviewer jobs"),
                }
            }
            tokio::select! {
                _ = self.ai.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    }

    async fn run_job(&self, j: Job) {
        let res = match j.kind.as_str() {
            "review" => self.run_review_job(&j).await,
            _ => self.run_command_job(&j).await.map(|_| ("done", None)),
        };
        match res {
            Ok((state, review)) => {
                let _ = self.db.finish_ai_job(&j.id, state, None, review.as_deref());
            }
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(job = %j.id, change = %j.change_id, error = %msg, "roll's reviewer");
                let retryable = !matches!(e, Error::Invalid(_) | Error::InvalidComment(..) | Error::NotFound(_) | Error::Unprocessable(_) | Error::Forbidden(_));
                // `j` was read before the claim that counted this attempt.
                let attempt = j.attempts + 1;
                if retryable && attempt < MAX_ATTEMPTS {
                    let delay = self.config.review.retry_ms * 4i64.pow(attempt - 1);
                    let _ = self.db.retry_ai_job(&j.id, &msg, crate::db::now_ms() + delay);
                    return;
                }
                if j.kind != "review" {
                    self.command_gave_up(&j, &msg);
                }
                let _ = self.db.finish_ai_job(&j.id, "failed", Some(&msg), None);
                if j.kind == "review" {
                    self.review_gave_up(&j, &msg).await;
                }
            }
        }
    }

    /// A review that kept failing: say so on the change, and stop owing it.
    async fn review_gave_up(&self, j: &Job, msg: &str) {
        let Ok(c) = self.change_or_404(&j.change_id) else { return };
        let Ok(r) = self.repo_by_id(&c.repo_id) else { return };
        if !matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            let body = format!("**roll review** couldn't review this change: {msg}\n\nAsk again with `@roll review` once that's fixed.");
            let _ = self.submit_review(&r, &c, &Reviewer::roll(), &json!({ "verdict": "comment", "body": body }), &Origin::ROLL).await;
        }
        let _ = self.finish_review_request(&c, &Reviewer::roll());
    }

    /// A command (`@roll explain`, `fix`, a question) that failed: answer in its thread, so the
    /// person asking isn't left waiting.
    fn command_gave_up(&self, j: &Job, msg: &str) {
        let thread = j.command.as_ref().and_then(|c| c.get("thread")).and_then(Value::as_str).unwrap_or_default();
        let (Ok(c), Ok(Some(th))) = (self.change_or_404(&j.change_id), self.db.thread(thread)) else { return };
        let Ok(r) = self.repo_by_id(&c.repo_id) else { return };
        if let Err(e) = self.roll_reply(&r, &c, &th, &format!("I couldn't answer: {msg}")) {
            tracing::debug!(job = %j.id, error = %e, "replying to a failed command");
        }
    }

    /// Settle a review job without reviewing: `skipped`, and the change no longer waits on it.
    fn skip(&self, c: &Change) -> Result<(&'static str, Option<String>)> {
        self.finish_review_request(c, &Reviewer::roll())?;
        Ok(("skipped", None))
    }

    /// Record what a running job is doing.
    fn stage(&self, j: &Job, stage: &str, progress: Option<Value>) {
        if let Err(e) = self.db.set_ai_stage(&j.id, stage, progress.as_ref()) {
            tracing::debug!(job = %j.id, error = %e, "recording a review's stage");
        }
    }

    async fn run_review_job(&self, j: &Job) -> Result<(&'static str, Option<String>)> {
        let c = self.change_or_404(&j.change_id)?;
        let r = self.repo_by_id(&c.repo_id)?;
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            return Ok(("skipped", None));
        }
        let (settings, ..) = self.reviewer_settings(&r).await;
        let forced = j.mode.as_deref().and_then(Mode::parse);
        let head = self.store.resolve_or_404(&r.id, &c.head)?;
        let head_id = cs_id(&head);
        let reviews = self.reviews_view(&c)?;
        let last = reviews.iter().rev().find(|v| v.reviewer.kind == ReviewerKind::Roll && !v.dismissed && v.ai.is_some());
        if last.is_some_and(|v| v.head == head_id) && !j.full && forced.is_none() {
            return self.skip(&c);
        }
        let mb = self.merge_base(&r, &head)?;
        let Some(mb) = mb else { return self.skip(&c) };
        let all = self.store.compare(&r.id, &mb, &head)?;
        // Incremental: what changed since roll's last review, within the change's files.
        let mut incremental = false;
        let mut diffs = all.clone();
        if !j.full
            && let Some(prev) = last.and_then(|v| self.store.resolve(&r.id, &v.head).ok().flatten())
            && prev != head
        {
            let paths: std::collections::BTreeSet<String> = all.iter().map(|d| d.path.to_string()).collect();
            let inter: Vec<_> = self.store.compare(&r.id, &prev, &head)?.into_iter().filter(|d| paths.contains(d.path.as_str())).collect();
            if inter.is_empty() {
                return self.skip(&c);
            }
            diffs = inter;
            incremental = true;
        }
        let mut skipped = Vec::new();
        let mut files = Vec::new();
        for d in &diffs {
            let path = d.path.to_string();
            if glob::any(&settings.ignore, &path) {
                skipped.push(Skipped { path, reason: engine::IGNORED.into() });
                continue;
            }
            if files.len() >= settings.max_files as usize {
                skipped.push(Skipped { path, reason: format!("past the first {} files (max_files)", settings.max_files) });
                continue;
            }
            let patch = self.store.unified_diff(&r.id, d).await?;
            if patch.starts_with("Binary files") {
                skipped.push(Skipped { path, reason: "binary".into() });
                continue;
            }
            let status = match d.status {
                FileStatus::Added => "added",
                FileStatus::Modified => "modified",
                FileStatus::Deleted => "deleted",
            };
            let content = if d.status == FileStatus::Deleted {
                None
            } else {
                match self.store.read_file(&r.id, &head, &path).await? {
                    Some(f) if f.data.len() <= MAX_CONTENT => Some(String::from_utf8_lossy(&f.data).into_owned()),
                    _ => None,
                }
            };
            files.push(FileInput { path, status: status.into(), patch, content });
        }
        if files.is_empty() {
            // Only ignored (or binary) files changed: nothing to say.
            return self.skip(&c);
        }

        // Who pays, and how much may this cost.
        let human = human_author(self, &c);
        let public = Self::is_public(&r);
        let month = month_of(crate::db::now());
        let billing = self.db.billing(&r.owner_id)?;
        let mut chosen = self.choose_provider(&r.owner_id, false)?.ok_or_else(|| Error::Unprocessable("roll's reviewer has no model provider: set one for the owner (PUT /v1/code/owners/{owner}/review-provider) or on the server (ROLL_REVIEW_PROVIDER)".into()))?;

        // The mode.
        let cfg = self.review_config(&r).await;
        let mut choice = self.choose_mode(&r, &c, &settings, &files, forced, &cfg, &chosen).await;
        // Hosted spend is reserved before the model is called and released once the review is
        // metered, so reviews running at the same time can't overrun the owner's cap together.
        // The estimate prices every file sent; the review is metered on the files it read, and
        // with the author's human cap read again then, so it never costs more than it reserved.
        let mut reservation = None;
        if chosen.key == KeySource::Hosted {
            let spent_now = || {
                let (spent, free_used, _) = self.db.usage_month(&r.owner_id, &month, &c.author)?;
                Ok((spent, free_used))
            };
            let author_std = self.db.usage_month(&r.owner_id, &month, &c.author)?.2;
            let sent = files.len() as u32;
            let estimate = |mode: Mode| {
                move |free_used: u32| {
                    pricing::price(
                        &self.config.review.rates,
                        &price_input(Kind::Review, mode, KeySource::Hosted, sent, public, human),
                        &Month { public_free_used: free_used, author_standard_charged: author_std },
                    )
                }
            };
            match self.ai.reserve(&r.owner_id, billing.monthly_cap_usd, spent_now, estimate(choice.mode))? {
                Some(g) => reservation = Some(g),
                None => {
                    let cap = billing.monthly_cap_usd.unwrap_or_default();
                    match billing.on_cap.as_str() {
                        "byok_fallback" => match self.choose_provider(&r.owner_id, true)? {
                            Some(b) => chosen = b,
                            None => return self.capped(&r, &c, cap).await,
                        },
                        "standard_only" => {
                            if choice.mode != Mode::Standard {
                                choice.mode = Mode::Standard;
                                choice.reasons.push(format!("the monthly cap (${cap:.2}) is reached: standard only"));
                            }
                            // Standard reviews go on past the cap; still held, so the others see them.
                            reservation = self.ai.reserve(&r.owner_id, None, spent_now, estimate(Mode::Standard))?;
                        }
                        _ => return self.capped(&r, &c, cap).await,
                    }
                }
            }
        }
        let reservation = reservation;

        // The prompt.
        let default_head = self.head(&r)?;
        let mut guidelines = Vec::new();
        let mut prompt_file = None;
        if let Some(h) = default_head {
            for g in GUIDELINE_FILES {
                if let Some(f) = self.store.read_file(&r.id, &h, g).await? {
                    let t = String::from_utf8_lossy(&f.data).into_owned();
                    guidelines.push((g.to_string(), prompt::clip(&t, prompt::MAX_GUIDELINE).to_string()));
                }
            }
            if let Some(pf) = settings.prompt_file.as_deref()
                && let Some(f) = self.store.read_file(&r.id, &h, pf.trim_start_matches('/')).await?
            {
                prompt_file = Some(String::from_utf8_lossy(&f.data).into_owned());
            }
        }
        let changed: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let learnings: Vec<Learning> = self
            .db
            .learnings(&r.id)?
            .into_iter()
            .filter(|l| l.path.as_deref().is_none_or(|p| changed.iter().any(|c| glob::matches(p, c))))
            .map(|l| Learning { id: l.id, path: l.path, text: l.text })
            .collect();
        let system = prompt::system(&RepoPrompt { settings: &settings, prompt_file: prompt_file.as_deref(), guidelines: &guidelines, learnings: &learnings });
        let context = if choice.mode == Mode::Standard {
            Vec::new()
        } else {
            self.stage(j, "indexing", None);
            match self.deep_context(&r, &files, settings.speed).await {
                Some(blocks) => blocks,
                None => {
                    choice.reasons.push("the codebase index is still being built: reviewed without repository context".into());
                    Vec::new()
                }
            }
        };
        let existing: Vec<Existing> = self
            .db
            .threads(&c.id)?
            .into_iter()
            .filter(|t| !t.resolved && !t.outdated && t.comments.first().is_some_and(|x| x.author.kind == ReviewerKind::Roll))
            .map(|t| Existing { path: t.path.clone(), line: t.line, text: t.comments[0].body.clone() })
            .collect();
        let open_roll = existing.len();
        let input = code_review::ReviewInput {
            title: c.title.clone(),
            body: c.body.clone(),
            files,
            skipped,
            mode: choice.mode,
            speed: settings.speed,
            incremental,
            system,
            context,
            existing,
            settings: settings.clone(),
        };
        let engine = Engine::new(chosen.provider.clone(), chosen.secondary_model.clone());

        // Progress for `GET /ai-jobs`; `fast` also posts each group's findings as soon as it's done.
        self.stage(j, "chunking", None);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Partial>();
        let streaming = settings.speed == Speed::Fast;
        let streamed = Arc::new(parking_lot::Mutex::new(Vec::<engine::Finding>::new()));
        let poster = {
            let (r2, c2, streamed2) = (r.clone(), c.clone(), streamed.clone());
            let show = choice.mode != Mode::Standard;
            let me = self.me.upgrade();
            let job = j.clone();
            tokio::spawn(async move {
                let Some(me) = me else { return };
                while let Some(p) = rx.recv().await {
                    match p {
                        Partial::Groups(total) => me.stage(&job, "reviewing", Some(json!({ "groups_done": 0, "groups_total": total }))),
                        Partial::GroupDone(done, total) => me.stage(&job, "reviewing", Some(json!({ "groups_done": done, "groups_total": total }))),
                        Partial::Verifying => me.stage(&job, "verifying", None),
                        Partial::Findings(fs) if streaming => {
                            for f in fs {
                                let t = crate::review::ThreadInput {
                                    path: f.path.clone(),
                                    line: f.line,
                                    start_line: Some(f.start_line),
                                    side: Some(f.side.clone()),
                                    body: engine::render_finding(&f, show),
                                };
                                if me.create_thread(&r2, &c2, &Reviewer::roll(), &t, &Origin::ROLL).await.is_ok() {
                                    streamed2.lock().push(f);
                                }
                            }
                        }
                        Partial::Findings(_) => {}
                    }
                }
            })
        };
        let result = engine.review(&input, Some(tx)).await;
        let _ = poster.await;
        let result = match result {
            Ok(r) => r,
            Err(engine::EngineError::Nothing(why)) => {
                tracing::debug!(change = %c.id, why, "nothing to review");
                return self.skip(&c);
            }
            Err(engine::EngineError::Provider(e)) => {
                return Err(match e {
                    // The provider refused the request itself (a bad key, a bad request): asking
                    // again won't help. Rate limits and overload were already retried.
                    code_review::ProviderError::Rejected { status, .. } if (400..500).contains(&status) && status != 408 && status != 429 => Error::Unprocessable(e.to_string()),
                    code_review::ProviderError::Config(_) => Error::Unprocessable(e.to_string()),
                    _ => Error::Internal(anyhow::anyhow!(e.to_string())),
                });
            }
        };

        // The review: findings already streamed are threads already.
        let posted = streamed.lock().clone();
        let show_conf = choice.mode != Mode::Standard;
        let comments: Vec<Value> = result
            .findings
            .iter()
            .filter(|f| !posted.contains(f))
            .map(|f| json!({ "path": f.path, "line": f.line, "start_line": f.start_line, "side": f.side, "body": engine::render_finding(f, show_conf) }))
            .collect();
        self.stage(j, "posting", None);
        let header = header(&choice, settings.speed, &result, incremental);
        let mut body = engine::render_body(&result, &header);
        let (verdict, lowered) = engine::guard_approval(&result.verdict, &result.skipped, incremental, open_roll);
        if let Some(why) = lowered {
            body.push_str(&format!("\n\n_{why}_"));
        }
        let c = self.change_or_404(&c.id)?;
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            return Ok(("skipped", None));
        }
        let review = match self.submit_review(&r, &c, &Reviewer::roll(), &json!({ "verdict": verdict, "body": body, "comments": comments }), &Origin::ROLL).await {
            Ok(v) => v,
            // A comment the store won't place (its file moved on): post the review without them.
            Err(e @ (Error::Invalid(_) | Error::InvalidComment(..))) => {
                tracing::warn!(change = %c.id, error = %e, "posting roll's review comments");
                let body = format!("{body}\n\n_Some inline comments couldn't be placed: {e}_");
                self.submit_review(&r, &c, &Reviewer::roll(), &json!({ "verdict": verdict, "body": body }), &Origin::ROLL).await?
            }
            Err(e) => return Err(e),
        };

        // Meter it.
        let mut usage = result.total_usage();
        let mut models: Vec<(String, Usage)> = result.usage.iter().map(|m| (m.model.clone(), m.usage)).collect();
        if let Some((m, u)) = &choice.classifier {
            usage.add(u);
            models.push((m.clone(), *u));
        }
        let cost = models.iter().map(|(m, u)| pricing::provider_cost(m, u, &self.config.review.model_rates)).try_fold(0.0, |a, x| x.map(|x| a + x));
        // The month's counters again: other reviews may have been metered while this one ran.
        // A hosted review is free exactly when its reservation claimed a free public review.
        let (_, free_used, author_std) = self.db.usage_month(&r.owner_id, &month, &c.author)?;
        let free_used = match &reservation {
            Some(g) if g.free => 0,
            Some(_) => u32::MAX,
            None => free_used,
        };
        let price = pricing::price(
            &self.config.review.rates,
            &price_input(Kind::Review, choice.mode, chosen.key, result.files_reviewed.max(1), public, human),
            &Month { public_free_used: free_used, author_standard_charged: author_std },
        );
        let ai = json!({
            "mode": choice.mode.as_str(),
            "chosen_by": choice.chosen_by,
            "reasons": choice.reasons,
            "speed": settings.speed.as_str(),
            "incremental": incremental,
            "provider": chosen.name,
            "model": result.model,
            "key": chosen.key.as_str(),
            "tokens": { "input": usage.input_tokens, "output": usage.output_tokens, "cache_write": usage.cache_write_tokens, "cache_read": usage.cache_read_tokens },
            "price_usd": price.charged_usd,
            "list_price_usd": price.list_usd,
            "skipped_files": result.skipped,
            "details": engine::ai_summary(&result),
        });
        self.db.set_review_ai(&review.id, &ai)?;
        self.db.insert_usage(&UsageRow {
            id: new_id("use"),
            owner_id: r.owner_id.clone(),
            repo_id: r.id.clone(),
            change_id: Some(c.id.clone()),
            review_id: Some(review.id.clone()),
            kind: "review".into(),
            mode: choice.mode.as_str().into(),
            chosen_by: choice.chosen_by.into(),
            reasons: choice.reasons.clone(),
            speed: settings.speed.as_str().into(),
            provider: chosen.name.into(),
            model: result.model.clone(),
            key_source: chosen.key.as_str().into(),
            files_reviewed: result.files_reviewed,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            provider_cost_usd: cost,
            list_usd: price.list_usd,
            price_usd: price.charged_usd,
            free: price.free,
            capped: price.capped,
            human_author: human,
            author: c.author.clone(),
            public,
            month,
            created_at: crate::db::now(),
        })?;
        if let Ok(Some(v)) = self.db.review(&review.id) {
            self.emit(&r.id, "review", serde_json::to_value(&v).unwrap_or_default());
        }
        Ok(("done", Some(review.id)))
    }

    /// The monthly cap stopped a hosted review (`on_cap: pause`).
    async fn capped(&self, r: &RepoRow, c: &Change, cap: f64) -> Result<(&'static str, Option<String>)> {
        let body = format!(
            "**roll review** is paused for this month: the owner's AI review cap (${cap:.2}) is reached.\n\nRaise it (PUT /v1/code/owners/{}/billing), switch `on_cap` to `byok_fallback` or `standard_only`, or set your own provider key.",
            r.owner
        );
        let _ = self.submit_review(r, c, &Reviewer::roll(), &json!({ "verdict": "comment", "body": body }), &Origin::ROLL).await;
        self.finish_review_request(c, &Reviewer::roll())?;
        Ok(("skipped", None))
    }

    #[allow(clippy::too_many_arguments)]
    async fn choose_mode(
        &self,
        r: &RepoRow,
        c: &Change,
        settings: &ReviewerSettings,
        files: &[FileInput],
        forced: Option<Mode>,
        cfg: &crate::review::ReviewConfig,
        chosen: &Chosen,
    ) -> Choice {
        if let Some(m) = forced {
            return Choice { mode: m, chosen_by: "command", reasons: vec![format!("{} requested", m.as_str())], classifier: None };
        }
        if let Some(m) = settings.fixed_mode() {
            return Choice { mode: m, chosen_by: "settings", reasons: vec![format!("the repo reviews in {} mode", m.as_str())], classifier: None };
        }
        let mut lines = 0;
        for f in files {
            let a = prompt::annotate(&f.patch);
            lines += a.additions + a.deletions;
        }
        let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let codeowned = paths.iter().filter(|p| cfg.codeowners.rule_for(p).is_some_and(|x| !x.owners.is_empty())).count();
        let (symbols, _) = changed_symbols(files);
        let fanout = match self.ai.index.as_ref().and_then(|i| i.repo(&r.id).ok()) {
            Some(idx) => symbols.iter().filter_map(|s| idx.reference_count(s).ok()).max().unwrap_or(0),
            None => 0,
        };
        let (reverts, ejections) = self.db.author_history(&r.id, &c.author, crate::db::now() - 30 * 86_400).unwrap_or((0, 0));
        let labels = c.work_item.as_deref().and_then(|w| self.db.work(w).ok().flatten()).map(|w| w.labels).unwrap_or_default();
        let input = RiskInput {
            lines,
            paths: paths.clone(),
            codeowned,
            fanout,
            tests_changed: paths.iter().any(|p| code_index::is_test_path(p)),
            author_reverts: reverts,
            author_ejections: ejections,
            labels,
        };
        let (m, reasons) = mode::score(&input, &settings.auto);
        if settings.auto.classifier == Classifier::Model {
            let engine = Engine::new(chosen.provider.clone(), chosen.secondary_model.clone());
            let mut summary = format!("Title: {}\nLines changed: {lines}\nHeuristic: {} ({})\nFiles:\n", c.title, m.as_str(), reasons.join("; "));
            for f in files.iter().take(200) {
                let a = prompt::annotate(&f.patch);
                summary.push_str(&format!("- {} ({}, +{} -{})\n", f.path, f.status, a.additions, a.deletions));
            }
            if let Some((cm, why, model, usage)) = engine.classify(&[], &summary, settings).await {
                return Choice { mode: cm, chosen_by: "auto-model", reasons: why, classifier: Some((model, usage)) };
            }
        }
        Choice { mode: m, chosen_by: "auto-heuristic", reasons, classifier: None }
    }

    /// Repository context for a deep review; `None` while the repo's first index build is still
    /// running (the review goes on without it and says so).
    async fn deep_context(&self, r: &RepoRow, files: &[FileInput], speed: Speed) -> Option<Vec<ContextBlock>> {
        match self.index_ready(r, std::time::Duration::from_millis(self.config.review_index_wait_ms)).await {
            Ok(true) => {}
            Ok(false) => return None,
            Err(e) => tracing::warn!(repo = %r.full_name(), error = %e, "indexing for a deep review"),
        }
        Some(self.deep_blocks(r, files, speed).await)
    }

    /// Definitions, callers, related tests and similar code for a deep review.
    async fn deep_blocks(&self, r: &RepoRow, files: &[FileInput], speed: Speed) -> Vec<ContextBlock> {
        let Some(idx) = self.ai.index.as_ref().and_then(|i| i.repo(&r.id).ok()) else { return Vec::new() };
        let Ok(Some(head)) = self.head(r) else { return Vec::new() };
        let wide = speed == Speed::Thorough;
        let per = if wide { 8 } else { 4 };
        let changed: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let (symbols, biggest) = changed_symbols(files);
        let mut blocks = Vec::new();
        if let Ok(callers) = idx.callers_of(&symbols, &changed, per) {
            for (name, hits) in callers.into_iter().take(if wide { 20 } else { 10 }) {
                let mut text = String::new();
                for h in hits {
                    if let Some(snip) = self.snippet(r, &head, &h.path, &h.lines, 3).await {
                        text.push_str(&format!("{}:\n```\n{}```\n", h.path, snip));
                    }
                }
                if !text.is_empty() {
                    blocks.push(ContextBlock { title: format!("Uses of `{name}` outside this change"), text });
                }
            }
        }
        for name in symbols.iter().take(10) {
            if let Ok(defs) = idx.definitions(name, 3) {
                let others: Vec<_> = defs.into_iter().filter(|d| !changed.contains(&d.path)).collect();
                if !others.is_empty() {
                    let mut text = String::new();
                    for d in others {
                        if let Some(snip) = self.snippet(r, &head, &d.path, &[d.line], 6).await {
                            text.push_str(&format!("{} ({}):\n```\n{}```\n", d.path, d.kind, snip));
                        }
                    }
                    blocks.push(ContextBlock { title: format!("Other definitions of `{name}`"), text });
                }
            }
        }
        if let Ok(tests) = idx.related_tests(&changed, &symbols, per)
            && !tests.is_empty()
        {
            let text: String = tests.iter().map(|t| format!("- {t}\n")).collect();
            blocks.push(ContextBlock { title: "Tests related to the changed code".into(), text });
        }
        if let Some(snippet) = biggest
            && let Ok(sim) = idx.similar(&snippet, &changed, 3)
        {
            let sim: Vec<_> = sim.into_iter().filter(|(_, s)| *s >= 0.15).collect();
            if !sim.is_empty() {
                let text = sim.iter().map(|(p, s)| format!("- {p} (similarity {:.0}%)\n", s * 100.0)).collect::<String>();
                blocks.push(ContextBlock { title: "Similar code elsewhere (duplicated logic may need the same change)".into(), text });
            }
        }
        blocks
    }

    /// Lines around `lines` of a file at `at` (`ctx` each side), numbered.
    async fn snippet(&self, r: &RepoRow, at: &ChangesetId, path: &str, lines: &[u64], ctx: u64) -> Option<String> {
        let f = self.store.read_file(&r.id, at, path).await.ok()??;
        if f.data.len() > MAX_CONTENT {
            return None;
        }
        let text = String::from_utf8_lossy(&f.data);
        let all: Vec<&str> = text.lines().collect();
        let mut keep = std::collections::BTreeSet::new();
        for l in lines.iter().take(2) {
            for n in l.saturating_sub(ctx).max(1)..=(l + ctx).min(all.len() as u64) {
                keep.insert(n);
            }
        }
        let mut out = String::new();
        let mut prev = 0;
        for n in keep {
            if prev != 0 && n != prev + 1 {
                out.push_str("  …\n");
            }
            out.push_str(&format!("{n:>5} | {}\n", all.get(n as usize - 1).unwrap_or(&"")));
            prev = n;
        }
        Some(out)
    }

    // ── commands that need the model ──

    async fn run_command_job(&self, j: &Job) -> Result<()> {
        let cmd = j.command.clone().unwrap_or(Value::Null);
        let kind = cmd.get("command").and_then(Value::as_str).unwrap_or_default().to_string();
        let thread_id = cmd.get("thread").and_then(Value::as_str).unwrap_or_default().to_string();
        let by: Reviewer = serde_json::from_value(cmd.get("by").cloned().unwrap_or(Value::Null)).unwrap_or_else(|_| Reviewer::user("someone"));
        let c = self.change_or_404(&j.change_id)?;
        let r = self.repo_by_id(&c.repo_id)?;
        let th = self.db.thread(&thread_id)?.ok_or_else(|| Error::NotFound(format!("thread {thread_id}")))?;
        let (settings, ..) = self.reviewer_settings(&r).await;
        let mut chosen = match self.choose_provider(&r.owner_id, false)? {
            Some(c) => c,
            None => {
                self.roll_reply(&r, &c, &th, "I can't answer: roll's reviewer has no model provider configured for this owner.")?;
                return Ok(());
            }
        };
        // Chats count against the cap like reviews: reserved before the call, released after metering.
        let human = human_author(self, &c);
        let public = Self::is_public(&r);
        let month = month_of(crate::db::now());
        let mut reservation = None;
        if chosen.key == KeySource::Hosted {
            let billing = self.db.billing(&r.owner_id)?;
            let estimate = |_| pricing::price(&self.config.review.rates, &price_input(Kind::Chat, Mode::Standard, KeySource::Hosted, 0, public, human), &Month::default());
            let spent_now = || {
                let (spent, free_used, _) = self.db.usage_month(&r.owner_id, &month, &c.author)?;
                Ok((spent, free_used))
            };
            match self.ai.reserve(&r.owner_id, billing.monthly_cap_usd, spent_now, estimate)? {
                Some(g) => reservation = Some(g),
                None => {
                    let cap = billing.monthly_cap_usd.unwrap_or_default();
                    match (billing.on_cap.as_str(), self.choose_provider(&r.owner_id, true)?) {
                        ("byok_fallback", Some(b)) => chosen = b,
                        _ => {
                            self.roll_reply(&r, &c, &th, &format!("I can't answer: the owner's monthly AI review cap (${cap:.2}) is reached."))?;
                            return Ok(());
                        }
                    }
                }
            }
        }
        let _reservation = reservation;
        // The thread's code, its conversation and the question.
        let head = self.store.resolve_or_404(&r.id, &c.head)?;
        let at = self.store.resolve(&r.id, &th.at)?.unwrap_or(head);
        let code = self.snippet(&r, &at, &th.path, &[th.start_line, th.line], 12).await.unwrap_or_default();
        let mut convo = String::new();
        for cm in &th.comments {
            convo.push_str(&format!("**{}**: {}\n\n", cm.author.display(), cm.body.trim()));
        }
        let task = match kind.as_str() {
            "fix" => format!(
                "# Task\n\nWrite a fix for the problem discussed in this thread: the exact replacement text for lines {}..{} of {} \
                 in `suggestion` (only those lines), and a one-paragraph `reply` explaining it.\n\n# Code ({})\n\n```\n{code}```\n\n# Thread\n\n{convo}",
                th.start_line, th.line, th.path, th.path
            ),
            "explain" => format!("# Task\n\nExplain the finding in this thread in more detail: why it's a problem, when it happens, and how to fix it. `suggestion` is null unless a small fix for lines {}..{} is obvious.\n\n# Code ({})\n\n```\n{code}```\n\n# Thread\n\n{convo}", th.start_line, th.line, th.path),
            _ => format!(
                "# Task\n\nAnswer the last question in this thread about the change \"{}\", briefly and concretely. `suggestion` is null unless they asked for code for lines {}..{}.\n\n# Code ({})\n\n```\n{code}```\n\n# Thread\n\n{convo}",
                c.title, th.start_line, th.line, th.path
            ),
        };
        let learnings: Vec<Learning> = self.db.learnings(&r.id)?.into_iter().map(|l| Learning { id: l.id, path: l.path, text: l.text }).collect();
        let system = prompt::system(&RepoPrompt { settings: &settings, prompt_file: None, guidelines: &[], learnings: &learnings });
        let engine = Engine::new(chosen.provider.clone(), chosen.secondary_model.clone());
        let (reply, suggestion, model, usage) = engine.reply(&system, &task, &settings).await.map_err(|e| match e {
            code_review::ProviderError::Rejected { .. } | code_review::ProviderError::Config(_) => Error::Unprocessable(e.to_string()),
            e => Error::Internal(anyhow::anyhow!(e.to_string())),
        })?;
        let body = match (&suggestion, kind.as_str()) {
            (Some(s), _) => format!("{}\n\n```suggestion\n{}\n```", reply.trim(), s.trim_end_matches('\n')),
            (None, _) => reply.trim().to_string(),
        };
        let th = self.roll_reply(&r, &c, &th, &body)?;
        if kind == "fix" && suggestion.is_some() {
            let c = self.change_or_404(&c.id)?;
            let cm = th.comments.last().map(|x| x.id.clone());
            match self.apply_suggestion(&r, &Caller::Admin, &by, &c, &th, cm.as_deref()).await {
                Ok(_) => {}
                Err(e) => {
                    let th = self.db.thread(&th.id)?.ok_or_else(|| Error::NotFound(th.id.clone()))?;
                    self.roll_reply(&r, &c, &th, &format!("I couldn't apply it: {e}"))?;
                }
            }
        }
        // Meter the reply.
        let price = pricing::price(&self.config.review.rates, &price_input(Kind::Chat, Mode::Standard, chosen.key, 0, public, human), &Month::default());
        let cost = pricing::provider_cost(&model, &usage, &self.config.review.model_rates);
        self.db.insert_usage(&UsageRow {
            id: new_id("use"),
            owner_id: r.owner_id.clone(),
            repo_id: r.id.clone(),
            change_id: Some(c.id.clone()),
            review_id: None,
            kind: "chat".into(),
            mode: "standard".into(),
            chosen_by: kind.clone(),
            reasons: Vec::new(),
            speed: settings.speed.as_str().into(),
            provider: chosen.name.into(),
            model,
            key_source: chosen.key.as_str().into(),
            files_reviewed: 0,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            provider_cost_usd: cost,
            list_usd: price.list_usd,
            price_usd: price.charged_usd,
            free: false,
            capped: false,
            human_author: human,
            author: c.author.clone(),
            public,
            month,
            created_at: crate::db::now(),
        })?;
        Ok(())
    }
}

fn price_input(kind: Kind, mode: Mode, key: KeySource, files: u32, public: bool, human: bool) -> PriceInput {
    PriceInput { kind, mode, key, files, public_repo: public, human_author: human }
}

/// Symbols defined on added lines of the changed files, and the biggest added block (for
/// `similar`).
fn changed_symbols(files: &[FileInput]) -> (Vec<String>, Option<String>) {
    let mut names = std::collections::BTreeSet::new();
    let mut biggest: Option<String> = None;
    for f in files {
        let Some(content) = &f.content else { continue };
        let a = prompt::annotate(&f.patch);
        let facts = code_index::extract::extract(&f.path, content);
        for s in facts.symbols {
            if a.added.contains(&s.line) {
                names.insert(s.name);
            }
        }
        // The longest run of added lines.
        let lines: Vec<&str> = content.lines().collect();
        let mut run: Vec<u64> = Vec::new();
        let mut best: Vec<u64> = Vec::new();
        for l in &a.added {
            if run.last().is_some_and(|p| p + 1 != *l) {
                run.clear();
            }
            run.push(*l);
            if run.len() > best.len() {
                best = run.clone();
            }
        }
        if best.len() >= 3 {
            let text: String = best.iter().filter_map(|l| lines.get(*l as usize - 1)).map(|l| format!("{l}\n")).collect();
            if biggest.as_ref().is_none_or(|b| text.len() > b.len()) {
                biggest = Some(text);
            }
        }
    }
    (names.into_iter().take(30).collect(), biggest)
}

fn header(choice: &Choice, speed: Speed, r: &ReviewResult, incremental: bool) -> String {
    let mut h = format!("**roll review** · {} mode", choice.mode.as_str());
    match choice.chosen_by {
        "auto-heuristic" | "auto-model" => h.push_str(&format!(" (auto: {})", choice.reasons.join("; "))),
        _ => {}
    }
    h.push_str(&format!(" · {} · {}", speed.as_str(), r.model));
    if incremental {
        h.push_str(" · changes since the last review");
    }
    h
}
