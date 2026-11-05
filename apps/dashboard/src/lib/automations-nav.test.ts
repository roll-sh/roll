import { describe, expect, it } from "vitest";
import { hashFor, viewFromHash, type AutomationsView } from "./agents-surface";
import { backOf, depth, isBuilder, levelOf, parentOf, step, type Trail } from "./automations-nav";

const list: AutomationsView = { kind: "automations" };
const runs: AutomationsView = { kind: "automations", list: "runs" };
const overview: AutomationsView = { kind: "automations", open: "aut_1" };
const edit: AutomationsView = { kind: "automations", open: "aut_1", tab: "edit" };
const settings: AutomationsView = { kind: "automations", open: "aut_1", tab: "settings" };
const history: AutomationsView = { kind: "automations", open: "aut_1", tab: "runs" };
const run: AutomationsView = { kind: "automations", open: "aut_1", tab: "runs", run: "arn_9" };
const thread: AutomationsView = { ...run, thread: "thr_1" };

function walk(...views: AutomationsView[]) {
  let trail: Trail = [];
  const dirs: number[] = [];
  for (const v of views) {
    const s = step(trail, v);
    trail = s.trail;
    dirs.push(s.dir);
  }
  return { trail, dirs };
}

describe("the automations sheet's stack", () => {
  it("names each level, and the builder's tabs are one level", () => {
    expect(levelOf(list)).toEqual({ kind: "list" });
    expect(levelOf(runs)).toEqual({ kind: "list" });
    expect(levelOf(overview)).toEqual({ kind: "automation", id: "aut_1" });
    expect(levelOf(edit)).toEqual({ kind: "builder", id: "aut_1" });
    expect(levelOf(settings)).toEqual({ kind: "builder", id: "aut_1" });
    expect(levelOf({ kind: "automations", open: "new", template: "main-breaks" })).toEqual({ kind: "builder", id: "new" });
    expect(levelOf(history)).toEqual({ kind: "history", id: "aut_1" });
    expect(levelOf(run)).toEqual({ kind: "run", run: "arn_9" });
    expect(levelOf(thread)).toEqual({ kind: "thread", thread: "thr_1" });
    expect([edit, settings, { kind: "automations", open: "new" } as AutomationsView].every(isBuilder)).toBe(true);
    expect([list, overview, history, run].some(isBuilder)).toBe(false);
  });

  it("a deep link goes back up its parents, keeping the list's tab, and the list closes", () => {
    expect(parentOf(thread)).toEqual(run);
    expect(parentOf(run)).toEqual(overview);
    expect(parentOf({ kind: "automations", list: "runs", run: "arn_9" })).toEqual(runs);
    expect(parentOf({ kind: "automations", list: "runs", open: "aut_1", tab: "runs", run: "arn_9" })).toEqual({ kind: "automations", list: "runs", open: "aut_1" });
    expect(parentOf(edit)).toEqual(overview);
    expect(parentOf(history)).toEqual(overview);
    expect(parentOf({ kind: "automations", list: "templates", open: "new", template: "x" })).toEqual({ kind: "automations", list: "templates" });
    expect(parentOf(overview)).toEqual(list);
    expect(parentOf(list)).toBeNull();
    expect(backOf([thread])).toEqual(run);
    expect(backOf([list])).toBeNull();
    expect([list, overview, edit, run, thread].map(depth)).toEqual([0, 1, 2, 2, 3]);
  });

  it("goes back where you came from, and says which way each page moves", () => {
    const a = walk(list, overview, history, run);
    expect(a.dirs).toEqual([1, 1, 1, 1]);
    expect(backOf(a.trail)).toEqual(history);
    // Back to the overview from the run drops the history between.
    expect(walk(list, overview, history, run, overview)).toEqual({ trail: [list, overview], dirs: [1, 1, 1, 1, -1] });
    // A tab replaces the top.
    expect(walk(list, runs).trail).toEqual([runs]);
    expect(walk(list, overview, edit, settings)).toEqual({ trail: [list, overview, settings], dirs: [1, 1, 1, 0] });
    // The same view again changes nothing.
    expect(walk(list, list).dirs).toEqual([1, 0]);
    // Up past what was opened in between (an automation, then another from a link).
    const other: AutomationsView = { kind: "automations", open: "aut_2" };
    expect(walk(list, overview, run, other)).toEqual({ trail: [list, other], dirs: [1, 1, 1, -1] });
    // A new one, once created, is replaced by its overview: Back goes to the list.
    const created: AutomationsView = { kind: "automations", open: "aut_new" };
    expect(walk(list, { kind: "automations", open: "new" }, created)).toEqual({ trail: [list, created], dirs: [1, 1, 1] });
  });

  it("deep-links every level, the conversation under its run too", () => {
    for (const v of [list, runs, overview, edit, settings, history, run, thread]) expect(viewFromHash(hashFor(v))).toEqual(v);
    expect(hashFor(thread)).toBe("#agents/automations/aut_1/runs/arn_9/thr_1");
    expect(hashFor(edit)).toBe("#agents/automations/aut_1/edit");
  });
});
