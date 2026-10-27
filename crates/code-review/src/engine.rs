//! One review: the diff is split into file groups that fit the model's budget (measured with
//! the provider's token counter when it has one, never silently truncated — whatever can't be
//! reviewed is listed in [`ReviewResult::skipped`]), the groups run with bounded concurrency,
//! findings are validated against the lines the diff shows, deduplicated against open threads,
//! verified (`max`, `thorough`) and turned into a verdict by the profile.

use crate::mode::{Mode, Speed};
use crate::prompt::{self, Annotated};
use crate::provider::{Effort, Provider, ProviderError, Request, Stop, SystemBlock, Usage};
use crate::schema;
use crate::settings::{Profile, ReviewerSettings};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Arc;
use tokio::sync::mpsc;

/// One changed file.
#[derive(Clone, Debug, Default)]
pub struct FileInput {
    pub path: String,
    /// `added`, `modified` or `deleted`.
    pub status: String,
    /// The unified diff.
    pub patch: String,
    /// The new version (for verification excerpts); `None` when deleted.
    pub content: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

/// Repository context for `deep` and `max`: definitions, callers, related tests, similar code.
#[derive(Clone, Debug)]
pub struct ContextBlock {
    pub title: String,
    pub text: String,
}

/// An open thread roll already left, so a re-review doesn't repeat it.
#[derive(Clone, Debug)]
pub struct Existing {
    pub path: String,
    pub line: u64,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct ReviewInput {
    pub title: String,
    pub body: String,
    pub files: Vec<FileInput>,
    /// Files the caller already left out (ignored, binary, past `max_files`).
    pub skipped: Vec<Skipped>,
    pub mode: Mode,
    pub speed: Speed,
    /// Only what changed since roll's last review of the change.
    pub incremental: bool,
    pub system: Vec<SystemBlock>,
    pub context: Vec<ContextBlock>,
    pub existing: Vec<Existing>,
    pub settings: ReviewerSettings,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Finding {
    pub path: String,
    pub side: String,
    pub start_line: u64,
    pub line: u64,
    pub severity: String,
    pub title: String,
    pub body: String,
    pub suggestion: Option<String>,
    pub confidence: f64,
}

/// Streamed while a review runs: progress, and each group's findings as it finishes.
#[derive(Clone, Debug)]
pub enum Partial {
    /// A group's findings, as soon as the group is done.
    Findings(Vec<Finding>),
    /// The diff was split into this many groups (one model request each, at least).
    Groups(usize),
    /// Another group finished: `(done, total)`.
    GroupDone(usize, usize),
    /// The verification pass started (`max`, `thorough`).
    Verifying,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct ModelUsage {
    pub model: String,
    pub calls: u32,
    pub usage: Usage,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ReviewResult {
    pub walkthrough: String,
    pub files: Vec<(String, String)>,
    pub diagram: Option<String>,
    pub findings: Vec<Finding>,
    pub notes: Vec<String>,
    pub skipped: Vec<Skipped>,
    /// `approve`, `request_changes` or `comment`.
    pub verdict: String,
    pub usage: Vec<ModelUsage>,
    pub requests: u32,
    pub dropped_invalid: u32,
    pub dropped_duplicates: u32,
    pub dropped_unverified: u32,
    pub dropped_low_confidence: u32,
    /// Files actually reviewed.
    pub files_reviewed: u32,
    pub effort: Option<Effort>,
    pub model: String,
}

impl ReviewResult {
    pub fn total_usage(&self) -> Usage {
        let mut u = Usage::default();
        for m in &self.usage {
            u.add(&m.usage);
        }
        u
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("nothing could be reviewed: {0}")]
    Nothing(String),
}

/// Budgets.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Diff tokens per request (`balanced`); `fast` uses 40%, `thorough` 150%.
    pub group_tokens: u64,
    /// A file whose diff alone is bigger isn't reviewed.
    pub max_file_tokens: u64,
    pub max_output_tokens: u32,
    /// Requests in flight per review: `fast`, then the others.
    pub fast_concurrency: usize,
    pub concurrency: usize,
    /// Repository context, in bytes (`thorough` doubles it).
    pub context_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { group_tokens: 60_000, max_file_tokens: 150_000, max_output_tokens: 16_000, fast_concurrency: 4, concurrency: 2, context_bytes: 48 << 10 }
    }
}

pub struct Engine {
    pub provider: Arc<dyn Provider>,
    /// For `fast` and the classifier; `None`: the provider's own model.
    pub secondary_model: Option<String>,
    pub limits: Limits,
}

struct Group {
    files: Vec<(FileInput, Annotated)>,
}

struct GroupOut {
    walkthrough: String,
    files: Vec<(String, String)>,
    diagram: Option<String>,
    findings: Vec<Finding>,
    notes: Vec<String>,
    skipped: Vec<Skipped>,
    usage: Vec<(String, Usage)>,
    requests: u32,
    dropped_invalid: u32,
    dropped_duplicates: u32,
    error: Option<ProviderError>,
}

/// A rough token count for text the provider can't count (about 3 bytes per token for code).
pub fn estimate_tokens(s: &str) -> u64 {
    (s.len() as u64).div_ceil(3)
}

impl Engine {
    pub fn new(provider: Arc<dyn Provider>, secondary_model: Option<String>) -> Engine {
        Engine { provider, secondary_model, limits: Limits::default() }
    }

    fn model_for(&self, speed: Speed, settings: &ReviewerSettings) -> String {
        match speed {
            Speed::Fast => settings.secondary_model.clone().or_else(|| self.secondary_model.clone()).unwrap_or_default(),
            _ => settings.model.clone().unwrap_or_default(),
        }
    }

    pub fn effort_for(mode: Mode, speed: Speed, settings: &ReviewerSettings) -> Effort {
        if let Some(e) = settings.effort {
            return e;
        }
        match (speed, mode) {
            (Speed::Fast, _) => Effort::Low,
            (Speed::Thorough, _) => Effort::Xhigh,
            (Speed::Balanced, Mode::Standard) => Effort::Medium,
            (Speed::Balanced, _) => Effort::High,
        }
    }

    fn group_budget(&self, speed: Speed) -> u64 {
        match speed {
            Speed::Fast => self.limits.group_tokens * 2 / 5,
            Speed::Balanced => self.limits.group_tokens,
            Speed::Thorough => self.limits.group_tokens * 3 / 2,
        }
    }

    /// Review a change. `partial` gets each group's findings as it finishes (`fast`).
    pub async fn review(&self, input: &ReviewInput, partial: Option<mpsc::UnboundedSender<Partial>>) -> Result<ReviewResult, EngineError> {
        let mode = input.mode;
        let speed = input.speed;
        let settings = &input.settings;
        let model = self.model_for(speed, settings);
        let effort = Self::effort_for(mode, speed, settings);
        let diagram = settings.diagram && speed != Speed::Fast;
        let mut out = ReviewResult { skipped: input.skipped.clone(), effort: Some(effort), ..Default::default() };

        // Annotate and size every file; pack them into groups.
        let budget = self.group_budget(speed);
        let mut groups: Vec<Group> = Vec::new();
        let mut cur = Group { files: Vec::new() };
        let mut cur_tokens = 0;
        for f in &input.files {
            let a = prompt::annotate(&f.patch);
            if a.text.trim().is_empty() {
                out.skipped.push(Skipped { path: f.path.clone(), reason: "no textual changes".into() });
                continue;
            }
            let t = estimate_tokens(&a.text) + 30;
            if t > self.limits.max_file_tokens {
                out.skipped.push(Skipped { path: f.path.clone(), reason: format!("too large to review (about {t} tokens of diff)") });
                continue;
            }
            if !cur.files.is_empty() && cur_tokens + t > budget {
                groups.push(std::mem::replace(&mut cur, Group { files: Vec::new() }));
                cur_tokens = 0;
            }
            cur_tokens += t;
            cur.files.push((f.clone(), a));
        }
        if !cur.files.is_empty() {
            groups.push(cur);
        }
        if groups.is_empty() {
            return Err(EngineError::Nothing(if out.skipped.is_empty() { "the change has no files".into() } else { "every file was skipped".into() }));
        }

        let total = groups.len();
        if let Some(tx) = &partial {
            let _ = tx.send(Partial::Groups(total));
        }
        let done = std::sync::atomic::AtomicUsize::new(0);
        let context = render_context(&input.context, if speed == Speed::Thorough { self.limits.context_bytes * 2 } else { self.limits.context_bytes }, mode);
        let ctx = Ctx { input, model: &model, effort, diagram, context: &context, budget };
        let concurrency = if speed == Speed::Fast { self.limits.fast_concurrency } else { self.limits.concurrency }.max(1);
        let mut results: Vec<(usize, GroupOut)> = futures::stream::iter(groups.into_iter().enumerate())
            .map(|(i, g)| {
                let ctx = &ctx;
                let partial = partial.clone();
                let done = &done;
                async move {
                    let mut o = self.run_group(ctx, g, 0).await;
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    // Within a review, drop what's already open, then stream.
                    let (kept, dups) = dedupe(std::mem::take(&mut o.findings), &ctx.input.existing);
                    o.findings = kept;
                    o.dropped_duplicates += dups;
                    if let Some(tx) = &partial {
                        if !o.findings.is_empty() {
                            let _ = tx.send(Partial::Findings(o.findings.clone()));
                        }
                        let _ = tx.send(Partial::GroupDone(n, total));
                    }
                    (i, o)
                }
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;
        results.sort_by_key(|(i, _)| *i);

        let mut walk = Vec::new();
        let mut errors = Vec::new();
        let mut usage: BTreeMap<String, ModelUsage> = BTreeMap::new();
        let mut reviewed_groups = 0;
        for (_, o) in results {
            if o.error.is_none() {
                reviewed_groups += 1;
            }
            if let Some(e) = o.error {
                errors.push(e);
            }
            if !o.walkthrough.trim().is_empty() {
                walk.push(o.walkthrough);
            }
            out.files.extend(o.files);
            if out.diagram.is_none() {
                out.diagram = o.diagram.filter(|d| !d.trim().is_empty());
            }
            out.findings.extend(o.findings);
            out.notes.extend(o.notes);
            out.skipped.extend(o.skipped);
            out.requests += o.requests;
            out.dropped_invalid += o.dropped_invalid;
            out.dropped_duplicates += o.dropped_duplicates;
            for (m, u) in o.usage {
                let e = usage.entry(m.clone()).or_insert_with(|| ModelUsage { model: m, ..Default::default() });
                e.calls += 1;
                e.usage.add(&u);
            }
        }
        out.usage = usage.into_values().collect();
        if reviewed_groups == 0 {
            // Every group failed: a bad key or an outage is an error, not an empty review.
            if let Some(e) = errors.into_iter().next() {
                return Err(e.into());
            }
            return Err(EngineError::Nothing("every file was skipped".into()));
        }
        out.walkthrough = match walk.len() {
            0 => String::new(),
            1 => walk.remove(0),
            _ => walk.join("\n\n"),
        };
        let (kept, dups) = dedupe(std::mem::take(&mut out.findings), &[]);
        out.findings = kept;
        out.dropped_duplicates += dups;
        // Deep and max report confidence: drop what the model itself doubts.
        if mode != Mode::Standard {
            let before = out.findings.len();
            out.findings.retain(|f| f.confidence >= 0.3);
            out.dropped_low_confidence = (before - out.findings.len()) as u32;
        }
        if settings.profile == Profile::Chill {
            out.findings.retain(|f| f.severity != "nit");
        }
        if (mode == Mode::Max || speed == Speed::Thorough) && speed != Speed::Fast && !out.findings.is_empty() {
            if let Some(tx) = &partial {
                let _ = tx.send(Partial::Verifying);
            }
            self.verify(&ctx, &mut out).await;
        }
        out.verdict = verdict(&out.findings, settings.profile).to_string();
        out.model = if model.is_empty() { self.provider.model().to_string() } else { model };
        // Everything the engine skipped came from `input.files`.
        out.files_reviewed = input.files.len().saturating_sub(out.skipped.len().saturating_sub(input.skipped.len())) as u32;
        Ok(out)
    }

    fn request(&self, ctx: &Ctx, files: &[(FileInput, Annotated)], max_tokens: u32) -> Request {
        let input = ctx.input;
        let mut user = prompt::task(input.mode, ctx.diagram, input.incremental);
        let _ = write!(user, "\n# Change\n\nTitle: {}\n", input.title.trim());
        if !input.body.trim().is_empty() {
            let _ = write!(user, "\nDescription:\n{}\n", prompt::clip(input.body.trim(), 8 << 10));
        }
        if !ctx.context.is_empty() {
            user.push_str("\n# Repository context\n\n");
            user.push_str(ctx.context);
        }
        user.push_str("\n# Diff\n");
        for (f, a) in files {
            let _ = write!(user, "\n## {} ({}, +{} -{})\n\n```\n{}```\n", f.path, f.status, a.additions, a.deletions, a.text);
        }
        Request {
            model: ctx.model.to_string(),
            system: input.system.clone(),
            user,
            schema: schema::review(),
            schema_name: "review",
            effort: ctx.effort,
            max_tokens,
        }
    }

    /// Review one group. Too big for the budget → split; cut off → split (or skip one file);
    /// declined → its files are skipped with the reason.
    fn run_group<'a>(&'a self, ctx: &'a Ctx<'a>, g: Group, depth: u32) -> futures::future::BoxFuture<'a, GroupOut> {
        Box::pin(async move {
            let mut out = GroupOut {
                walkthrough: String::new(),
                files: Vec::new(),
                diagram: None,
                findings: Vec::new(),
                notes: Vec::new(),
                skipped: Vec::new(),
                usage: Vec::new(),
                requests: 0,
                dropped_invalid: 0,
                dropped_duplicates: 0,
                error: None,
            };
            let max_out = self.limits.max_output_tokens;
            let req = self.request(ctx, &g.files, max_out);
            // Measure when the provider can; split what doesn't fit.
            {
                match self.provider.count_tokens(&req).await {
                    Ok(Some(n)) => {
                        let system: u64 = ctx.input.system.iter().map(|b| estimate_tokens(&b.text)).sum();
                        let diff = n.saturating_sub(system);
                        if diff > ctx.budget * 3 / 2 {
                            if g.files.len() > 1 {
                                return self.split(ctx, g, depth).await;
                            }
                            if diff > self.limits.max_file_tokens {
                                out.skipped.push(Skipped { path: g.files[0].0.path.clone(), reason: format!("too large to review ({n} tokens)") });
                                return out;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::debug!(error = %e, "counting tokens failed; using the estimate"),
                }
            }
            out.requests += 1;
            let resp = match self.provider.complete(&req).await {
                Ok(r) => r,
                Err(e) => {
                    for (f, _) in &g.files {
                        out.skipped.push(Skipped { path: f.path.clone(), reason: format!("not reviewed: {e}") });
                    }
                    out.error = Some(e);
                    return out;
                }
            };
            out.usage.push((if resp.model.is_empty() { req.model.clone() } else { resp.model.clone() }, resp.usage));
            match resp.stop {
                Stop::EndTurn => {}
                Stop::MaxTokens if g.files.len() > 1 && depth < 6 => {
                    let mut more = self.split(ctx, g, depth).await;
                    more.usage.extend(out.usage);
                    more.requests += out.requests;
                    return more;
                }
                Stop::MaxTokens => {
                    out.skipped.push(Skipped { path: g.files[0].0.path.clone(), reason: "the review of this file didn't fit in the model's output".into() });
                    return out;
                }
                Stop::Refusal { category } => {
                    let why = match category {
                        Some(c) => format!("declined by the model ({c})"),
                        None => "declined by the model".into(),
                    };
                    for (f, _) in &g.files {
                        out.skipped.push(Skipped { path: f.path.clone(), reason: why.clone() });
                    }
                    return out;
                }
                Stop::Other(s) => {
                    for (f, _) in &g.files {
                        out.skipped.push(Skipped { path: f.path.clone(), reason: format!("the model stopped early ({s})") });
                    }
                    return out;
                }
            }
            let Some(v) = resp.json else { return out };
            out.walkthrough = v.get("walkthrough").and_then(Value::as_str).unwrap_or_default().to_string();
            out.diagram = v.get("diagram").and_then(Value::as_str).map(str::to_string);
            if let Some(files) = v.get("files").and_then(Value::as_array) {
                for f in files {
                    let (Some(p), Some(s)) = (f.get("path").and_then(Value::as_str), f.get("summary").and_then(Value::as_str)) else { continue };
                    if g.files.iter().any(|(x, _)| x.path == p) {
                        out.files.push((p.to_string(), s.to_string()));
                    }
                }
            }
            if let Some(notes) = v.get("notes").and_then(Value::as_array) {
                out.notes.extend(notes.iter().filter_map(Value::as_str).map(str::to_string));
            }
            for f in v.get("findings").and_then(Value::as_array).into_iter().flatten() {
                match validate(f, &g.files) {
                    Ok(f) => out.findings.push(f),
                    Err(note) => {
                        out.dropped_invalid += 1;
                        if let Some(n) = note {
                            out.notes.push(n);
                        }
                    }
                }
            }
            out
        })
    }

    async fn split(&self, ctx: &Ctx<'_>, mut g: Group, depth: u32) -> GroupOut {
        let back = g.files.split_off(g.files.len() / 2);
        let (mut a, b) = futures::join!(self.run_group(ctx, g, depth + 1), self.run_group(ctx, Group { files: back }, depth + 1));
        a.walkthrough = [a.walkthrough, b.walkthrough].into_iter().filter(|w| !w.trim().is_empty()).collect::<Vec<_>>().join("\n\n");
        a.files.extend(b.files);
        a.diagram = a.diagram.or(b.diagram);
        a.findings.extend(b.findings);
        a.notes.extend(b.notes);
        a.skipped.extend(b.skipped);
        a.usage.extend(b.usage);
        a.requests += b.requests;
        a.dropped_invalid += b.dropped_invalid;
        // The pair failed only when both halves did (otherwise the failed half's files are
        // listed as skipped).
        a.error = match (a.error, b.error) {
            (Some(x), Some(_)) => Some(x),
            _ => None,
        };
        a.dropped_duplicates += b.dropped_duplicates;
        a
    }

    /// `max` / `thorough`: re-check every finding against the code around it; drop the ones
    /// the model can't confirm.
    async fn verify(&self, ctx: &Ctx<'_>, out: &mut ReviewResult) {
        let input = ctx.input;
        let mut user = String::from(
            "# Task\n\nYou are checking another reviewer's findings. For each finding, read the code and decide whether it is a \
             real problem. Answer with one result per finding index. `confirmed` is false when the finding is wrong, \
             speculative or already handled by the surrounding code.\n\n# Findings\n",
        );
        for (i, f) in out.findings.iter().enumerate() {
            let _ = write!(user, "\n## {i}: {} ({}:{}-{}, {} side, {})\n\n{}\n", f.title, f.path, f.start_line, f.line, f.side, f.severity, f.body);
            if let Some(content) = input.files.iter().find(|x| x.path == f.path).and_then(|x| x.content.as_deref())
                && f.side == "new"
            {
                let lines: Vec<&str> = content.lines().collect();
                let from = (f.start_line as usize).saturating_sub(16);
                let to = (f.line as usize + 15).min(lines.len());
                user.push_str("\n```\n");
                for (n, l) in lines.iter().enumerate().take(to).skip(from) {
                    let _ = writeln!(user, "{:>5} | {l}", n + 1);
                }
                user.push_str("```\n");
            }
        }
        if !ctx.context.is_empty() {
            user.push_str("\n# Repository context\n\n");
            user.push_str(ctx.context);
        }
        let req = Request {
            model: ctx.model.to_string(),
            system: input.system.clone(),
            user,
            schema: schema::verify(),
            schema_name: "verify",
            effort: ctx.effort,
            max_tokens: self.limits.max_output_tokens,
        };
        out.requests += 1;
        match self.provider.complete(&req).await {
            Ok(r) => {
                let m = if r.model.is_empty() { self.provider.model().to_string() } else { r.model.clone() };
                match out.usage.iter_mut().find(|u| u.model == m) {
                    Some(u) => {
                        u.calls += 1;
                        u.usage.add(&r.usage);
                    }
                    None => out.usage.push(ModelUsage { model: m, calls: 1, usage: r.usage }),
                }
                let Some(v) = r.json.filter(|_| r.stop == Stop::EndTurn) else {
                    out.notes.push("The verification pass didn't finish; findings are unverified.".into());
                    return;
                };
                let confirmed: std::collections::BTreeSet<usize> = v
                    .get("results")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|x| x.get("confirmed").and_then(Value::as_bool) == Some(true))
                    .filter_map(|x| x.get("index").and_then(Value::as_u64).map(|i| i as usize))
                    .collect();
                let before = out.findings.len();
                let mut i = 0;
                out.findings.retain(|_| {
                    let keep = confirmed.contains(&i);
                    i += 1;
                    keep
                });
                out.dropped_unverified = (before - out.findings.len()) as u32;
            }
            Err(e) => out.notes.push(format!("The verification pass failed ({e}); findings are unverified.")),
        }
    }

    /// `classifier: model`: one small call that picks a mode. `None` when it fails (the caller
    /// keeps the heuristic's choice).
    pub async fn classify(&self, system: &[SystemBlock], summary: &str, settings: &ReviewerSettings) -> Option<(Mode, Vec<String>, String, Usage)> {
        let user = format!(
            "# Task\n\nDecide how deep a code review this change needs: `standard` (line-by-line), `deep` (needs \
             repository context: callers, invariants, tests) or `max` (high risk: security, data loss, concurrency, \
             money). Give short reasons.\n\n{summary}"
        );
        let req = Request {
            model: settings.secondary_model.clone().or_else(|| self.secondary_model.clone()).unwrap_or_default(),
            system: system.to_vec(),
            user,
            schema: schema::classify(),
            schema_name: "classify",
            effort: Effort::Low,
            max_tokens: 2000,
        };
        let r = self.provider.complete(&req).await.ok()?;
        let v = r.json.filter(|_| r.stop == Stop::EndTurn)?;
        let mode = Mode::parse(v.get("mode")?.as_str()?)?;
        let reasons = v.get("reasons")?.as_array()?.iter().filter_map(Value::as_str).map(str::to_string).collect();
        let model = if r.model.is_empty() { req.model.clone() } else { r.model };
        Some((mode, reasons, model, r.usage))
    }

    /// Answer in a thread: `@roll explain`, a question, or `@roll fix` (a suggestion for the
    /// thread's lines).
    pub async fn reply(&self, system: &[SystemBlock], task: &str, settings: &ReviewerSettings) -> Result<(String, Option<String>, String, Usage), ProviderError> {
        let req = Request {
            model: settings.model.clone().unwrap_or_default(),
            system: system.to_vec(),
            user: task.to_string(),
            schema: schema::reply(),
            schema_name: "reply",
            effort: settings.effort.unwrap_or(Effort::Medium),
            max_tokens: 8000,
        };
        let r = self.provider.complete(&req).await?;
        let model = if r.model.is_empty() { self.provider.model().to_string() } else { r.model.clone() };
        match (&r.stop, r.json) {
            (Stop::EndTurn, Some(v)) => Ok((
                v.get("reply").and_then(Value::as_str).unwrap_or_default().to_string(),
                v.get("suggestion").and_then(Value::as_str).map(str::to_string),
                model,
                r.usage,
            )),
            (Stop::Refusal { .. }, _) => Ok(("I can't help with that one.".into(), None, model, r.usage)),
            _ => Ok(("I couldn't finish an answer; try asking a narrower question.".into(), None, model, r.usage)),
        }
    }
}

struct Ctx<'a> {
    input: &'a ReviewInput,
    model: &'a str,
    effort: Effort,
    diagram: bool,
    context: &'a str,
    budget: u64,
}

fn render_context(blocks: &[ContextBlock], max: usize, mode: Mode) -> String {
    if mode == Mode::Standard {
        return String::new();
    }
    let mut s = String::new();
    for b in blocks {
        let piece = format!("## {}\n\n{}\n\n", b.title, b.text.trim());
        if s.len() + piece.len() > max {
            let room = max.saturating_sub(s.len());
            if room > 200 {
                s.push_str(prompt::clip(&piece, room));
                s.push_str("\n…\n");
            }
            break;
        }
        s.push_str(&piece);
    }
    s
}

/// Check a finding against the lines its file's diff shows. `Err(Some(note))`: kept as a note.
fn validate(v: &Value, files: &[(FileInput, Annotated)]) -> Result<Finding, Option<String>> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let path = s("path");
    let title = s("title");
    let body = s("body");
    let note = || Some(format!("{path}: {title} — {body}").trim().to_string());
    let Some((_, a)) = files.iter().find(|(f, _)| f.path == path) else { return Err(note()) };
    let side = match s("side").as_str() {
        "old" => "old",
        _ => "new",
    };
    let line = v.get("line").and_then(Value::as_u64).unwrap_or(0);
    let mut start = v.get("start_line").and_then(Value::as_u64).unwrap_or(line);
    let lines = if side == "new" { &a.new_lines } else { &a.old_lines };
    if !lines.contains(&line) {
        return Err(note());
    }
    if start == 0 || start > line || !lines.contains(&start) {
        start = line;
    }
    let severity = s("severity");
    if !schema::SEVERITIES.contains(&severity.as_str()) || title.trim().is_empty() {
        return Err(None);
    }
    let suggestion = v
        .get("suggestion")
        .and_then(Value::as_str)
        .map(str::to_string)
        // A suggestion replaces lines of the new version, all of them shown in the diff.
        .filter(|_| side == "new" && (start..=line).all(|l| a.new_lines.contains(&l)));
    let confidence = v.get("confidence").and_then(Value::as_f64).unwrap_or(0.5).clamp(0.0, 1.0);
    Ok(Finding { path, side: side.into(), start_line: start, line, severity, title, body, suggestion, confidence })
}

fn words(s: &str) -> std::collections::BTreeSet<String> {
    s.to_ascii_lowercase().split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| w.len() > 2).map(str::to_string).collect()
}

fn similar(a: &str, b: &str) -> bool {
    let (wa, wb) = (words(a), words(b));
    if wa.is_empty() || wb.is_empty() {
        return a.trim().eq_ignore_ascii_case(b.trim());
    }
    let inter = wa.intersection(&wb).count() as f64;
    let union = wa.union(&wb).count() as f64;
    inter / union >= 0.5
}

/// Drop findings that repeat an open thread or each other (same file, within 3 lines, saying
/// much the same). Returns what's kept and how many were dropped.
pub fn dedupe(findings: Vec<Finding>, existing: &[Existing]) -> (Vec<Finding>, u32) {
    let mut kept: Vec<Finding> = Vec::new();
    let mut dropped = 0;
    for f in findings {
        let near = |p: &str, l: u64| p == f.path && l.abs_diff(f.line) <= 3;
        let text = format!("{} {}", f.title, f.body);
        let dup = existing.iter().any(|e| near(&e.path, e.line) && similar(&e.text, &text))
            || kept.iter().any(|k| near(&k.path, k.line) && similar(&format!("{} {}", k.title, k.body), &text));
        if dup {
            dropped += 1;
        } else {
            kept.push(f);
        }
    }
    (kept, dropped)
}

/// The verdict a profile gives these findings.
pub fn verdict(findings: &[Finding], profile: Profile) -> &'static str {
    let serious = |f: &&Finding, conf: f64| (f.severity == "bug" || f.severity == "security") && f.confidence >= conf;
    let blocking = match profile {
        Profile::Chill => findings.iter().any(|f| serious(&f, 0.8)),
        Profile::Assertive => findings.iter().any(|f| serious(&f, 0.5) || (f.severity == "perf" && f.confidence >= 0.8)),
    };
    if blocking {
        "request_changes"
    } else if findings.is_empty() {
        "approve"
    } else {
        "comment"
    }
}

/// Skipped because the repo's reviewer settings ignore the path (the owner's choice, so it
/// doesn't stop an approval).
pub const IGNORED: &str = "ignored by the reviewer settings";

/// roll only approves what it actually read: an `approve` becomes `comment` when files went
/// unreviewed for any reason but the repo's own `ignore` (past `max_files`, binary, too large,
/// declined by the model), and when an incremental review leaves roll's earlier findings open
/// (it only read the new commits). A change can't buy an approval by hiding code past the limit
/// or by tripping a refusal. Returns the verdict and, when it was lowered, why.
pub fn guard_approval(verdict: &str, skipped: &[Skipped], incremental: bool, open_roll_threads: usize) -> (&'static str, Option<String>) {
    let v: &'static str = match verdict {
        "approve" => "approve",
        "request_changes" => "request_changes",
        _ => "comment",
    };
    if v != "approve" {
        return (v, None);
    }
    let unread = skipped.iter().filter(|s| s.reason != IGNORED).count();
    if unread > 0 {
        return ("comment", Some(format!("roll didn't approve: {unread} file(s) weren't reviewed (see Skipped files).")));
    }
    if incremental && open_roll_threads > 0 {
        return ("comment", Some(format!("roll didn't approve: {open_roll_threads} earlier finding(s) are still open.")));
    }
    ("approve", None)
}

/// The review body: mode line, walkthrough, file table, diagram, notes and skipped files.
pub fn render_body(r: &ReviewResult, header: &str) -> String {
    let mut b = String::new();
    b.push_str(header.trim());
    b.push_str("\n\n");
    if !r.walkthrough.trim().is_empty() {
        b.push_str(r.walkthrough.trim());
        b.push_str("\n\n");
    }
    if !r.files.is_empty() {
        b.push_str("| File | Change |\n| --- | --- |\n");
        for (p, s) in &r.files {
            let _ = writeln!(b, "| `{}` | {} |", p.replace('|', "\\|"), s.replace('|', "\\|").replace('\n', " "));
        }
        b.push('\n');
    }
    if let Some(d) = &r.diagram {
        let _ = write!(b, "```mermaid\n{}\n```\n\n", d.trim().trim_start_matches("```mermaid").trim_end_matches("```").trim());
    }
    if !r.notes.is_empty() {
        b.push_str("**Notes**\n\n");
        for n in &r.notes {
            let _ = writeln!(b, "- {}", n.trim());
        }
        b.push('\n');
    }
    if !r.skipped.is_empty() {
        b.push_str("<details><summary>Not reviewed</summary>\n\n");
        for s in &r.skipped {
            let _ = writeln!(b, "- `{}`: {}", s.path, s.reason);
        }
        b.push_str("\n</details>\n");
    }
    b.trim_end().to_string()
}

/// A finding as a thread comment, with a committable suggestion block.
pub fn render_finding(f: &Finding, show_confidence: bool) -> String {
    let mut b = format!("**{}** · {}\n\n{}", f.severity, f.title.trim(), f.body.trim());
    if show_confidence {
        let _ = write!(b, "\n\n<sub>confidence {:.0}%</sub>", f.confidence * 100.0);
    }
    if let Some(s) = &f.suggestion {
        let _ = write!(b, "\n\n```suggestion\n{}\n```", s.trim_end_matches('\n'));
    }
    b
}

/// What [`render_finding`] wrote, read back: `(severity, title, confidence)` (confidence as
/// 0–1 when it was shown). `None` for text that isn't a rendered finding.
pub fn parse_finding(body: &str) -> Option<(String, String, Option<f64>)> {
    let first = body.lines().next()?;
    let rest = first.strip_prefix("**")?;
    let (severity, title) = rest.split_once("** · ")?;
    if severity.is_empty() || severity.contains(' ') || severity.len() > 20 {
        return None;
    }
    let confidence = body.rfind("<sub>confidence ").and_then(|i| {
        let tail = &body[i + "<sub>confidence ".len()..];
        tail.split_once("%</sub>").and_then(|(n, _)| n.trim().parse::<f64>().ok()).map(|p| (p / 100.0).clamp(0.0, 1.0))
    });
    Some((severity.to_string(), title.trim().to_string(), confidence))
}

/// `ai` on a roll review: how it was made and what it cost.
pub fn ai_summary(r: &ReviewResult) -> Value {
    json!({
        "model": r.model,
        "effort": r.effort.map(|e| e.as_str()),
        "requests": r.requests,
        "files_reviewed": r.files_reviewed,
        "findings": r.findings.len(),
        "dropped": { "invalid": r.dropped_invalid, "duplicates": r.dropped_duplicates, "unverified": r.dropped_unverified, "low_confidence": r.dropped_low_confidence },
        "skipped_files": r.skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_read_back() {
        let mut x = f("a.rs", 3, "bug", 0.87, "Off by one");
        x.body = "The loop skips the last item.".into();
        x.suggestion = Some("for i in 0..=n {".into());
        assert_eq!(parse_finding(&render_finding(&x, true)), Some(("bug".into(), "Off by one".into(), Some(0.87))));
        assert_eq!(parse_finding(&render_finding(&x, false)), Some(("bug".into(), "Off by one".into(), None)));
        assert_eq!(parse_finding("On it: a review of what changed."), None);
        assert_eq!(parse_finding("**bold** text without the separator"), None);
    }

    fn f(path: &str, line: u64, sev: &str, conf: f64, title: &str) -> Finding {
        Finding { path: path.into(), side: "new".into(), start_line: line, line, severity: sev.into(), title: title.into(), body: String::new(), suggestion: None, confidence: conf }
    }

    #[test]
    fn approvals_need_everything_read() {
        let sk = |reason: &str| Skipped { path: "x".into(), reason: reason.into() };
        assert_eq!(guard_approval("approve", &[], false, 0), ("approve", None));
        assert_eq!(guard_approval("approve", &[sk(IGNORED)], false, 0).0, "approve");
        for reason in ["past the first 50 files (max_files)", "binary", "the model declined to review it", "too large"] {
            let (v, why) = guard_approval("approve", &[sk(IGNORED), sk(reason)], false, 0);
            assert_eq!(v, "comment", "{reason}");
            assert!(why.unwrap().contains("1 file(s)"));
        }
        assert_eq!(guard_approval("approve", &[], true, 2).0, "comment");
        assert_eq!(guard_approval("approve", &[], true, 0).0, "approve");
        assert_eq!(guard_approval("approve", &[], false, 2).0, "approve", "a full review re-read everything");
        assert_eq!(guard_approval("request_changes", &[sk("binary")], true, 3), ("request_changes", None));
        assert_eq!(guard_approval("whatever", &[], false, 0).0, "comment");
    }

    #[test]
    fn verdicts() {
        assert_eq!(verdict(&[], Profile::Chill), "approve");
        assert_eq!(verdict(&[f("a", 1, "style", 0.9, "x")], Profile::Chill), "comment");
        assert_eq!(verdict(&[f("a", 1, "bug", 0.7, "x")], Profile::Chill), "comment");
        assert_eq!(verdict(&[f("a", 1, "bug", 0.9, "x")], Profile::Chill), "request_changes");
        assert_eq!(verdict(&[f("a", 1, "bug", 0.6, "x")], Profile::Assertive), "request_changes");
        assert_eq!(verdict(&[f("a", 1, "perf", 0.9, "x")], Profile::Assertive), "request_changes");
    }

    #[test]
    fn dedupes() {
        let existing = vec![Existing { path: "a.rs".into(), line: 10, text: "Unchecked index panics on empty input".into() }];
        let (k, d) = dedupe(
            vec![
                f("a.rs", 11, "bug", 0.9, "Unchecked index panics on empty input"),
                f("a.rs", 40, "bug", 0.9, "Unchecked index panics on empty input"),
                f("a.rs", 41, "bug", 0.9, "Unchecked index panics on empty input"),
                f("b.rs", 10, "bug", 0.9, "Something else entirely"),
            ],
            &existing,
        );
        assert_eq!(d, 2);
        assert_eq!(k.len(), 2);
    }

    #[test]
    fn validates_lines() {
        let a = prompt::annotate("@@ -1,2 +1,3 @@\n a\n+b\n c\n");
        let files = vec![(FileInput { path: "x.rs".into(), ..Default::default() }, a)];
        let ok = json!({"path": "x.rs", "side": "new", "start_line": 1, "line": 2, "severity": "bug", "title": "t", "body": "b", "suggestion": "B", "confidence": 1.5});
        let v = validate(&ok, &files).unwrap();
        assert_eq!((v.start_line, v.line, v.confidence), (1, 2, 1.0));
        assert_eq!(v.suggestion.as_deref(), Some("B"));
        let outside = json!({"path": "x.rs", "side": "new", "start_line": 9, "line": 9, "severity": "bug", "title": "t", "body": "b", "suggestion": null, "confidence": 0.5});
        assert!(validate(&outside, &files).unwrap_err().unwrap().contains("x.rs"));
        let other = json!({"path": "y.rs", "side": "new", "start_line": 1, "line": 1, "severity": "bug", "title": "t", "body": "b", "suggestion": null, "confidence": 0.5});
        assert!(validate(&other, &files).is_err());
        let bad_sev = json!({"path": "x.rs", "side": "new", "start_line": 1, "line": 1, "severity": "meh", "title": "t", "body": "b", "suggestion": null, "confidence": 0.5});
        assert_eq!(validate(&bad_sev, &files).unwrap_err(), None);
        // A suggestion on the old side can't be applied: it's dropped, the finding stays.
        let a2 = prompt::annotate("@@ -1,2 +1,1 @@\n-a\n b\n");
        let files2 = vec![(FileInput { path: "x.rs".into(), ..Default::default() }, a2)];
        let old = json!({"path": "x.rs", "side": "old", "start_line": 1, "line": 1, "severity": "bug", "title": "t", "body": "b", "suggestion": "z", "confidence": 0.5});
        assert_eq!(validate(&old, &files2).unwrap().suggestion, None);
    }

    #[test]
    fn renders() {
        let mut r = ReviewResult { walkthrough: "Adds x.".into(), ..Default::default() };
        r.files.push(("a|b.rs".into(), "new\nthing".into()));
        r.diagram = Some("```mermaid\nsequenceDiagram\nA->>B: hi\n```".into());
        r.skipped.push(Skipped { path: "big.json".into(), reason: "too large".into() });
        r.notes.push("Consider docs.".into());
        let b = render_body(&r, "**roll review** · standard");
        assert!(b.starts_with("**roll review** · standard\n\nAdds x."));
        assert!(b.contains("| `a\\|b.rs` | new thing |"));
        assert!(b.contains("```mermaid\nsequenceDiagram\nA->>B: hi\n```"));
        assert!(b.contains("- `big.json`: too large"));
        let mut fi = f("a", 1, "bug", 0.84, "Off by one");
        fi.suggestion = Some("let x = 1;\n".into());
        let c = render_finding(&fi, true);
        assert!(c.contains("**bug** · Off by one") && c.contains("confidence 84%") && c.ends_with("```suggestion\nlet x = 1;\n```"));
    }
}
