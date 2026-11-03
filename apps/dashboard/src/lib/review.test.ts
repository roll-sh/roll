import { describe, expect, it } from "vitest";
import type { Review, ReviewRules, ReviewStatus, Thread } from "./code-api";
import { parsePatch, splitRows } from "./diff";
import {
  addDraft,
  applyState,
  approvalNotes,
  canSubmit,
  clampApprovals,
  codeownerRows,
  draftFor,
  draftsOnRow,
  emptyPending,
  endsSelection,
  inSelection,
  loadPending,
  newSideText,
  parseGlobs,
  pendingKey,
  placeSplit,
  placeUnified,
  removeDraft,
  reviewerBadges,
  reviewerName,
  reviewHeadline,
  reviewPayload,
  rowSpots,
  rulesPatch,
  savePending,
  selectionLabel,
  selectLine,
  stepThread,
  stripSuggestion,
  suggestionDiff,
  suggestionLines,
  threadOrder,
  threadsForFile,
  tightened,
  timeline,
  updateDraft,
  verdictLabel,
} from "./review";

// Old 1..6, new 1..7: line 3 changed, line 5 added after old 4.
const PATCH = [
  "@@ -1,6 +1,7 @@",
  " one",
  " two",
  "-three",
  "+THREE",
  " four",
  "+four and a half",
  " five",
  " six",
].join("\n");
const lines = parsePatch(PATCH);

const thread = (id: string, over: Partial<Thread> = {}): Thread => ({
  id,
  change: "chg_1",
  review: null,
  path: "a.txt",
  side: "new",
  line: 1,
  start_line: 1,
  revision: 2,
  original_line: 1,
  original_start_line: 1,
  original_revision: 1,
  outdated: false,
  resolved: false,
  resolved_by: null,
  resolved_at: null,
  source: "roll",
  created_at: 100,
  updated_at: 100,
  comments: [],
  ...over,
});

const rules = (over: Partial<ReviewRules> = {}): ReviewRules => ({
  required_approvals: 1,
  require_codeowners: false,
  count_ai_approval: true,
  block_on_request_changes: true,
  dismiss_stale: true,
  require_resolved_threads: false,
  allow_self_approval: false,
  auto_approve: [],
  ...over,
});

/** A Storage stand-in. */
function memoryStore() {
  const m = new Map<string, string>();
  return {
    m,
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
    removeItem: (k: string) => void m.delete(k),
  };
}

describe("threads in a unified diff", () => {
  it("puts each thread under the row showing its line on its side", () => {
    const changed = thread("t_new3", { line: 3, start_line: 3 }); // + THREE
    const removed = thread("t_old3", { side: "old", line: 3, start_line: 3 }); // - three
    const context = thread("t_ctx", { line: 6, start_line: 5 }); // " five" is new 6
    const p = placeUnified(lines, [changed, removed, context]);
    const at = (id: string) => [...p.inline.entries()].find(([, ts]) => ts.some((t) => t.id === id))?.[0];
    expect(lines[at("t_new3")!]).toMatchObject({ kind: "add", new: 3 });
    expect(lines[at("t_old3")!]).toMatchObject({ kind: "del", old: 3 });
    expect(lines[at("t_ctx")!]).toMatchObject({ kind: "ctx", new: 6, old: 5 });
    expect(p.outdated).toEqual([]);
    expect(p.offDiff).toEqual([]);
  });

  it("keeps two threads on one line together, in order", () => {
    const a = thread("a", { line: 2, created_at: 1 });
    const b = thread("b", { line: 2, created_at: 2 });
    const p = placeUnified(lines, threadsForFile({ path: "a.txt", old_path: null }, [b, a]));
    expect([...p.inline.values()]).toEqual([[a, b]]);
  });

  it("sends outdated threads and threads off the hunks below the file", () => {
    const old = thread("old", { outdated: true, line: 3 });
    const far = thread("far", { line: 40, start_line: 38 });
    const removedSideOnAdd = thread("wrong-side", { side: "old", line: 7 }); // old 7 doesn't exist
    const p = placeUnified(lines, [old, far, removedSideOnAdd]);
    expect(p.inline.size).toBe(0);
    expect(p.outdated).toEqual([old]);
    expect(p.offDiff).toEqual([far, removedSideOnAdd]);
  });
});

describe("threads in a split diff", () => {
  it("puts old-side threads on the left row and new-side ones on the right", () => {
    const rows = splitRows(lines);
    const p = placeSplit(rows, [thread("n", { line: 3 }), thread("o", { side: "old", line: 3 }), thread("added", { line: 5 })]);
    const rowOf = (id: string) => [...p.inline.entries()].find(([, ts]) => ts.some((t) => t.id === id))![0];
    // The changed line pairs old 3 with new 3 on one split row.
    expect(rowOf("n")).toBe(rowOf("o"));
    const [left, right] = rows[rowOf("n")];
    expect(left).toMatchObject({ kind: "del", old: 3 });
    expect(right).toMatchObject({ kind: "add", new: 3 });
    // An added line with nothing removed opposite it sits alone on its row.
    const [l5, r5] = rows[rowOf("added")];
    expect(l5).toBeNull();
    expect(r5).toMatchObject({ kind: "add", new: 5 });
  });

  it("treats outdated and off-diff threads like the unified view", () => {
    const p = placeSplit(splitRows(lines), [thread("x", { outdated: true }), thread("y", { line: 99 })]);
    expect(p.outdated.map((t) => t.id)).toEqual(["x"]);
    expect(p.offDiff.map((t) => t.id)).toEqual(["y"]);
  });
});

describe("threads per file", () => {
  it("matches the path, and old-side threads on a renamed file's old path", () => {
    const ts = [
      thread("here", { path: "new.txt" }),
      thread("old-side", { path: "old.txt", side: "old" }),
      thread("new-side-on-old-path", { path: "old.txt", side: "new" }),
      thread("elsewhere", { path: "b.txt" }),
    ];
    expect(threadsForFile({ path: "new.txt", old_path: "old.txt" }, ts).map((t) => t.id)).toEqual(["here", "old-side"]);
    expect(threadsForFile({ path: "new.txt", old_path: null }, ts).map((t) => t.id)).toEqual(["here"]);
  });

  it("reads new-side text, or nothing when a line isn't in the diff", () => {
    expect(newSideText(lines, 3, 5)).toEqual(["THREE", "four", "four and a half"]);
    expect(newSideText(lines, 7, 7)).toEqual(["six"]);
    expect(newSideText(lines, 6, 9)).toBeNull();
  });
});

describe("suggestions", () => {
  it("splits a suggestion into lines, dropping the fence's trailing newline", () => {
    expect(suggestionLines("a\nb\n")).toEqual(["a", "b"]);
    expect(suggestionLines("a\r\nb")).toEqual(["a", "b"]);
    expect(suggestionLines("")).toEqual([]);
    expect(suggestionLines("\n")).toEqual([""]);
  });

  it("diffs a suggestion against the lines it replaces, keeping shared edges as context", () => {
    expect(suggestionDiff(["let a = 1;", "let b = 2;", "return a;"], "let a = 1;\nlet b = 3;\nreturn a;")).toEqual([
      { kind: "ctx", text: "let a = 1;" },
      { kind: "del", text: "let b = 2;" },
      { kind: "add", text: "let b = 3;" },
      { kind: "ctx", text: "return a;" },
    ]);
  });

  it("shows deletions, insertions and unknown originals", () => {
    expect(suggestionDiff(["x", "y"], "")).toEqual([
      { kind: "del", text: "x" },
      { kind: "del", text: "y" },
    ]);
    expect(suggestionDiff(["x"], "x\nnew")).toEqual([
      { kind: "ctx", text: "x" },
      { kind: "add", text: "new" },
    ]);
    expect(suggestionDiff(null, "a\nb")).toEqual([
      { kind: "add", text: "a" },
      { kind: "add", text: "b" },
    ]);
    // Identical: nothing changes.
    expect(suggestionDiff(["same"], "same").map((l) => l.kind)).toEqual(["ctx"]);
  });

  it("does not double-count a line shared by both edges", () => {
    // "a" is both the common prefix and suffix candidate: it must appear once.
    expect(suggestionDiff(["a"], "a\na")).toEqual([
      { kind: "ctx", text: "a" },
      { kind: "add", text: "a" },
    ]);
  });

  it("strips the suggestion block from a comment body, keeping the words around it", () => {
    expect(stripSuggestion("Use a const here.\n\n```suggestion\nconst a = 1;\n```\n\nThanks")).toBe("Use a const here.\n\nThanks");
    expect(stripSuggestion("```suggestion\nx\n```")).toBe("");
    expect(stripSuggestion("~~~~ suggestion\nx\n~~~~")).toBe("");
    expect(stripSuggestion("```ts\nconst a = 1;\n```")).toBe("```ts\nconst a = 1;\n```");
  });

  it("knows when a suggestion can be applied, and why not", () => {
    const t = { outdated: false, side: "new" as const, resolved: false };
    expect(applyState({ thread: t, suggestion: "x", writer: true, open: true })).toEqual({ can: true, reason: null });
    expect(applyState({ thread: t, suggestion: null, writer: true, open: true })).toEqual({ can: false, reason: null });
    expect(applyState({ thread: t, suggestion: "x", writer: false, open: true }).reason).toMatch(/writers/);
    expect(applyState({ thread: t, suggestion: "x", writer: true, open: false }).reason).toMatch(/isn't open/);
    expect(applyState({ thread: { ...t, outdated: true }, suggestion: "x", writer: true, open: true }).reason).toMatch(/changed/);
    expect(applyState({ thread: { ...t, side: "old" }, suggestion: "x", writer: true, open: true }).reason).toMatch(/removed/);
  });
});

describe("review state", () => {
  it("words every state", () => {
    expect(reviewHeadline({ state: "approved", approvals: 2, required: 2 })).toEqual({ label: "Approved", detail: "2 of 2 approvals", tone: "ok" });
    expect(reviewHeadline({ state: "approved", approvals: 1, required: 0 })).toMatchObject({ detail: "1 approval" });
    expect(reviewHeadline({ state: "approved", approvals: 0, required: 0 })).toMatchObject({ detail: "no approval required" });
    expect(reviewHeadline({ state: "auto_approved", approvals: 0, required: 1 })).toMatchObject({ label: "Auto-approved", tone: "ok" });
    expect(reviewHeadline({ state: "changes_requested", approvals: 1, required: 2 })).toEqual({
      label: "Changes requested",
      detail: "1 of 2 approvals",
      tone: "danger",
    });
    expect(reviewHeadline({ state: "pending", approvals: 0, required: 1 })).toEqual({ label: "Review required", detail: "0 of 1 approval", tone: "accent" });
    expect(reviewHeadline({ state: "pending", approvals: 0, required: 0 })).toMatchObject({ label: "Awaiting review", tone: "muted" });
  });

  it("lists code owners still owed first", () => {
    const status = {
      codeowners: [
        { path_pattern: "docs/*", owners: ["@docs"], paths: ["docs/a.md"], satisfied: true },
        { path_pattern: "crates/**", owners: ["@acme", "agent:infra"], paths: ["crates/a.rs", "crates/b.rs"], satisfied: false },
        { path_pattern: "*.lock", owners: [], paths: ["x.lock"], satisfied: false },
      ],
    } as Pick<ReviewStatus, "codeowners">;
    expect(codeownerRows(status)).toEqual([
      { pattern: "crates/**", owners: "@acme, agent:infra", satisfied: false, files: 2 },
      { pattern: "*.lock", owners: "no owners", satisfied: false, files: 1 },
      { pattern: "docs/*", owners: "@docs", satisfied: true, files: 1 },
    ]);
  });

  it("explains approvals that don't count", () => {
    const notes = approvalNotes({
      approvals: [
        { reviewer: { kind: "user", name: "a", app: null }, review: "rv_1", stale: false, counted: true, reason: null },
        { reviewer: { kind: "user", name: "b", app: null }, review: "rv_2", stale: false, counted: false, reason: "self-approval" },
      ],
    });
    expect(notes.get("rv_2")).toEqual({ counted: false, reason: "self-approval" });
    expect(approvalNotes(null).size).toBe(0);
  });

  it("names reviewers, GitHub bots without [bot]", () => {
    expect(reviewerName({ name: "coderabbitai[bot]" })).toBe("coderabbitai");
    expect(reviewerName("greptile-apps[bot]")).toBe("greptile-apps");
    expect(reviewerName({ name: "ana" })).toBe("ana");
    expect(reviewerBadges({ kind: "app", name: "coderabbitai[bot]", app: null }, "github")).toEqual(["app", "github"]);
    expect(reviewerBadges({ kind: "agent", name: "fixer", app: null }, "roll")).toEqual(["agent"]);
    expect(verdictLabel("request_changes")).toBe("requested changes");
  });

  it("orders the timeline oldest first, stably", () => {
    const r = (id: string, created_at: number) => ({ id, created_at }) as Review;
    expect(timeline([r("b", 2), r("c", 1), r("a", 2)]).map((x) => x.id)).toEqual(["c", "a", "b"]);
  });
});

describe("selecting lines", () => {
  it("selects one line, then extends on shift-click within a file and side", () => {
    const one = selectLine(null, { path: "a", side: "new", line: 10 }, false);
    expect(one).toEqual({ path: "a", side: "new", start: 10, end: 10 });
    expect(selectLine(one, { path: "a", side: "new", line: 14 }, true)).toEqual({ path: "a", side: "new", start: 10, end: 14 });
    expect(selectLine(one, { path: "a", side: "new", line: 6 }, true)).toEqual({ path: "a", side: "new", start: 6, end: 10 });
    // Another file, another side, or no shift: start over.
    expect(selectLine(one, { path: "b", side: "new", line: 14 }, true)).toEqual({ path: "b", side: "new", start: 14, end: 14 });
    expect(selectLine(one, { path: "a", side: "old", line: 14 }, true)).toEqual({ path: "a", side: "old", start: 14, end: 14 });
    expect(selectLine(one, { path: "a", side: "new", line: 14 }, false)).toEqual({ path: "a", side: "new", start: 14, end: 14 });
  });

  it("keeps the anchor when a range is extended again", () => {
    const range = { path: "a", side: "new" as const, start: 10, end: 14 };
    expect(selectLine(range, { path: "a", side: "new", line: 20 }, true)).toEqual({ ...range, end: 20 });
    expect(selectLine(range, { path: "a", side: "new", line: 8 }, true)).toEqual({ ...range, start: 8, end: 14 });
  });

  it("knows which rows are selected and where the composer goes", () => {
    const sel = { path: "a.txt", side: "new" as const, start: 3, end: 5 };
    expect(inSelection(sel, "a.txt", "new", 4)).toBe(true);
    expect(inSelection(sel, "a.txt", "old", 4)).toBe(false);
    expect(inSelection(sel, "b.txt", "new", 4)).toBe(false);
    expect(inSelection(null, "a.txt", "new", 4)).toBe(false);
    const row5 = lines.find((l) => l.new === 5)!;
    expect(endsSelection(sel, "a.txt", row5)).toBe(true);
    expect(endsSelection(sel, "a.txt", row5, "old")).toBe(false);
    expect(endsSelection(sel, "a.txt", lines.find((l) => l.new === 4)!)).toBe(false);
    expect(rowSpots(lines[0])).toEqual([]); // the hunk header
    expect(rowSpots(lines.find((l) => l.kind === "ctx")!)).toEqual([
      { side: "old", line: 1 },
      { side: "new", line: 1 },
    ]);
    expect(selectionLabel(sel)).toBe("L3–L5");
    expect(selectionLabel({ start: 7, end: 7 })).toBe("L7");
  });
});

describe("the pending review", () => {
  it("adds, edits and removes draft comments without touching the original", () => {
    const p0 = emptyPending("chg_1", 2);
    const p1 = addDraft(p0, draftFor({ path: "a.txt", side: "new", start: 3, end: 5 }, "range"), "d1");
    const p2 = addDraft(p1, draftFor({ path: "a.txt", side: "old", start: 3, end: 3 }, "single"), "d2");
    expect(p0.comments).toEqual([]);
    expect(p2.comments).toEqual([
      { id: "d1", path: "a.txt", side: "new", line: 5, start_line: 3, body: "range" },
      { id: "d2", path: "a.txt", side: "old", line: 3, body: "single" },
    ]);
    expect(updateDraft(p2, "d2", "edited").comments[1].body).toBe("edited");
    expect(removeDraft(p2, "d1").comments.map((c) => c.id)).toEqual(["d2"]);
  });

  it("finds the drafts drawn under a row", () => {
    const p = addDraft(addDraft(emptyPending("c", 1), { path: "a.txt", side: "old", line: 3, body: "o" }, "o"), { path: "a.txt", line: 3, body: "n" }, "n");
    const del3 = lines.find((l) => l.kind === "del")!;
    const add3 = lines.find((l) => l.kind === "add" && l.new === 3)!;
    expect(draftsOnRow(p, "a.txt", del3).map((d) => d.id)).toEqual(["o"]);
    expect(draftsOnRow(p, "a.txt", add3).map((d) => d.id)).toEqual(["n"]);
    expect(draftsOnRow(p, "a.txt", add3, "old")).toEqual([]);
    expect(draftsOnRow(p, "b.txt", add3)).toEqual([]);
  });

  it("round-trips through storage, removes empty drafts, and flags an older revision", () => {
    const store = memoryStore();
    const p = { ...addDraft(emptyPending("chg_1", 2), { path: "a", line: 1, body: "x" }, "d"), body: "summary" };
    savePending(store, p);
    expect(JSON.parse(store.m.get(pendingKey("chg_1"))!)).toEqual(p);
    expect(loadPending(store, "chg_1", 2)).toEqual({ pending: p, stale: false });
    expect(loadPending(store, "chg_1", 3)).toEqual({ pending: p, stale: true });
    savePending(store, emptyPending("chg_1", 3));
    expect(store.m.has(pendingKey("chg_1"))).toBe(false);
    expect(loadPending(store, "chg_1", 3)).toEqual({ pending: emptyPending("chg_1", 3), stale: false });
  });

  it("ignores corrupt or foreign drafts", () => {
    const store = memoryStore();
    store.setItem(pendingKey("chg_1"), "{not json");
    expect(loadPending(store, "chg_1", 1).pending).toEqual(emptyPending("chg_1", 1));
    store.setItem(pendingKey("chg_1"), JSON.stringify({ change: "chg_2", revision: 1, comments: [] }));
    expect(loadPending(store, "chg_1", 1).pending).toEqual(emptyPending("chg_1", 1));
  });

  it("builds the submit body and knows when it can be sent", () => {
    const p = { ...addDraft(addDraft(emptyPending("c", 1), { path: "a", line: 2, body: "  keep  " }, "x"), { path: "a", line: 3, body: "   " }, "y"), body: "  " };
    expect(reviewPayload(p, "comment")).toEqual({ verdict: "comment", comments: [{ path: "a", line: 2, body: "keep" }] });
    expect(reviewPayload({ ...emptyPending("c", 1), body: " LGTM " }, "approve")).toEqual({ verdict: "approve", body: "LGTM" });
    expect(reviewPayload(emptyPending("c", 1), "approve")).toEqual({ verdict: "approve" });
    expect(canSubmit("approve", "", 0)).toBe(true);
    expect(canSubmit("comment", "", 0)).toBe(false);
    expect(canSubmit("request_changes", " ", 0)).toBe(false);
    expect(canSubmit("request_changes", "", 1)).toBe(true);
    expect(canSubmit("comment", "hi", 0)).toBe(true);
  });
});

describe("keyboard order", () => {
  const files = [
    { path: "b.txt", old_path: null },
    { path: "a.txt", old_path: null },
  ];

  it("walks unresolved threads in file order, then by line", () => {
    const ts = [
      thread("a2", { path: "a.txt", line: 2 }),
      thread("b9", { path: "b.txt", line: 9 }),
      thread("a1", { path: "a.txt", line: 1 }),
      thread("done", { path: "a.txt", line: 5, resolved: true }),
      thread("gone", { path: "zzz.txt", line: 1 }),
    ];
    expect(threadOrder(files, ts)).toEqual(["b9", "a1", "a2", "gone"]);
  });

  it("walks resolved ones when nothing is open", () => {
    expect(threadOrder(files, [thread("r1", { resolved: true, path: "a.txt" })])).toEqual(["r1"]);
    expect(threadOrder(files, [])).toEqual([]);
  });

  it("steps forward and back, wrapping, from nowhere to the ends", () => {
    const order = ["x", "y", "z"];
    expect(stepThread(order, null, 1)).toBe("x");
    expect(stepThread(order, null, -1)).toBe("z");
    expect(stepThread(order, "x", 1)).toBe("y");
    expect(stepThread(order, "z", 1)).toBe("x");
    expect(stepThread(order, "x", -1)).toBe("z");
    expect(stepThread(order, "gone", 1)).toBe("x");
    expect(stepThread([], "x", 1)).toBeNull();
  });
});

describe("settings", () => {
  it("patches only what changed", () => {
    expect(rulesPatch(rules(), rules())).toEqual({});
    expect(rulesPatch(rules(), rules({ required_approvals: 2, dismiss_stale: false }))).toEqual({ required_approvals: 2, dismiss_stale: false });
    expect(rulesPatch(rules(), rules({ auto_approve: [{ paths: ["docs/**"], comment_only: false }] }))).toEqual({
      auto_approve: [{ paths: ["docs/**"], comment_only: false }],
    });
  });

  it("names the rules the file tightened", () => {
    expect(tightened(rules(), rules({ required_approvals: 2, require_codeowners: true }))).toEqual(["required_approvals", "require_codeowners"]);
    expect(tightened(rules(), rules({ auto_approve: [{ paths: ["x"], comment_only: true }] }))).toEqual([]);
  });

  it("parses globs and clamps approvals", () => {
    expect(parseGlobs("docs/**, **/*.md\n  ,docs/**")).toEqual(["docs/**", "**/*.md"]);
    expect(parseGlobs("")).toEqual([]);
    expect([-1, 0, 3.6, 21, Number.NaN].map(clampApprovals)).toEqual([0, 0, 4, 20, 0]);
  });
});
