import { describe, expect, it } from "vitest";
import type { AiJob, Pricing, ReviewAi, ReviewerSettings, Usage } from "./code-api";
import {
  activeJob,
  aiHeader,
  bars,
  capUse,
  competitorRow,
  fileKeys,
  formatTokens,
  formatUsd,
  formErrors,
  formPatch,
  groupMatches,
  highlightRanges,
  hostedPrice,
  lastFailure,
  markStep,
  modeBars,
  monthKey,
  monthLabel,
  findingOf,
  jobStage,
  jobSteps,
  overlayNote,
  parseFinding,
  searchLinkRev,
  pricingRows,
  recentMonths,
  repoBars,
  severityTone,
  splitHighlights,
  stripAiHeader,
  toForm,
  validRegex,
} from "./ai-review";

const ai = (over: Partial<ReviewAi> = {}): ReviewAi => ({
  mode: "deep",
  chosen_by: "auto-heuristic",
  reasons: ["touches crates/auth/**", "412 changed lines"],
  speed: "balanced",
  incremental: false,
  provider: "anthropic",
  model: "claude-opus-5-5",
  key: "hosted",
  tokens: { input: 8000, output: 1500, cache_write: 0, cache_read: 2000 },
  price_usd: 2.1,
  list_price_usd: 2.1,
  skipped_files: [{ path: "Cargo.lock", reason: "ignored" }],
  details: {
    model: "claude-opus-5-5",
    effort: "medium",
    requests: 2,
    files_reviewed: 7,
    findings: 3,
    dropped: { invalid: 1, duplicates: 1, unverified: 0, low_confidence: 0 },
    skipped_files: 1,
  },
  ...over,
});

const job = (id: string, state: AiJob["state"], over: Partial<AiJob> = {}): AiJob => ({
  id,
  repo_id: "repo_1",
  change_id: "chg_1",
  kind: "review",
  mode: "deep",
  full: false,
  state,
  attempts: 1,
  error: null,
  review_id: null,
  requested_by: null,
  created_at: 100,
  updated_at: 100,
  ...over,
});

const settings = (over: Partial<ReviewerSettings> = {}): ReviewerSettings => ({
  enabled: true,
  mode: "auto",
  auto: { deep_paths: ["crates/*/src/auth/**"], max_paths: [], standard_below_lines: 80, classifier: "heuristic" },
  speed: "balanced",
  auto_review: true,
  drafts: false,
  required: false,
  profile: "chill",
  ignore: ["**/*.lock"],
  instructions: [{ path: "crates/**/*.rs", text: "No unbounded allocations." }],
  rules: ["Every route needs a test"],
  prompt: null,
  prompt_file: null,
  model: null,
  secondary_model: null,
  effort: null,
  diagram: true,
  max_files: 200,
  ...over,
});

describe("money, months, tokens", () => {
  it("formats dollars", () => {
    expect(formatUsd(0)).toBe("$0");
    expect(formatUsd(0.004)).toBe("<$0.01");
    expect(formatUsd(0.7)).toBe("$0.70");
    expect(formatUsd(1204.5)).toBe("$1,204.50");
    expect(formatUsd(null)).toBe("—");
    expect(formatUsd(Number.NaN)).toBe("—");
  });

  it("lists months newest first across a year boundary", () => {
    const months = recentMonths(new Date(Date.UTC(2026, 1, 15)), 4);
    expect(months).toEqual(["2026-02", "2026-01", "2025-12", "2025-11"]);
    expect(monthKey(new Date(Date.UTC(2026, 9, 31, 23, 59)))).toBe("2026-10");
    expect(monthLabel("2026-10")).toBe("Oct 2026");
    expect(monthLabel("garbage")).toBe("garbage");
  });

  it("abbreviates token counts", () => {
    expect(formatTokens(999)).toBe("999");
    expect(formatTokens(1500)).toBe("1.5k");
    expect(formatTokens(45_000)).toBe("45k");
    expect(formatTokens(2_500_000)).toBe("2.5M");
  });
});

describe("a roll review's header", () => {
  it("explains the mode, cost and what was skipped", () => {
    const h = aiHeader(ai());
    expect(h.mode).toBe("Deep review");
    expect(h.auto).toBe(true);
    expect(h.chosen).toBe("Picked automatically");
    expect(h.reasons).toEqual(["touches crates/auth/**", "412 changed lines"]);
    expect(h.price).toBe("$2.10");
    expect(h.listPrice).toBeNull();
    expect(h.key).toBe("hosted");
    expect(h.files).toBe(7);
    expect(h.findings).toBe(3);
    expect(h.dropped).toBe(2);
    expect(h.tokens).toBe("10k in · 1.5k out · 20% cached");
    expect(h.skipped).toEqual([{ path: "Cargo.lock", reason: "ignored" }]);
  });

  it("shows the list price only when the charge differs, and names the key", () => {
    const h = aiHeader(ai({ price_usd: 0, list_price_usd: 0.7, key: "byok", chosen_by: "command", tokens: { input: 10, output: 5, cache_write: 0, cache_read: 0 } }));
    expect(h.price).toBe("$0");
    expect(h.listPrice).toBe("$0.70");
    expect(h.key).toBe("your key");
    expect(h.auto).toBe(false);
    expect(h.chosen).toBe("Asked for with @roll");
    expect(h.tokens).toBe("10 in · 5 out");
  });

  it("drops the header line roll writes into the body", () => {
    expect(stripAiHeader("**roll review** · deep mode (auto: x) · balanced · m\n\nAdds auth.\n\n| File | Change |")).toBe("Adds auth.\n\n| File | Change |");
    expect(stripAiHeader("A person's review")).toBe("A person's review");
  });
});

describe("roll's findings", () => {
  it("parses severity, title, body and confidence", () => {
    const f = parseFinding("**bug** · Off by one\n\nThe loop skips the last item.\n\n<sub>confidence 84%</sub>");
    expect(f).toEqual({ severity: "bug", title: "Off by one", body: "The loop skips the last item.", confidence: 0.84 });
  });

  it("works without confidence (standard reviews) and for every severity", () => {
    for (const s of ["bug", "security", "perf", "style", "nit"] as const) {
      expect(parseFinding(`**${s}** · T\n\nB`)).toEqual({ severity: s, title: "T", body: "B", confidence: null });
    }
  });

  it("leaves replies and unknown severities as plain body", () => {
    expect(parseFinding("Sure — the guard on line 4 covers it.")).toEqual({ severity: null, title: null, body: "Sure — the guard on line 4 covers it.", confidence: null });
    expect(parseFinding("**meh** · T\n\nB").severity).toBeNull();
  });

  it("prefers the structured finding and strips the severity line from the body", () => {
    const body = "**bug** · Off by one\n\nThe loop skips the last item.\n\n<sub>confidence 84%</sub>";
    expect(findingOf({ body, finding: { severity: "security", title: "Token in logs", confidence: 0.9 } })).toEqual({
      severity: "security",
      title: "Token in logs",
      body: "The loop skips the last item.",
      confidence: 0.9,
    });
    // Null confidence in the record falls back to the text's note.
    expect(findingOf({ body, finding: { severity: "bug", title: "Off by one", confidence: null } }).confidence).toBe(0.84);
  });

  it("falls back to the text when there's no finding record", () => {
    expect(findingOf({ body: "**nit** · Name\n\nRename it.", finding: null })).toEqual({ severity: "nit", title: "Name", body: "Rename it.", confidence: null });
    expect(findingOf({ body: "A reply." }).severity).toBeNull();
  });

  it("ignores severities it doesn't know", () => {
    const f = findingOf({ body: "Hm.", finding: { severity: "odd" as "bug", title: "T", confidence: null } });
    expect(f.severity).toBeNull();
    expect(f.title).toBe("T");
  });

  it("gives bugs and security the accent, the rest stay quiet", () => {
    expect(severityTone("bug")).toBe("accent");
    expect(severityTone("security")).toBe("accent");
    expect(severityTone("perf")).toBe("muted");
    expect(severityTone("style")).toBe("muted");
    expect(severityTone("nit")).toBe("muted");
  });
});

describe("jobs and the mark", () => {
  it("finds the job in flight and the last failure", () => {
    expect(activeJob(null)).toBeNull();
    expect(activeJob([job("a", "done"), job("b", "running")])?.id).toBe("b");
    expect(activeJob([job("a", "queued"), job("b", "done")])?.id).toBe("a");
    expect(activeJob([job("a", "done"), job("b", "failed")])).toBeNull();
    expect(lastFailure([job("a", "done"), job("b", "failed", { error: "rate limited" })])?.error).toBe("rate limited");
    expect(lastFailure([job("a", "failed"), job("b", "done")])).toBeNull();
  });

  it("reads the stage, falling back to the state on older servers", () => {
    expect(jobStage(job("a", "running", { stage: "verifying" }))).toBe("verifying");
    expect(jobStage(job("a", "queued"))).toBe("queued");
    expect(jobStage(job("a", "running"))).toBe("chunking");
    expect(jobStage(job("a", "done"))).toBe("done");
    expect(jobStage(job("a", "failed"))).toBe("failed");
  });

  it("shows real stages as step lines, by mode", () => {
    expect(jobSteps(job("a", "queued", { stage: "queued" }))).toEqual({ steps: ["Waiting for a reviewer slot"], current: 0, fraction: null });
    const std = jobSteps(job("a", "running", { mode: "standard", stage: "chunking" }));
    expect(std.steps).toEqual(["Reading the diff", "Reviewing each file", "Writing comments"]);
    expect(std.current).toBe(0);
    const deep = jobSteps(job("a", "running", { mode: "deep", stage: "indexing" }));
    expect(deep.steps[0]).toBe("Pulling in context from the codebase");
    expect(deep.steps).not.toContain("Re-checking every finding");
    const max = jobSteps(job("a", "running", { mode: "max", stage: "verifying" }));
    expect(max.steps[max.current]).toBe("Re-checking every finding");
    expect(max.current).toBe(3);
  });

  it("adds verifying when a thorough job reaches it at any depth", () => {
    const s = jobSteps(job("a", "running", { mode: "standard", stage: "verifying" }));
    expect(s.steps).toEqual(["Reading the diff", "Reviewing each file", "Re-checking every finding", "Writing comments"]);
    expect(s.current).toBe(2);
  });

  it("counts file groups while reviewing", () => {
    const s = jobSteps(job("a", "running", { mode: "standard", stage: "reviewing", progress: { groups_done: 3, groups_total: 7 } }));
    expect(s.steps[s.current]).toBe("Reviewing files · 3 of 7 groups");
    expect(s.fraction).toBeCloseTo(3 / 7);
    // One group needs no counter; progress past the total is clamped.
    expect(jobSteps(job("a", "running", { mode: "standard", stage: "reviewing", progress: { groups_done: 0, groups_total: 1 } })).steps[1]).toBe(
      "Reviewing each file",
    );
    expect(jobSteps(job("a", "running", { mode: "standard", stage: "reviewing", progress: { groups_done: 9, groups_total: 4 } })).fraction).toBe(1);
    // No meter outside reviewing.
    expect(jobSteps(job("a", "running", { mode: "standard", stage: "posting", progress: { groups_done: 4, groups_total: 4 } })).fraction).toBeNull();
  });

  it("holds on the last line once finished", () => {
    const s = jobSteps(job("a", "done", { mode: "standard", stage: "done" }));
    expect(s.current).toBe(s.steps.length - 1);
  });

  it("thinks while roll works and celebrates once", () => {
    expect(markStep(false, true, null, "idle")).toBe("think");
    expect(markStep(false, true, null, "deploying")).toBeNull();
    expect(markStep(true, true, null, "thinking")).toBeNull();
    expect(markStep(true, false, job("a", "done"), "thinking")).toBe("success");
    expect(markStep(true, false, job("a", "failed"), "thinking")).toBe("idle");
    expect(markStep(true, false, job("a", "done"), "success")).toBeNull();
    expect(markStep(false, false, job("a", "done"), "idle")).toBeNull();
  });
});

describe("usage and pricing", () => {
  const usage: Usage = {
    owner: "acme",
    month: "2026-10",
    totals: { reviews: 12, chats: 3, price_usd: 9.4, list_usd: 10, provider_cost_usd: 3.1, free_reviews: 1, capped_reviews: 0, tokens: { input: 1, output: 1, cache_write: 0, cache_read: 0 } },
    by_repo: [
      { repo: "acme/web", repo_id: "r1", count: 2, price_usd: 1.4 },
      { repo: "acme/api", repo_id: "r2", count: 10, price_usd: 8 },
      { repo: "acme/docs", repo_id: "r3", count: 0, price_usd: 0 },
    ],
    by_mode: { standard: { reviews: 10, price_usd: 4 }, max: { reviews: 2, price_usd: 5.4 }, deep: { reviews: 0, price_usd: 0 } },
    by_key: { hosted: { count: 12, price_usd: 9.4 } },
    cap: { monthly_cap_usd: 50, on_cap: "pause", remaining_usd: 40.6 },
    recent: [],
  };

  it("draws bars against the largest, largest first, stable on ties", () => {
    const b = repoBars(usage);
    expect(b.map((x) => x.label)).toEqual(["acme/api", "acme/web", "acme/docs"]);
    expect(b[0].frac).toBe(1);
    expect(b[1].frac).toBeCloseTo(0.175);
    expect(b[2].frac).toBe(0);
    expect(b[0].detail).toBe("10 runs · $8.00"); // reviews and chat replies together
    expect(bars([{ key: "a", label: "a", value: 1, detail: "" }, { key: "b", label: "b", value: 1, detail: "" }]).map((x) => x.key)).toEqual(["a", "b"]);
    expect(bars([{ key: "z", label: "z", value: 0, detail: "" }])[0].frac).toBe(0);
  });

  it("keeps modes in depth order and skips empty ones", () => {
    const m = modeBars(usage);
    expect(m.map((x) => x.key)).toEqual(["standard", "max"]);
    expect(m[1].frac).toBe(1);
    expect(m[0].frac).toBeCloseTo(4 / 5.4);
  });

  it("measures the cap", () => {
    expect(capUse(10, null)).toBeNull();
    expect(capUse(10, 40)).toBe(0.25);
    expect(capUse(80, 40)).toBe(1);
    expect(capUse(1, 0)).toBe(1);
  });

  const pricing: Pricing = {
    currency: "USD",
    hosted: { standard: { per_file: 0.1, min_per_review: 0.25 }, deep: { multiplier: 3 }, max: { multiplier: 10 }, chat: 0.05 },
    byok: { standard: 0.02, deep: 0.05, max: 0.15, chat: 0 },
    public_repos: { free_standard_reviews_per_month: 100 },
    human_monthly_cap: { standard: 24, note: "" },
    spending_caps: { on_cap: ["pause", "byok_fallback", "standard_only"] },
    competitors: [
      { name: "Greptile Pro", per_seat_month: 30, included_credits_per_seat: 50, credit_usd: 1, credits_per_review: { base: 1, plus: 3, apex: 10 } },
      { name: "CodeRabbit Essentials", per_seat_month_annual: 24, per_seat_month_monthly: 30, overage_per_reviewed_file: 0.25 },
    ],
  };

  it("lays out roll's rates", () => {
    expect(pricingRows(pricing)).toEqual([
      { label: "Standard", hosted: "$0.10 per file, min $0.25", byok: "$0.02 per review" },
      { label: "Deep", hosted: "3× standard", byok: "$0.05 per review" },
      { label: "Max", hosted: "10× standard", byok: "$0.15 per review" },
      { label: "Chat reply", hosted: "$0.05", byok: "free" },
    ]);
  });

  it("summarises competitors from whatever fields they publish", () => {
    expect(competitorRow(pricing.competitors[0])).toEqual({
      name: "Greptile Pro",
      seat: "$30.00 / seat / month",
      usage: "50 credits per seat; $1.00 per extra credit; base 1 · plus 3 · apex 10 credits per review",
    });
    expect(competitorRow(pricing.competitors[1])).toEqual({
      name: "CodeRabbit Essentials",
      seat: "$24.00–$30.00 / seat / month",
      usage: "$0.25 per file over the limit",
    });
    expect(competitorRow({ name: "X" })).toEqual({ name: "X", seat: "—", usage: "—" });
  });

  it("prices hosted reviews like the server", () => {
    expect(hostedPrice(pricing, "standard", 1)).toBe(0.25);
    expect(hostedPrice(pricing, "standard", 7)).toBe(0.7);
    expect(hostedPrice(pricing, "deep", 7)).toBe(2.1);
    expect(hostedPrice(pricing, "max", 7)).toBe(7);
  });
});

describe("search", () => {
  it("groups matches by file in first-seen order, lines sorted", () => {
    const g = groupMatches([
      { path: "b.rs", line: 9, text: "x" },
      { path: "a.rs", line: 3, text: "y" },
      { path: "b.rs", line: 2, text: "z" },
    ]);
    expect(g.map((f) => f.path)).toEqual(["b.rs", "a.rs"]);
    expect(g[0].matches.map((m) => m.line)).toEqual([2, 9]);
    expect(groupMatches([])).toEqual([]);
  });

  it("highlights literal matches case-insensitively by default", () => {
    expect(highlightRanges("Foo foo FOO", "foo")).toEqual([
      [0, 3],
      [4, 7],
      [8, 11],
    ]);
    expect(highlightRanges("Foo foo", "foo", { case: true })).toEqual([[4, 7]]);
    // Regex characters in a literal query are literal.
    expect(highlightRanges("a.b axb", "a.b")).toEqual([[0, 3]]);
  });

  it("highlights regex matches, skips empty ones and survives bad patterns", () => {
    expect(highlightRanges("fn main() fn x()", "fn \\w+", { regex: true })).toEqual([
      [0, 7],
      [10, 14],
    ]);
    expect(highlightRanges("abc", "x*", { regex: true })).toEqual([]);
    expect(highlightRanges("abc", "(", { regex: true })).toEqual([]);
    expect(highlightRanges("abc", "")).toEqual([]);
    expect(highlightRanges("aaaa", "a", {}, 2)).toHaveLength(2);
    expect(validRegex("(")).toBe(false);
    expect(validRegex("a|b")).toBe(true);
  });

  it("splits text into plain and highlighted pieces", () => {
    expect(splitHighlights("let foo = foo;", [[4, 7], [10, 13]])).toEqual([
      { text: "let ", hit: false },
      { text: "foo", hit: true },
      { text: " = ", hit: false },
      { text: "foo", hit: true },
      { text: ";", hit: false },
    ]);
    expect(splitHighlights("abc", [])).toEqual([{ text: "abc", hit: false }]);
    expect(splitHighlights("", [])).toEqual([{ text: "", hit: false }]);
    expect(splitHighlights("ab", [[0, 2]])).toEqual([{ text: "ab", hit: true }]);
  });
});

describe("reviewer settings form", () => {
  it("round-trips without changes", () => {
    const s = settings();
    expect(formPatch(toForm(s), s)).toEqual({});
  });

  it("sends only what changed, with lists split by line and blanks as null", () => {
    const s = settings();
    const f = toForm(s);
    f.mode = "max";
    f.ignore = "**/*.lock\n\n  vendor/**  \n";
    f.prompt = "Be terse.";
    f.model = "  ";
    f.effort = "high";
    expect(formPatch(f, s)).toEqual({ mode: "max", ignore: ["**/*.lock", "vendor/**"], prompt: "Be terse.", effort: "high" });
  });

  it("sends the whole auto block when any of it changes", () => {
    const s = settings();
    const f = toForm(s);
    f.max_paths = "**/crypto/**";
    f.standard_below_lines = "120";
    expect(formPatch(f, s)).toEqual({
      auto: { deep_paths: ["crates/*/src/auth/**"], max_paths: ["**/crypto/**"], standard_below_lines: 120, classifier: "heuristic" },
    });
  });

  it("clears optional strings to null and drops incomplete instructions", () => {
    const s = settings({ prompt: "old", model: "m", prompt_file: ".roll/p.md" });
    const f = toForm(s);
    f.prompt = "";
    f.model = "";
    f.prompt_file = "";
    f.instructions = [...f.instructions, { path: "", text: "" }];
    expect(formPatch(f, s)).toEqual({ prompt: null, model: null, prompt_file: null });
  });

  it("flags fields the server would reject", () => {
    const f = toForm(settings());
    expect(formErrors(f)).toEqual({});
    f.standard_below_lines = "eighty";
    f.max_files = "0";
    f.instructions = [{ path: "x/**", text: "" }];
    expect(Object.keys(formErrors(f)).sort()).toEqual(["instructions", "max_files", "standard_below_lines"]);
  });

  it("knows which keys the repo file sets", () => {
    expect([...fileKeys({ file: { mode: "deep", speed: "fast" } })]).toEqual(["mode", "speed"]);
    expect(fileKeys({ file: null }).size).toBe(0);
  });
});

describe("search at a revision", () => {
  it("links results to the changeset searched, else the indexed head", () => {
    expect(searchLinkRev({ rev: "cs_b", head: "cs_a" })).toBe("cs_b");
    expect(searchLinkRev({ rev: null, head: "cs_a" })).toBe("cs_a");
    expect(searchLinkRev({ head: null })).toBe("HEAD");
  });

  it("says how much was searched at the revision", () => {
    expect(overlayNote({ overlay: null })).toBeNull();
    expect(overlayNote({})).toBeNull();
    expect(overlayNote({ overlay: { base: "cs_a", files: 3 } }, "main")).toBe("3 files searched at this change, the rest from main.");
    expect(overlayNote({ overlay: { base: "cs_a", files: 1 } }, "trunk", "this revision")).toBe("1 file searched at this revision, the rest from trunk.");
    expect(overlayNote({ overlay: { base: "cs_a", files: 0 } }, "main")).toBe("Nothing in this change differs from main: results come from main's index.");
  });
});
