/**
 * Pure helpers for roll's reviewer, billing and code search: what a roll review's `ai` record says,
 * severity chips on its comments, job progress, usage bars, the pricing table, search result
 * grouping and highlighting, and the reviewer settings form. No React, so they're unit-tested.
 */

import type {
  AiJob,
  AiJobStage,
  AiMode,
  AiModeSetting,
  CommentFinding,
  Competitor,
  Pricing,
  ReviewAi,
  ReviewerSettings,
  ReviewerView,
  SearchMatch,
  SearchResult,
  Usage,
} from "./code-api";

// ── money and months ──

/** "$0.70", "$1,204.50", "<$0.01" for tiny non-zero amounts, "$0" for nothing. */
export function formatUsd(n: number | null | undefined) {
  if (n == null || !Number.isFinite(n)) return "—";
  if (n === 0) return "$0";
  if (Math.abs(n) < 0.01) return n < 0 ? "−<$0.01" : "<$0.01";
  return `${n < 0 ? "−" : ""}$${Math.abs(n).toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

/** "2026-10" for a date (UTC). */
export function monthKey(d: Date) {
  return `${d.getUTCFullYear()}-${String(d.getUTCMonth() + 1).padStart(2, "0")}`;
}

/** The last `n` months, newest first, as `YYYY-MM`. */
export function recentMonths(now: Date, n = 12) {
  const out: string[] = [];
  for (let i = 0; i < n; i++) out.push(monthKey(new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() - i, 1))));
  return out;
}

/** "Oct 2026". */
export function monthLabel(month: string) {
  const [y, m] = month.split("-").map(Number);
  if (!y || !m) return month;
  return new Date(Date.UTC(y, m - 1, 1)).toLocaleDateString("en-US", { month: "short", year: "numeric", timeZone: "UTC" });
}

/** "12.4k" style token counts. */
export function formatTokens(n: number) {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(n < 10_000_000 ? 1 : 0)}M`;
}

// ── a roll review ──

export const MODE_LABEL: Record<AiMode, string> = { standard: "Standard", deep: "Deep", max: "Max" };
export const MODE_HINT: Record<AiModeSetting, string> = {
  auto: "roll picks the depth for each change from its risk",
  standard: "Walkthrough and line comments on the diff",
  deep: "Adds codebase context: definitions, callers, related tests",
  max: "Deep, plus a second pass that re-checks every finding",
};

export interface AiHeader {
  mode: string;
  /** "Picked automatically" and friends. */
  chosen: string;
  auto: boolean;
  reasons: string[];
  speed: string;
  model: string;
  key: string;
  incremental: boolean;
  price: string;
  /** The list price, when what was charged differs (free allowance, cap, a person's monthly cap). */
  listPrice: string | null;
  tokens: string;
  files: number;
  findings: number;
  dropped: number;
  skipped: { path: string; reason: string }[];
}

const CHOSEN: Record<ReviewAi["chosen_by"], string> = {
  "auto-heuristic": "Picked automatically",
  "auto-model": "Picked automatically by a quick model call",
  settings: "Set in the repo's settings",
  command: "Asked for with @roll",
};

/** What the header of a roll review shows, from its `ai` record. */
export function aiHeader(ai: ReviewAi): AiHeader {
  const d = ai.details;
  const dropped = d ? d.dropped.invalid + d.dropped.duplicates + d.dropped.unverified + d.dropped.low_confidence : 0;
  const t = ai.tokens;
  return {
    mode: `${MODE_LABEL[ai.mode] ?? ai.mode} review`,
    chosen: CHOSEN[ai.chosen_by] ?? ai.chosen_by,
    auto: ai.chosen_by === "auto-heuristic" || ai.chosen_by === "auto-model",
    reasons: ai.reasons ?? [],
    speed: ai.speed,
    model: ai.model,
    key: ai.key === "byok" ? "your key" : "hosted",
    incremental: ai.incremental,
    price: formatUsd(ai.price_usd),
    listPrice: Math.abs(ai.list_price_usd - ai.price_usd) >= 0.005 ? formatUsd(ai.list_price_usd) : null,
    tokens: `${formatTokens(t.input + t.cache_read + t.cache_write)} in · ${formatTokens(t.output)} out${
      t.cache_read > 0 ? ` · ${Math.round((t.cache_read / Math.max(1, t.input + t.cache_read + t.cache_write)) * 100)}% cached` : ""
    }`,
    files: d?.files_reviewed ?? 0,
    findings: d?.findings ?? 0,
    dropped,
    skipped: ai.skipped_files ?? [],
  };
}

/** roll's review body starts with a one-line header the card already shows; drop it. */
export function stripAiHeader(body: string) {
  return body.replace(/^\*\*roll review\*\*[^\n]*\n*/, "");
}

// ── roll's comments ──

export const SEVERITIES = ["bug", "security", "perf", "style", "nit"] as const;
export type Severity = (typeof SEVERITIES)[number];

/** Bugs and security findings get the accent; the rest stay quiet. */
export function severityTone(s: Severity): "accent" | "muted" {
  return s === "bug" || s === "security" ? "accent" : "muted";
}

export interface Finding {
  severity: Severity | null;
  title: string | null;
  /** The rest of the comment, without the severity line or the confidence note. */
  body: string;
  /** 0–1, shown for deep and max reviews. */
  confidence: number | null;
}

/**
 * roll writes each finding as `**bug** · Title`, the explanation, then (deep and max)
 * `<sub>confidence 84%</sub>`. Anything else (a reply, a person's comment) comes back as plain body.
 */
export function parseFinding(body: string): Finding {
  let rest = body;
  let confidence: number | null = null;
  const conf = /\n*<sub>confidence (\d{1,3})%<\/sub>[ \t]*(?=\n|$)/.exec(rest);
  if (conf) {
    confidence = Math.min(100, Number(conf[1])) / 100;
    rest = rest.slice(0, conf.index) + rest.slice(conf.index + conf[0].length);
  }
  const head = /^\*\*(bug|security|perf|style|nit)\*\* · ([^\n]*)\n*/.exec(rest);
  if (!head) return { severity: null, title: null, body: rest.trim(), confidence };
  return { severity: head[1] as Severity, title: head[2].trim(), body: rest.slice(head[0].length).trim(), confidence };
}

// ── jobs ──

/** The job in flight (queued or running), newest first; null when roll is idle on this change. */
export function activeJob(jobs: AiJob[] | null | undefined): AiJob | null {
  if (!jobs?.length) return null;
  for (let i = jobs.length - 1; i >= 0; i--) if (jobs[i].state === "queued" || jobs[i].state === "running") return jobs[i];
  return null;
}

/** The latest finished job that failed, if it's newer than the latest success: why roll stopped. */
export function lastFailure(jobs: AiJob[] | null | undefined): AiJob | null {
  if (!jobs?.length) return null;
  const last = jobs[jobs.length - 1];
  return last.state === "failed" ? last : null;
}

const STAGE_LABEL: Record<Exclude<AiJobStage, "done" | "failed">, string> = {
  queued: "Waiting for a reviewer slot",
  indexing: "Pulling in context from the codebase",
  chunking: "Reading the diff",
  reviewing: "Reviewing each file",
  verifying: "Re-checking every finding",
  posting: "Writing comments",
};

/** The stage a job is at; servers without `stage` only say queued or running. */
export function jobStage(job: Pick<AiJob, "state" | "stage">): AiJobStage {
  if (job.stage) return job.stage;
  if (job.state === "queued") return "queued";
  if (job.state === "done" || job.state === "skipped") return "done";
  if (job.state === "failed") return "failed";
  return "chunking";
}

export interface JobSteps {
  /** The step lines, in order; `current` is the one that shimmers, earlier ones are done. */
  steps: string[];
  current: number;
  /** groups_done / groups_total while reviewing, for a hairline meter; null otherwise. */
  fraction: number | null;
}

/**
 * The step lines shown while roll works, from the job's real stage and progress. Indexing shows for
 * the modes that pull in context (deep, max, auto); verifying for max, or whenever the job reaches it
 * (thorough speed verifies at any depth).
 */
export function jobSteps(job: Pick<AiJob, "state" | "stage" | "mode" | "progress">): JobSteps {
  const stage = jobStage(job);
  if (stage === "queued") return { steps: [STAGE_LABEL.queued], current: 0, fraction: null };
  const mode = job.mode;
  const order: (keyof typeof STAGE_LABEL)[] = [];
  if (mode === "deep" || mode === "max" || mode === "auto" || mode == null || stage === "indexing") order.push("indexing");
  order.push("chunking", "reviewing");
  if (mode === "max" || stage === "verifying") order.push("verifying");
  order.push("posting");
  const p = job.progress;
  const groups = p && p.groups_total > 1 ? `Reviewing files · ${Math.min(p.groups_done, p.groups_total)} of ${p.groups_total} groups` : null;
  const steps = order.map((k) => (k === "reviewing" && groups ? groups : STAGE_LABEL[k]));
  const at = stage === "done" || stage === "failed" ? order.length - 1 : order.indexOf(stage);
  const fraction = stage === "reviewing" && p && p.groups_total > 0 ? Math.min(1, p.groups_done / p.groups_total) : null;
  return { steps, current: Math.max(0, at), fraction };
}

/**
 * A roll comment's finding: the structured `finding` when the server sends it (the text still leads
 * with the severity line, which is stripped from the body), the text format otherwise.
 */
export function findingOf(comment: { body: string; finding?: CommentFinding | null }): Finding {
  const parsed = parseFinding(comment.body);
  const f = comment.finding;
  if (!f) return parsed;
  const severity = (SEVERITIES as readonly string[]).includes(f.severity) ? (f.severity as Severity) : null;
  return { severity, title: f.title || parsed.title, body: parsed.body, confidence: f.confidence ?? parsed.confidence };
}

// ── usage and pricing ──

export interface Bar {
  key: string;
  label: string;
  value: number;
  /** 0–1 against the largest. */
  frac: number;
  detail: string;
}

/** Bars against the largest value, largest first (ties keep their order). */
export function bars(rows: { key: string; label: string; value: number; detail: string }[]): Bar[] {
  const max = Math.max(0, ...rows.map((r) => r.value));
  return rows
    .map((r, i) => ({ ...r, i, frac: max > 0 ? r.value / max : 0 }))
    .sort((a, b) => b.value - a.value || a.i - b.i)
    .map(({ i: _i, ...r }) => r);
}

/** Spend per repo, as bars. `count` covers reviews and chat replies alike, so it's "runs". */
export function repoBars(u: Usage): Bar[] {
  return bars(
    u.by_repo.map((r) => ({
      key: r.repo_id,
      label: r.repo,
      value: r.price_usd,
      detail: `${r.count} run${r.count === 1 ? "" : "s"} · ${formatUsd(r.price_usd)}`,
    })),
  );
}

/** Spend per mode, in mode order (standard, deep, max), skipping modes with nothing. */
export function modeBars(u: Usage): Bar[] {
  const rows = (["standard", "deep", "max"] as const)
    .map((m) => ({ m, v: u.by_mode[m] }))
    .filter((x) => x.v && (x.v.reviews > 0 || x.v.price_usd > 0))
    .map(({ m, v }) => ({ key: m, label: MODE_LABEL[m], value: v!.price_usd, detail: `${v!.reviews} · ${formatUsd(v!.price_usd)}` }));
  const max = Math.max(0, ...rows.map((r) => r.value));
  return rows.map((r) => ({ ...r, frac: max > 0 ? r.value / max : 0 }));
}

/** How much of the cap is spent, 0–1; null without a cap. */
export function capUse(spent: number, cap: number | null) {
  if (cap == null) return null;
  if (cap <= 0) return 1;
  return Math.min(1, Math.max(0, spent / cap));
}

export interface PriceRow {
  label: string;
  hosted: string;
  byok: string;
}

/** roll's rates, one row per kind of review. */
export function pricingRows(p: Pricing): PriceRow[] {
  const s = p.hosted.standard;
  return [
    { label: "Standard", hosted: `${formatUsd(s.per_file)} per file, min ${formatUsd(s.min_per_review)}`, byok: `${formatUsd(p.byok.standard)} per review` },
    { label: "Deep", hosted: `${p.hosted.deep.multiplier}× standard`, byok: `${formatUsd(p.byok.deep)} per review` },
    { label: "Max", hosted: `${p.hosted.max.multiplier}× standard`, byok: `${formatUsd(p.byok.max)} per review` },
    { label: "Chat reply", hosted: `${formatUsd(p.hosted.chat)}`, byok: p.byok.chat > 0 ? formatUsd(p.byok.chat) : "free" },
  ];
}

export interface CompetitorRow {
  name: string;
  seat: string;
  usage: string;
}

/** A competitor's published prices in one line each; fields it doesn't have are left out. */
export function competitorRow(c: Competitor): CompetitorRow {
  const seat =
    c.per_seat_month != null
      ? `${formatUsd(c.per_seat_month)} / seat / month`
      : c.per_seat_month_annual != null
        ? `${formatUsd(c.per_seat_month_annual)}–${formatUsd(c.per_seat_month_monthly ?? c.per_seat_month_annual)} / seat / month`
        : "—";
  const usage: string[] = [];
  if (c.included_credits_per_seat != null) usage.push(`${c.included_credits_per_seat} credits per seat`);
  if (c.credit_usd != null) usage.push(`${formatUsd(c.credit_usd)} per extra credit`);
  if (c.credits_per_review) {
    usage.push(
      Object.entries(c.credits_per_review)
        .map(([k, v]) => `${k} ${v}`)
        .join(" · ") + " credits per review",
    );
  }
  if (c.overage_per_reviewed_file != null) usage.push(`${formatUsd(c.overage_per_reviewed_file)} per file over the limit`);
  return { name: c.name, seat, usage: usage.join("; ") || "—" };
}

/** What a hosted review of `files` files costs in `mode`, at list price. */
export function hostedPrice(p: Pricing, mode: AiMode, files: number) {
  const base = Math.max(p.hosted.standard.min_per_review, p.hosted.standard.per_file * Math.max(0, files));
  const mult = mode === "deep" ? p.hosted.deep.multiplier : mode === "max" ? p.hosted.max.multiplier : 1;
  return Math.round(base * mult * 100) / 100;
}

// ── search ──

/** The revision search results link to: the changeset searched, else the indexed head. */
export function searchLinkRev(result: Pick<SearchResult, "rev" | "head">) {
  return result.rev ?? result.head ?? "HEAD";
}

/**
 * The note under results when a revision was searched: how many files were searched at that
 * revision directly, and that the rest came from the default branch's index. Null otherwise.
 */
export function overlayNote(result: Pick<SearchResult, "overlay">, defaultBranch = "main", what = "this change"): string | null {
  const o = result.overlay;
  if (!o) return null;
  if (o.files === 0) return `Nothing in ${what} differs from ${defaultBranch}: results come from ${defaultBranch}'s index.`;
  return `${o.files.toLocaleString("en-US")} file${o.files === 1 ? "" : "s"} searched at ${what}, the rest from ${defaultBranch}.`;
}

export interface FileMatches {
  path: string;
  matches: SearchMatch[];
}

/** Matches grouped by file, files in the order they first appear, lines in order. */
export function groupMatches(matches: SearchMatch[]): FileMatches[] {
  const by = new Map<string, SearchMatch[]>();
  for (const m of matches) {
    const list = by.get(m.path);
    if (list) list.push(m);
    else by.set(m.path, [m]);
  }
  return [...by].map(([path, ms]) => ({ path, matches: [...ms].sort((a, b) => a.line - b.line) }));
}

/**
 * Where `query` matches in `text`, as non-overlapping [start, end) ranges: literal and
 * case-insensitive by default, like the server. A bad regex matches nothing.
 */
export function highlightRanges(text: string, query: string, opts: { regex?: boolean; case?: boolean } = {}, max = 50): [number, number][] {
  if (!query) return [];
  let re: RegExp;
  try {
    re = new RegExp(opts.regex ? query : query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), opts.case ? "g" : "gi");
  } catch {
    return [];
  }
  const out: [number, number][] = [];
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) && out.length < max) {
    if (m[0].length === 0) {
      re.lastIndex++;
      continue;
    }
    out.push([m.index, m.index + m[0].length]);
  }
  return out;
}

/** `text` cut into plain and highlighted pieces. */
export function splitHighlights(text: string, ranges: [number, number][]): { text: string; hit: boolean }[] {
  const out: { text: string; hit: boolean }[] = [];
  let at = 0;
  for (const [s, e] of ranges) {
    if (s > at) out.push({ text: text.slice(at, s), hit: false });
    out.push({ text: text.slice(s, e), hit: true });
    at = e;
  }
  if (at < text.length || !out.length) out.push({ text: text.slice(at), hit: false });
  return out;
}

/** Whether a regex query compiles (the server answers 400 otherwise). */
export function validRegex(q: string) {
  try {
    new RegExp(q);
    return true;
  } catch {
    return false;
  }
}

// ── reviewer settings form ──

/** The settings as form fields: lists are one entry per line, optional strings are "". */
export interface ReviewerForm {
  enabled: boolean;
  mode: AiModeSetting;
  deep_paths: string;
  max_paths: string;
  standard_below_lines: string;
  classifier: "heuristic" | "model";
  speed: ReviewerSettings["speed"];
  auto_review: boolean;
  drafts: boolean;
  required: boolean;
  profile: ReviewerSettings["profile"];
  ignore: string;
  instructions: { path: string; text: string }[];
  rules: string;
  prompt: string;
  prompt_file: string;
  model: string;
  secondary_model: string;
  effort: ReviewerSettings["effort"] | "";
  diagram: boolean;
  max_files: string;
}

const lines = (v: string[]) => v.join("\n");
const unlines = (v: string) =>
  v
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);
const orNull = (v: string) => (v.trim() ? v.trim() : null);

export function toForm(s: ReviewerSettings): ReviewerForm {
  return {
    enabled: s.enabled,
    mode: s.mode,
    deep_paths: lines(s.auto.deep_paths),
    max_paths: lines(s.auto.max_paths),
    standard_below_lines: String(s.auto.standard_below_lines),
    classifier: s.auto.classifier,
    speed: s.speed,
    auto_review: s.auto_review,
    drafts: s.drafts,
    required: s.required,
    profile: s.profile,
    ignore: lines(s.ignore),
    instructions: s.instructions.map((i) => ({ ...i })),
    rules: lines(s.rules),
    prompt: s.prompt ?? "",
    prompt_file: s.prompt_file ?? "",
    model: s.model ?? "",
    secondary_model: s.secondary_model ?? "",
    effort: s.effort ?? "",
    diagram: s.diagram,
    max_files: String(s.max_files),
  };
}

/** Problems that would make the server answer 400, by field. */
export function formErrors(f: ReviewerForm): Partial<Record<keyof ReviewerForm, string>> {
  const e: Partial<Record<keyof ReviewerForm, string>> = {};
  if (!/^\d+$/.test(f.standard_below_lines.trim())) e.standard_below_lines = "a whole number of lines";
  if (!/^\d+$/.test(f.max_files.trim()) || Number(f.max_files) < 1) e.max_files = "at least 1";
  if (f.instructions.some((i) => !i.path.trim() || !i.text.trim())) e.instructions = "each instruction needs a path and text";
  return e;
}

const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

/** The PATCH body: only what differs from `base` (the settings the form was opened with). */
export function formPatch(f: ReviewerForm, base: ReviewerSettings): Partial<ReviewerSettings> {
  const next: ReviewerSettings = {
    enabled: f.enabled,
    mode: f.mode,
    auto: {
      deep_paths: unlines(f.deep_paths),
      max_paths: unlines(f.max_paths),
      standard_below_lines: Number(f.standard_below_lines.trim() || base.auto.standard_below_lines),
      classifier: f.classifier,
    },
    speed: f.speed,
    auto_review: f.auto_review,
    drafts: f.drafts,
    required: f.required,
    profile: f.profile,
    ignore: unlines(f.ignore),
    instructions: f.instructions.map((i) => ({ path: i.path.trim(), text: i.text.trim() })).filter((i) => i.path && i.text),
    rules: unlines(f.rules),
    prompt: f.prompt.trim() ? f.prompt : null,
    prompt_file: orNull(f.prompt_file),
    model: orNull(f.model),
    secondary_model: orNull(f.secondary_model),
    effort: f.effort || null,
    diagram: f.diagram,
    max_files: Number(f.max_files.trim() || base.max_files),
  };
  const patch: Partial<ReviewerSettings> = {};
  for (const k of Object.keys(next) as (keyof ReviewerSettings)[]) {
    if (!same(next[k], base[k])) (patch as Record<string, unknown>)[k] = next[k];
  }
  return patch;
}

/** The top-level keys `.roll/review.yaml`'s `reviewer:` section sets (they win over the form). */
export function fileKeys(view: Pick<ReviewerView, "file">): Set<string> {
  return new Set(view.file && typeof view.file === "object" ? Object.keys(view.file) : []);
}

/**
 * What the shared mark should do as a change's AI job comes and goes: think while roll works (only
 * taking the mark from idle), celebrate once when a review lands, settle back on failure.
 */
export function markStep(
  wasActive: boolean,
  nowActive: boolean,
  finished: AiJob | null,
  mark: string,
): "think" | "success" | "idle" | null {
  if (nowActive) return !wasActive && mark === "idle" ? "think" : null;
  if (!wasActive) return null;
  if (mark !== "thinking") return null;
  return finished?.state === "done" ? "success" : "idle";
}
