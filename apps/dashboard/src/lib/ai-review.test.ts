import { describe, expect, it } from "vitest";
import type { AiJob, Pricing, ReviewAi, ReviewerSettings } from "./code-api";
import { activeJob, aiHeader, fileKeys, formErrors, formPatch, groupMatches, highlightRanges, hostedPrice, lastFailure, findingOf, jobStage, jobSteps, parseFinding, searchLinkRev, toForm, validRegex } from "./ai-review";

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

});

describe("roll's findings", () => {
  it("parses severity, title, body and confidence", () => {
    const f = parseFinding("**bug** · Off by one\n\nThe loop skips the last item.\n\n<sub>confidence 84%</sub>");
    expect(f).toEqual({ severity: "bug", title: "Off by one", body: "The loop skips the last item.", confidence: 0.84 });
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

});

describe("usage and pricing", () => {
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

});
