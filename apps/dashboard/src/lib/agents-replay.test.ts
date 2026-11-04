import { describe, expect, it } from "vitest";
import type { Entity, Share } from "./agents-api";
import { frameAt, MAX_GAP, providerLabel, realAt, replayAt, SHARE_MACHINE, shareStats, step, timeline } from "./agents-replay";
import { nodeTree, runsOf } from "./agents-store";
import { share, T0 } from "@/test/agents";

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

  it("names the providers the threads ran on", () => {
    const providers = frameAt(s, tl, at(9000)).machines.get(SHARE_MACHINE)!.value.info!.providers;
    expect(providers.map((p) => [p.id, p.name, p.models.map((m) => m.id)])).toEqual([["claude", "Claude Code", ["claude-opus-5-5"]]]);
    expect([providerLabel("codex"), providerLabel("acp:gemini")]).toEqual(["Codex", "Gemini"]);
  });

  it("counts what happened", () => {
    expect(shareStats(s)).toEqual({ runs: 1, steps: 3, threads: 2, files: 2, providers: ["claude"] });
  });

  it("handles a share with no times", () => {
    const empty: Share = { ...s, entities: [s.entities[0]] };
    const t = timeline(empty);
    expect(t.duration).toBe(0);
    expect(frameAt(empty, t, at(0)).projects.size).toBe(1);
  });
});
