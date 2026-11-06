import { describe, expect, it } from "vitest";
import { hashFor, viewFromHash, type AutomationsView } from "./agents-surface";
import { backOf, depth, parentOf } from "./automations-nav";

const list: AutomationsView = { kind: "automations" };
const runs: AutomationsView = { kind: "automations", list: "runs" };
const overview: AutomationsView = { kind: "automations", open: "aut_1" };
const edit: AutomationsView = { kind: "automations", open: "aut_1", tab: "edit" };
const settings: AutomationsView = { kind: "automations", open: "aut_1", tab: "settings" };
const history: AutomationsView = { kind: "automations", open: "aut_1", tab: "runs" };
const run: AutomationsView = { kind: "automations", open: "aut_1", tab: "runs", run: "arn_9" };
const thread: AutomationsView = { ...run, thread: "thr_1" };

describe("the automations sheet's stack", () => {
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

  it("deep-links every level, the conversation under its run too", () => {
    for (const v of [list, runs, overview, edit, settings, history, run, thread]) expect(viewFromHash(hashFor(v))).toEqual(v);
    expect(hashFor(thread)).toBe("#agents/automations/aut_1/runs/arn_9/thr_1");
    expect(hashFor(edit)).toBe("#agents/automations/aut_1/edit");
  });
});
