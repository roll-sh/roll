import { describe, expect, it } from "vitest";
import { deployChange, type DeployStatus, type EnvironmentSummary, type InstanceState, type Service } from "./apps-api";
import { service } from "./workspace-resources";

const svc = (env: Partial<EnvironmentSummary>, id = "svc_a"): Service => ({
  id,
  project: "prj_a",
  name: "web",
  source: { kind: "repo", repo: "acme/site" },
  build: { builder: "auto" },
  run: { type: "web", min_instances: 0, max_instances: 3, concurrency: 50, cpu: 0.5, memory_mb: 512, health_timeout_s: 60, suspend_after_s: 30, sleep_after_s: 600 },
  created_at: 1,
  environments: [{ name: "production", state: "running", url: "https://web-shop.up.roll.site", active: null, latest: null, ...env }],
});

const latest = (status: DeployStatus, id = "dep_1") => ({ latest: { id, status } });

describe("a service on the canvas", () => {
  it("maps instance states: running is online, waking and deploys are busy, asleep is idle and free", () => {
    const view = (state: InstanceState, env: Partial<EnvironmentSummary> = {}) => service(svc({ state, ...env }));
    expect(view("running")).toMatchObject({ state: "online", stateLabel: "Running", sub: "web-shop.up.roll.site" });
    expect(view("waking")).toMatchObject({ state: "starting", stateLabel: "Waking" });
    expect(view("stopped")).toMatchObject({ state: "idle", stateLabel: "Asleep", note: "$0.00/h" });
    expect(view("suspended")).toMatchObject({ state: "idle", stateLabel: "Suspended", note: "$0.00/h" });
    expect(view("none")).toMatchObject({ state: "idle", stateLabel: "Not deployed", note: undefined });
    expect(view("crashed")).toMatchObject({ state: "failed", stateLabel: "Crashed" });
    // A deploy on its way is busy whatever the instances do; once it ends the card says how.
    expect(view("stopped", latest("building"))).toMatchObject({ state: "starting", stateLabel: "Building…", note: undefined });
    expect(view("running", latest("failed"))).toMatchObject({ state: "online", meta: "Failed" });
    expect(view("running").href).toBe("/dashboard/apps/svc_a");
  });
});

describe("deployChange", () => {
  it("is pending while a deploy is on its way, then says how it ended", () => {
    const first = deployChange(new Map(), [svc(latest("queued"))]);
    expect(first).toMatchObject({ pending: true, finished: null });
    const building = deployChange(first.seen, [svc(latest("building"))]);
    expect(building).toMatchObject({ pending: true, finished: null });
    expect(deployChange(building.seen, [svc(latest("active"))])).toMatchObject({ pending: false, finished: "success" });
    expect(deployChange(building.seen, [svc(latest("failed"))])).toMatchObject({ pending: false, finished: "error" });
  });

  it("catches a deploy that came and went between two looks, and ignores old ones seen first", () => {
    expect(deployChange(new Map(), [svc(latest("active"))])).toMatchObject({ pending: false, finished: null });
    const before = deployChange(new Map(), [svc(latest("active", "dep_1"))]).seen;
    expect(deployChange(before, [svc(latest("active", "dep_2"))]).finished).toBe("success");
    // One of two services failing makes it an error.
    const two = deployChange(new Map(), [svc(latest("deploying")), svc(latest("deploying", "dep_9"), "svc_b")]).seen;
    expect(deployChange(two, [svc(latest("active")), svc(latest("failed", "dep_9"), "svc_b")]).finished).toBe("error");
  });
});
