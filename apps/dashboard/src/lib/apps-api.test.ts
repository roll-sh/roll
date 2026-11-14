import { describe, expect, it } from "vitest";
import { deployChange, describePending, formatFirewall, parseEnv, parseFirewall, stepDurations, toEnv, type DeployStatus, type EnvironmentSummary, type InstanceState, type Service } from "./apps-api";
import { service } from "./workspace-resources";

const svc = (env: Partial<EnvironmentSummary>, id = "svc_a"): Service => ({
  id,
  project: "prj_a",
  name: "web",
  source: { kind: "repo", repo: "acme/site" },
  build: { builder: "auto" },
  run: { type: "web", min_instances: 0, max_instances: 3, concurrency: 50, cpu: 0.5, memory_mb: 512, health_timeout_s: 60, suspend_after_s: 30, sleep_after_s: 600 },
  registry: null,
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

describe("pasted .env text", () => {
  it("reads KEY=value lines as a shell would, and names the lines it can't", () => {
    const { vars, errors } = parseEnv('# db\nexport DATABASE_URL=postgres://x?a=b\n\nNAME="a \\"b\\"\\nc"\nRAW=\'$HOME\'\n1BAD=x\nno equals\n');
    expect(vars).toEqual({ DATABASE_URL: "postgres://x?a=b", NAME: 'a "b"\nc', RAW: "$HOME" });
    expect(errors).toEqual([6, 7]);
  });

  it("writes variables as text that reads back exactly (the raw editor)", () => {
    const vars = { URL: "postgres://u:p@h:5432/db?ssl=true", SPACED: " a b ", QUOTE: `it's "x"`, PEM: "-----BEGIN-----\nabc\\n\t\r\n-----END-----", EMPTY: "", HASH: "a #b", DOLLAR: "$HOME" };
    expect(parseEnv(toEnv(vars))).toEqual({ vars, errors: [] });
    expect(toEnv({ A: "plain", B: "two words" })).toBe("A=plain\nB='two words'\n");
    const odd = parseEnv("__proto__=x\ntoString=y\n").vars;
    expect([Object.keys(odd), odd.__proto__, Object.hasOwn(odd, "constructor")]).toEqual([["__proto__", "toString"], "x", false]);
  });
});

describe("a deployment's steps", () => {
  it("time each step until the next, the queue until the first, and nothing after it's live", () => {
    const d = { created_at: 1000, steps: [{ step: "build", at: 1500 }, { step: "rollout", at: 35_500 }, { step: "live", at: 38_000 }] };
    expect(stepDurations(d)).toEqual([
      { label: "Queued", ms: 500 },
      { label: "Build", ms: 34_000 },
      { label: "Rollout", ms: 2500 },
      { label: "Live", ms: null },
    ]);
    expect(stepDurations({ created_at: 0, steps: [{ step: "build", at: 10 }] }, 110).at(-1)).toEqual({ label: "Build", ms: 100 });
  });
});

describe("firewall rules as typed", () => {
  it("read one a line and come back the same", () => {
    const text = "allow /admin/health\ndeny /admin/*\ndeny method=DELETE,PUT\n# a note\ndeny ua=Bad Bot/2";
    const rules = parseFirewall(text);
    expect(rules).toEqual([
      { action: "allow", path: "/admin/health" },
      { action: "deny", path: "/admin/*" },
      { action: "deny", methods: ["DELETE", "PUT"] },
      { action: "deny", user_agent: "Bad Bot/2" },
    ]);
    expect(formatFirewall(rules)).toBe(text.replace("# a note\n", ""));
  });
});

describe("pending changes", () => {
  it("name what the next deploy does", () => {
    expect(describePending({ variables: { added: ["ADDED"], removed: ["GONE"], changed: ["MODE"] }, run: ["memory_mb"] })).toBe("MODE changed, ADDED added, GONE removed, memory mb set");
    expect(describePending({ variables: null, run: [] })).toBeNull();
  });
});

