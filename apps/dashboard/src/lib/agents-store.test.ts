import { describe, expect, it } from "vitest";
import type { State, StreamEvent } from "./agents-api";
import { applyEvents, applySnapshot, emptyData, follow, liveNode, nodeTree, pendingRequests, runsOf } from "./agents-store";
import { approval, ev, M1, machine, mirrored, node, project, run, thread } from "@/test/agents";

const state = (cursor: number, entities: State["entities"], machines = [machine()]): State => ({ cursor, machines, entities });

describe("agents store", () => {
  it("loads a snapshot and keeps the newest cursor per record", () => {
    let d = applySnapshot(emptyData(), state(5, [mirrored("project", project(), 1), mirrored("thread", thread(), 5)]));
    expect(d.cursor).toBe(5);
    expect(d.threads.get("thr_1")?.value.title).toBe("Fix the flaky queue test");

    // A replay of an older version changes nothing; a newer one wins.
    d = applyEvents(d, [ev("thread", thread({ title: "old" }), 3)]);
    expect(d.threads.get("thr_1")?.value.title).toBe("Fix the flaky queue test");
    d = applyEvents(d, [ev("thread", thread({ title: "new", status: "running" }), 7)]);
    expect(d.threads.get("thr_1")?.value).toMatchObject({ title: "new", status: "running" });
    expect(d.cursor).toBe(7);
  });

  it("returns the same data when a batch is empty, and shares untouched collections", () => {
    const d = applySnapshot(emptyData(), state(1, [mirrored("project", project(), 1)]));
    expect(applyEvents(d, [])).toBe(d);
    const e = applyEvents(d, [ev("thread", thread(), 2)]);
    expect(e.projects).toBe(d.projects);
    expect(e.threads).not.toBe(d.threads);
  });

  it("tombstones beat older upserts", () => {
    let d = applyEvents(emptyData(), [ev("thread", thread(), 2)]);
    d = applyEvents(d, [{ type: "entity", cursor: 4, machine: M1, entity: { kind: "removed", entity: "thread", id: "thr_1", thread: null } }]);
    expect(d.threads.has("thr_1")).toBe(false);
    d = applyEvents(d, [ev("thread", thread(), 3)]);
    expect(d.threads.has("thr_1")).toBe(false);
  });

  it("drops records a newer snapshot doesn't have, but keeps ones newer than it", () => {
    let d = applyEvents(emptyData(), [ev("thread", thread(), 2), ev("thread", thread({ id: "thr_2" }), 9)]);
    d = applySnapshot(d, state(6, []));
    expect(d.threads.has("thr_1")).toBe(false); // removed before the snapshot
    expect(d.threads.has("thr_2")).toBe(true); // arrived on the stream after it
    expect(d.cursor).toBe(9);
  });

  it("keeps detail only for followed threads", () => {
    const msg = node({ type: "message", text: "Looking" });
    let d = applyEvents(emptyData(), [ev("node", msg, 2)]);
    expect(d.nodes.size).toBe(0);
    d = follow(d, ["thr_1"]);
    d = applyEvents(d, [ev("node", msg, 3), ev("node", node({ type: "message", text: "x" }, { id: "nod_2", thread: "thr_2" }), 4)]);
    expect(d.nodes.get("thr_1")?.size).toBe(1);
    expect(d.nodes.has("thr_2")).toBe(false);
    d = follow(d, []);
    expect(d.nodes.size).toBe(0);
  });

  it("streams deltas onto a node until the durable record catches up", () => {
    let d = follow(emptyData(), ["thr_1"]);
    d = applyEvents(d, [ev("node", node({ type: "message", text: "Look" }), 2)]);
    const delta = (text: string): StreamEvent => ({ type: "delta", machine: M1, delta: { type: "text", thread: "thr_1", node: "nod_1", text } });
    d = applyEvents(d, [delta("ing at"), delta(" the test")]);
    const shown = () => {
      const n = liveNode(d, d.nodes.get("thr_1")!.get("nod_1")!.value);
      return n.body.type === "message" ? n.body.text : null;
    };
    expect(shown()).toBe("Looking at the test");

    // A durable upsert that's behind the deltas doesn't rewind the text.
    d = applyEvents(d, [ev("node", node({ type: "message", text: "Looking at" }), 3)]);
    expect(shown()).toBe("Looking at the test");
    // Once it catches up (or completes) the record is shown as is.
    d = applyEvents(d, [ev("node", node({ type: "message", text: "Looking at the test." }, { status: "completed" }), 4)]);
    expect(d.live.size).toBe(0);
    expect(shown()).toBe("Looking at the test.");
    // A late delta for a finished node is ignored.
    d = applyEvents(d, [delta(" again")]);
    expect(shown()).toBe("Looking at the test.");
  });

  it("streams tool output", () => {
    let d = follow(emptyData(), ["thr_1"]);
    const tool = node({ type: "tool", name: "Bash", kind: "command", title: "cargo test", input: null, output: null, exit_code: null, files: [] });
    d = applyEvents(d, [ev("node", tool, 2), { type: "delta", machine: M1, delta: { type: "output", thread: "thr_1", node: "nod_1", text: "running 3 tests\n" } }]);
    const n = liveNode(d, d.nodes.get("thr_1")!.get("nod_1")!.value);
    expect(n.body.type === "tool" && n.body.output).toBe("running 3 tests\n");
  });

  it("removes a machine with its records", () => {
    let d = applySnapshot(emptyData(), state(3, [mirrored("project", project(), 1), mirrored("thread", thread(), 2), mirrored("project", project({ id: "prj_2" }), 3, "mch_2")]));
    d = applyEvents(d, [{ type: "machine_removed", cursor: 4, machine: M1 }]);
    expect(d.machines.size).toBe(0);
    expect([...d.projects.keys()]).toEqual(["prj_2"]);
    expect(d.threads.size).toBe(0);
    d = applyEvents(d, [{ type: "machine", cursor: 2, machine: machine() }]);
    expect(d.machines.size).toBe(0);
  });

  it("orders runs, nodes (with subagent children) and pending requests", () => {
    let d = follow(emptyData(), ["thr_1"]);
    d = applyEvents(d, [
      ev("run", run({ id: "run_2", ordinal: 2 }), 1),
      ev("run", run({ id: "run_1", ordinal: 1, status: "completed" }), 2),
      ev("node", node({ type: "subagent", agent: "Explore", prompt: "find callers", child: null, result: null }, { id: "nod_s", ordinal: 2, run: "run_2" }), 3),
      ev("node", node({ type: "message", text: "hi" }, { id: "nod_m", ordinal: 1, run: "run_2" }), 4),
      ev("node", node({ type: "message", text: "inner" }, { id: "nod_c", ordinal: 3, run: "run_2", parent: "nod_s" }), 5),
      ev("request", approval(), 6),
      ev("request", approval({ id: "req_done", status: "resolved" }), 7),
    ]);
    expect(runsOf(d, "thr_1").map((r) => r.id)).toEqual(["run_1", "run_2"]);
    const tree = nodeTree(d, "thr_1", "run_2");
    expect(tree.map((t) => t.node.id)).toEqual(["nod_m", "nod_s"]);
    expect(tree[1].children.map((t) => t.node.id)).toEqual(["nod_c"]);
    expect(pendingRequests(d).map((r) => r.id)).toEqual(["req_1"]);
  });
});
