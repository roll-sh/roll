import { describe, expect, it } from "vitest";
import type { Command, Reply } from "./agents-api";
import { planBulk, retryPlan, runBulk, skipSummary, summarize, undoPlan } from "./agents-bulk";
import { flatten, groupRows, namesOf, needsMe, nextOf, orderOf, rowsOf, sortRows } from "./agents-collection";
import { rangeTo, toggle, toggleAll, prune, type Selection } from "./agents-selection";
import { applyEvents, emptyData } from "./agents-store";
import { addView, encodeDoc, parseDoc, setViewDisplay } from "./agents-views";
import { approval, ev, machine, project, question, run, T0, thread } from "@/test/agents";

const NOW = T0 + 60_000;

function sample() {
  return applyEvents(emptyData(), [
    { type: "machine", cursor: 1, machine: machine() },
    { type: "machine", cursor: 2, machine: machine({ id: "mch_2", name: "build-box" }) },
    ev("project", project(), 3),
    ev("project", project({ id: "prj_2", name: "api" }), 4, "mch_2"),
    ev("thread", thread({ id: "thr_1", title: "Running one", status: "running", active_run: "run_1", updated_at: T0 + 5 }), 5),
    ev("thread", thread({ id: "thr_2", title: "Asks to run", status: "waiting", pending_requests: 1, updated_at: T0 + 4 }), 6),
    ev("thread", thread({ id: "thr_3", title: "Asks a question", status: "waiting", pending_requests: 1, updated_at: T0 + 3 }), 7),
    ev("thread", thread({ id: "thr_4", project: "prj_2", title: "Broken", status: "error", error: "boom", updated_at: T0 + 2 }), 8, "mch_2"),
    ev("thread", thread({ id: "thr_5", project: "prj_2", title: "done", updated_at: T0 + 1, worktree: { path: "/w", branch: "b", base: "main", removed_at: null } }), 9, "mch_2"),
    ev("thread", thread({ id: "thr_6", title: "Settled", inbox: { settle: { state: "settled", at: T0, by: "user" }, snooze: null, woke_at: null, visited_at: null, manual: false } }), 10),
    ev("run", run(), 11),
    ev("run", run({ id: "run_5", thread: "thr_5", status: "completed", stat: { files: 1, additions: 3, deletions: 1 } }), 12, "mch_2"),
    ev("request", approval({ id: "req_2", thread: "thr_2", created_at: T0 + 50 }), 13),
    ev("request", question({ id: "req_3", thread: "thr_3", created_at: T0 + 10 }), 14),
  ]);
}

const opts = { now: NOW, showArchived: false, filter: {}, focused: null };

describe("the collection: rows, grouping, sorting", () => {
  it("lists what the canvas shows, grouped by status with every column and sorted inside", () => {
    const data = sample();
    const rows = rowsOf(data, opts);
    // Settled threads are on the shelf, as on the canvas.
    expect(rows.map((r) => r.id).sort()).toEqual(["thr_1", "thr_2", "thr_3", "thr_4", "thr_5"]);
    const names = namesOf(data);
    const groups = groupRows(sortRows(rows, { key: "activity", dir: "desc" }, names), "status", "none", names);
    expect(groups.map((g) => [g.label, g.rows.map((r) => r.id)])).toEqual([
      ["Needs you", ["thr_2", "thr_3"]],
      ["Running", ["thr_1"]],
      ["Queued", []],
      ["Failed", ["thr_4"]],
      ["Done", ["thr_5"]],
    ]);
    // The list leaves the empty column out; a collapsed group keeps its header only.
    const items = flatten(groups, new Set(["status:failed"]));
    expect(items.map((i) => (i.kind === "group" ? `# ${i.group.label} ${i.group.count}` : i.row.id))).toEqual(["# Needs you 2", "thr_2", "thr_3", "# Running 1", "thr_1", "# Failed 1", "# Done 1", "thr_5"]);
    expect(orderOf(items)).toEqual(["thr_2", "thr_3", "thr_1", "thr_5"]);
  });

  it("sub-groups and sorts by another key; a filter and the focus narrow it as on the canvas", () => {
    const data = sample();
    const names = namesOf(data);
    const rows = sortRows(rowsOf(data, opts), { key: "title", dir: "asc" }, names);
    const groups = groupRows(rows, "machine", "project", names);
    expect(groups.map((g) => [g.label, g.count, g.groups!.map((s) => s.label)])).toEqual([
      ["ada-mbp", 3, ["roll"]],
      ["build-box", 2, ["api"]],
    ]);
    expect(groups[1].groups![0].rows.map((r) => r.thread.title)).toEqual(["Broken", "done"]);
    expect(rowsOf(data, { ...opts, filter: { status: ["waiting"] } }).map((r) => r.id).sort()).toEqual(["thr_2", "thr_3"]);
    expect(rowsOf(data, { ...opts, focused: "prj_2" }).map((r) => r.id).sort()).toEqual(["thr_4", "thr_5"]);
  });

  it("walks what needs you longest-waiting first, round to the start", () => {
    const data = sample();
    const order = needsMe(data, rowsOf(data, opts));
    expect(order).toEqual(["thr_3", "thr_2"]);
    expect(nextOf(order, null)).toBe("thr_3");
    expect(nextOf(order, "thr_3")).toBe("thr_2");
    expect(nextOf(order, "thr_2")).toBe("thr_3");
    expect(nextOf([], "thr_2")).toBeNull();
  });
});

describe("the selection", () => {
  it("toggles, ranges from the anchor in the order shown, selects a group and lets go of gone threads", () => {
    const order = ["a", "b", "c", "d", "e"];
    let s: Selection = { ids: new Set(), anchor: null };
    s = toggle(s, "b");
    s = rangeTo(s, "d", order);
    expect([...s.ids].sort()).toEqual(["b", "c", "d"]);
    expect(s.anchor).toBe("b");
    // Upwards from the same anchor adds, never drops.
    s = rangeTo(s, "a", order);
    expect([...s.ids].sort()).toEqual(["a", "b", "c", "d"]);
    // An anchor this order doesn't have (picked in another view): just the one.
    expect([...rangeTo({ ids: new Set(["z"]), anchor: "z" }, "c", order).ids].sort()).toEqual(["c", "z"]);
    s = toggle(s, "b");
    expect(s.ids.has("b")).toBe(false);
    // A group's checkbox: all on, then (all on) all off.
    s = toggleAll(s, ["d", "e"]);
    expect([...s.ids].sort()).toEqual(["a", "c", "d", "e"]);
    s = toggleAll(s, ["d", "e"]);
    expect([...s.ids].sort()).toEqual(["a", "c"]);
    expect([...prune(s, (id) => id !== "c").ids]).toEqual(["a"]);
  });
});

describe("bulk actions", () => {
  const reply = (r: Reply): Promise<Reply> => Promise.resolve(r);

  it("plans one step per thread it applies to and says why it skips the rest", () => {
    const data = sample();
    const all = ["thr_1", "thr_2", "thr_3", "thr_4", "thr_5", "thr_6", "thr_gone"];
    const settle = planBulk(data, all, { kind: "settle" });
    expect(settle.steps.map((s) => s.thread)).toEqual(["thr_4", "thr_5"]);
    expect(skipSummary(settle.skipped)).toBe("2 · It's waiting for your answer; 1 · It's still running; 1 · It's settled already; 1 · It's gone");
    // Approving answers approvals; a question is left for the conversation.
    const approve = planBulk(data, all, { kind: "approve" });
    expect(approve.steps.map((s) => s.command)).toEqual([{ type: "request.respond", request: "req_2", response: { type: "decision", decision: "accept" } }]);
    expect(approve.skipped.find((s) => s.thread === "thr_3")?.why).toMatch(/question/);
    expect(planBulk(data, all, { kind: "stop" }).steps.map((s) => s.command)).toEqual([{ type: "run.interrupt", thread: "thr_1" }]);
    expect(planBulk(data, all, { kind: "publish" }).steps.map((s) => s.thread)).toEqual(["thr_5"]);
  });

  it("sends each step once with its own id, summarizes, undoes what went through and retries only what failed", async () => {
    const data = sample();
    const plan = planBulk(data, ["thr_4", "thr_5", "thr_1"], { kind: "archive" });
    const sent: { command: Command; id: string }[] = [];
    let fail = true;
    const send = (_m: string, command: Command, id: string) => {
      sent.push({ command, id });
      if (command.type === "thread.archive" && command.thread === "thr_5" && fail) return Promise.reject(new Error("timed out"));
      return reply({ type: "ok" } as Reply);
    };
    const result = await runBulk(plan, send);
    expect(summarize(result)).toBe("1 archived · 1 failed (timed out) · 1 skipped");
    expect(new Set(sent.map((s) => s.id)).size).toBe(2);
    // Undo: only what went through.
    expect(undoPlan(result)!.steps.map((s) => s.command)).toEqual([{ type: "thread.unarchive", thread: "thr_4" }]);
    // Retrying a step that never got an answer reuses its id: the daemon answers with its receipt
    // rather than archiving twice.
    const retry = retryPlan(result)!;
    fail = false;
    const again = await runBulk(retry.plan, send, { ids: retry.ids });
    expect(summarize(again)).toBe("1 archived");
    expect(sent.at(-1)!.id).toBe(sent.find((s) => s.command.type === "thread.archive" && s.command.thread === "thr_5")!.id);
    // A step the daemon refused is tried again with a fresh id.
    const refused = await runBulk(planBulk(data, ["thr_4"], { kind: "archive" }), () => reply({ type: "error", code: "invalid", message: "no" } as Reply));
    expect(summarize(refused)).toBe("1 failed (no)");
    const r2 = retryPlan(refused)!;
    expect(r2.ids.size).toBe(0);
  });
});

describe("a saved view's layout", () => {
  it("round-trips layout, grouping and sort through the document; defaults and junk stay out", () => {
    let doc = addView(parseDoc({}), "Board", { status: ["running"] }, "v_board");
    doc = setViewDisplay(doc, "v_board", { layout: "board", groupBy: "project", subGroupBy: "machine", sort: { key: "cost", dir: "desc" } });
    const back = parseDoc(JSON.parse(JSON.stringify(encodeDoc(doc))));
    expect(back.views.find((v) => v.id === "v_board")!.display).toEqual({ layout: "board", groupBy: "project", subGroupBy: "machine", sort: { key: "cost", dir: "desc" } });
    // Defaults aren't stored; junk and a sub-group equal to the group are dropped.
    expect(parseDoc({ views: [{ id: "v_x", name: "X", filter: {}, display: { layout: "grid", groupBy: "status", subGroupBy: "status", sort: { key: "activity", dir: "desc" } } }] }).views[0].display).toEqual({});
    expect(parseDoc({ views: [{ id: "v_x", name: "X", filter: {}, display: { groupBy: "label", sort: { key: "x", dir: "up" } } }] }).views[0].display).toEqual({});
  });
});
