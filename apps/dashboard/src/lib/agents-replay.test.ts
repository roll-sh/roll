import { describe, expect, it } from "vitest";
import type { Entity, Share } from "./agents-api";
import { editsDiff, frameAt, MAX_GAP, realAt, replayAt, SHARE_MACHINE, step, timeline } from "./agents-replay";
import { applySnapshot, emptyData, nodeTree, runsOf } from "./agents-store";
import { machine, mirrored, node, project, run, share, T0, thread } from "@/test/agents";

const at = (ms: number) => T0 + ms;

describe("replaying a share", () => {
  const s = share();
  const tl = timeline(s);

  it("shortens long pauses on the replay clock and maps both ways", () => {
    expect(tl.real[0]).toBe(at(0));
    expect(tl.real[tl.real.length - 1]).toBe(at(9000));
    // 2300 → 8000 is a 5.7 s wait for the approval: MAX_GAP on the replay clock.
    const i = tl.real.indexOf(at(2300));
    expect(tl.at[i + 1] - tl.at[i]).toBe(MAX_GAP);
    expect(tl.duration).toBeLessThan(9000);
    for (const real of [at(0), at(700), at(2300), at(5000), at(8500), at(9000)]) expect(realAt(tl, replayAt(tl, real))).toBeCloseTo(real, 6);
    expect(realAt(tl, -5)).toBe(at(0));
    expect(realAt(tl, tl.duration + 100)).toBe(at(9000));
    expect(step(tl, 0, 1)).toBe(replayAt(tl, at(100)));
    expect(step(tl, tl.duration, -1)).toBe(replayAt(tl, at(8500)));
  });

  it("starts with the thread and nothing else", () => {
    const d = frameAt(s, tl, at(0) - 1);
    expect([...d.threads.keys()]).toEqual(["thr_1"]);
    expect(d.runs.size).toBe(0);
    expect(d.machines.get(SHARE_MACHINE)?.value.name).toBe("ada-mbp");
    expect(d.projects.get("prj_1")?.value.path).toBe("~/roll");
  });

  it("streams text, keeps the run busy, and waits on the approval", () => {
    let d = frameAt(s, tl, at(700));
    const th = d.threads.get("thr_1")!.value;
    expect([th.status, th.active_run, th.run_count]).toEqual(["running", "run_1", 1]);
    const m = d.nodes.get("thr_1")!.get("nod_m")!.value;
    expect(m.status).toBe("running");
    expect(m.body).toEqual({ type: "message", text: "01234" });
    expect(d.runs.get("run_1")!.value).toMatchObject({ status: "running", completed_at: null, checkpoint: null, stat: null });
    expect(d.threads.has("thr_2")).toBe(false);

    d = frameAt(s, tl, at(3000));
    expect(d.threads.get("thr_1")!.value).toMatchObject({ status: "waiting", pending_requests: 1 });
    expect(d.requests.get("req_1")!.value).toMatchObject({ status: "pending", response: null, resolved_by: null });
    expect(d.runs.get("run_1")!.value.status).toBe("waiting");
    expect(d.nodes.get("thr_1")!.get("nod_r")!.value.status).toBe("waiting");
    // The subagent appeared when it was started.
    expect(d.threads.get("thr_2")!.value.lineage?.parent).toBe("thr_1");
    expect(d.checkpoints.get("thr_1")?.size ?? 0).toBe(0);

    d = frameAt(s, tl, at(8300));
    expect(d.requests.get("req_1")!.value.status).toBe("resolved");
    const t = d.nodes.get("thr_1")!.get("nod_t")!.value;
    expect(t.body).toMatchObject({ type: "tool", output: "ok ok", exit_code: null });
    expect(d.threads.get("thr_1")!.value).toMatchObject({ status: "running", activity: "cargo test", pending_requests: 0 });
    expect(nodeTree(d, "thr_1", "run_1").map((n) => n.node.id)).toEqual(["nod_m", "nod_r", "nod_t"]);
  });

  it("ends exactly as the share was frozen", () => {
    const d = frameAt(s, tl, at(9000));
    const { kind: _k, ...want } = s.entities[1] as Extract<Entity, { kind: "thread" }>;
    expect(d.threads.get("thr_1")!.value).toEqual(want);
    expect(runsOf(d, "thr_1")[0].status).toBe("completed");
    expect(d.checkpoints.get("thr_1")!.size).toBe(1);
    expect(d.nodes.get("thr_1")!.get("nod_m")!.value.body).toEqual({ type: "message", text: "0123456789" });
  });

  it("shows text whole without streaming (reduced motion), but still steps through states", () => {
    const d = frameAt(s, tl, at(700), { stream: false });
    expect(d.nodes.get("thr_1")!.get("nod_m")!.value.body).toEqual({ type: "message", text: "0123456789" });
    expect(d.threads.get("thr_1")!.value.status).toBe("running");
  });

  it("handles a share with no times", () => {
    const empty: Share = { ...s, entities: [s.entities[0]] };
    const t = timeline(empty);
    expect(t.duration).toBe(0);
    expect(frameAt(empty, t, at(0)).projects.size).toBe(1);
  });
});

describe("a share's diff (P8)", () => {
  type File = { path: string; change: "add" | "modify" | "delete"; diff: string | null };
  const edit = (id: string, ordinal: number, files: File[], over: Parameters<typeof node>[1] = {}) =>
    node({ type: "tool", name: "Edit", kind: "edit", title: "Edit", input: null, output: null, exit_code: 0, files }, { id, ordinal, status: "completed", ...over });
  const data = () =>
    applySnapshot(
      emptyData(),
      {
        cursor: 10,
        machines: [machine()],
        entities: [
          mirrored("project", project(), 1),
          mirrored("thread", thread(), 2),
          mirrored("run", run(), 3),
          mirrored("run", run({ id: "run_2", ordinal: 2 }), 4),
          mirrored("run", run({ id: "run_3", ordinal: 3, status: "rolled_back" }), 5),
          mirrored("node", edit("nod_1", 1, [{ path: "src/queue.rs", change: "modify", diff: "--- a/src/queue.rs\n+++ b/src/queue.rs\n@@ -1,2 +1,2 @@\n-let a = 1;\n+let a = 2;\n" }]), 6),
          mirrored("node", edit("nod_2", 2, [{ path: "src/new.rs", change: "add", diff: "@@ -0,0 +1 @@\n+fn new() {}" }]), 7),
          mirrored("node", edit("nod_3", 1, [{ path: "src/queue.rs", change: "modify", diff: "@@ -5 +5 @@\n-b\n+c" }, { path: "src/new.rs", change: "modify", diff: "@@ -1 +1 @@\n-fn new() {}\n+fn new() -> u8 { 1 }" }], { run: "run_2" }), 8),
          mirrored("node", edit("nod_4", 2, [{ path: "src/broken.rs", change: "modify", diff: "@@ -1 +1 @@\n-x\n+y" }], { run: "run_2", status: "failed" }), 9),
          mirrored("node", edit("nod_5", 1, [{ path: "src/gone.rs", change: "delete", diff: null }], { run: "run_3" }), 10),
        ],
      },
      new Set(["thr_1"]),
    );

  it("is every file the thread's edits touched, each edit's hunks after the one before", () => {
    const d = editsDiff(data(), "thr_1", null);
    expect(d.files.map((f) => [f.path, f.change])).toEqual([
      ["src/queue.rs", "modify"],
      ["src/new.rs", "add"],
    ]);
    // The file headers go; both edits' hunks stay, in order.
    expect(d.files[0].diff).toBe("@@ -1,2 +1,2 @@\n-let a = 1;\n+let a = 2;\n@@ -5 +5 @@\n-b\n+c");
    expect(d.stat).toEqual({ files: 2, additions: 4, deletions: 3 });
    // A failed edit and a rolled-back run aren't changes.
    expect(d.files.some((f) => f.path === "src/broken.rs" || f.path === "src/gone.rs")).toBe(false);
  });

  it("can be one run's", () => {
    expect(editsDiff(data(), "thr_1", "run_2").files.map((f) => [f.path, f.change])).toEqual([
      ["src/queue.rs", "modify"],
      ["src/new.rs", "modify"],
    ]);
    expect(editsDiff(data(), "thr_1", "run_3").files).toEqual([]);
  });
});
