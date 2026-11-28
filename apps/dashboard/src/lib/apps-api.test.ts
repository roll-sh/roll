import { describe, expect, it, vi } from "vitest";
import { deployChange, followLog, formatFirewall, formatHeaders, parseEnv, parseFirewall, parseHeaders, toEnv, type DeployStatus, type EnvironmentSummary, type InstanceState, type Service } from "./apps-api";
import { service } from "./workspace-resources";

const svc = (env: Partial<EnvironmentSummary>, id = "svc_a"): Service => ({
  id,
  project: "prj_a",
  name: "web",
  source: { kind: "repo", repo: "acme/site" },
  build: { builder: "auto" },
  run: { type: "web", min_instances: 0, max_instances: 3, concurrency: 50, cpu: 0.5, memory_mb: 512, health_timeout_s: 60, suspend_after_s: 30, sleep_after_s: 600 },
  registry: null,
  git_login: null,
  github: null,
  created_at: 1,
  environments: [{ name: "production", state: "running", url: "https://web-shop.up.roll.site", active: null, latest: null, needs_deploy: false, ephemeral: false, ...env }],
});

const latest = (status: DeployStatus, id = "dep_1") => ({ latest: { id, status } });

describe("a service on the canvas", () => {
  it("maps instance states: running is online, waking and deploys are busy, asleep is idle and free", () => {
    const view = (state: InstanceState, env: Partial<EnvironmentSummary> = {}) => service(svc({ state, ...env }));
    expect(view("running")).toMatchObject({ state: "online", sub: "web-shop.up.roll.site" });
    expect(view("waking")).toMatchObject({ state: "starting" });
    expect(view("stopped")).toMatchObject({ state: "idle", note: "$0.00/h" });
    expect(view("suspended")).toMatchObject({ state: "idle", note: "$0.00/h" });
    expect(view("none")).toMatchObject({ state: "idle", note: undefined });
    expect(view("crashed")).toMatchObject({ state: "failed" });
    // A deploy on its way is busy whatever the instances do.
    expect(view("stopped", latest("building"))).toMatchObject({ state: "starting", note: undefined });
    expect(view("running", { ...latest("failed"), active: { id: "dep_live", revision: null, created_at: 1 } })).toMatchObject({ state: "online" });
    // What wants an eye: crashing reads failed between restarts, a capped project idle.
    expect(view("running", { attention: ["crash_loop"] })).toMatchObject({ state: "failed" });
    expect(view("stopped", { attention: ["capped"] })).toMatchObject({ state: "idle" });
    // A first deploy that failed: nothing serves.
    expect(view("none", latest("failed"))).toMatchObject({ state: "failed" });
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
    // A deploy that came and went between two looks is caught; an old one seen first isn't.
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
    // Written back as text (the raw editor), it reads back exactly; odd names stay plain keys.
    const all = { URL: "postgres://u:p@h:5432/db?ssl=true", SPACED: " a b ", QUOTE: `it's "x"`, PEM: "-----BEGIN-----\nabc\\n\t\r\n-----END-----", EMPTY: "", HASH: "a #b", DOLLAR: "$HOME" };
    expect(parseEnv(toEnv(all))).toEqual({ vars: all, errors: [] });
    const odd = parseEnv("__proto__=x\ntoString=y\n").vars;
    expect([Object.keys(odd), odd.__proto__, Object.hasOwn(odd, "constructor")]).toEqual([["__proto__", "toString"], "x", false]);
  });

});

describe("firewall rules and response headers as typed", () => {
  it("read one a line and come back the same", () => {
    const text = "allow /admin/health\ndeny /admin/*\ndeny method=DELETE,PUT\n# a note\ndeny ip=203.0.113.0/24,2001:db8::/32\ndeny /api/* header=X-Bot query=debug:1\nlimit 100:20 /api/* key=header:x-api-key\nchallenge /* bots=tools,spoofed\nlog /wp-* attacks=scanners,sqli\ndeny ua=Bad Bot/2";
    const rules = parseFirewall(text);
    expect(rules).toEqual([
      { action: "allow", path: "/admin/health" },
      { action: "deny", path: "/admin/*" },
      { action: "deny", methods: ["DELETE", "PUT"] },
      { action: "deny", ips: ["203.0.113.0/24", "2001:db8::/32"] },
      { action: "deny", path: "/api/*", query: { name: "debug", contains: "1" }, header: { name: "X-Bot" } },
      { action: "limit", per_minute: 100, burst: 20, path: "/api/*", key: "header:x-api-key" },
      { action: "challenge", path: "/*", bots: ["tools", "spoofed"] },
      { action: "log", path: "/wp-*", attacks: ["scanners", "sqli"] },
      { action: "deny", user_agent: "Bad Bot/2" },
    ]);
    expect(formatFirewall(rules)).toBe(text.replace("# a note\n", ""));
    // Response headers: one a line, grouped into rules by path and replace.
    const headers = "Strict-Transport-Security: max-age=63072000\n/api/* Cache-Control: no-store\n!/api/* Content-Type: application/problem+json\n# a note\nnot a header";
    const headerRules = parseHeaders(headers);
    expect(headerRules).toEqual([
      { set: { "Strict-Transport-Security": "max-age=63072000" } },
      { path: "/api/*", set: { "Cache-Control": "no-store" } },
      { path: "/api/*", set: { "Content-Type": "application/problem+json" }, replace: true },
    ]);
    expect(parseHeaders(formatHeaders(headerRules))).toEqual(headerRules);
  });
});

describe("following a log", () => {
  it("stops at a finished log's end instead of reading it all again", async () => {
    vi.useFakeTimers();
    const enc = new TextEncoder();
    const fetchFn = vi.fn(async () => new Response(new ReadableStream({ start(c) { c.enqueue(enc.encode('data: {"t":1,"stream":"stdout","text":"done"}\n\nevent: end\ndata: end\n\n')); c.close(); } })));
    vi.stubGlobal("fetch", fetchFn);
    const lines: string[] = [];
    let ended = false;
    followLog({ job: { service: "svc_1", env: "production", job: "job_1" } }, { onLine: (l) => lines.push(l.text), onReset: () => {}, onEnd: () => (ended = true) });
    await vi.waitFor(() => expect(ended).toBe(true));
    // Past every reconnect delay: nothing is read again.
    await vi.advanceTimersByTimeAsync(20_000);
    expect(lines).toEqual(["done"]);
    expect(fetchFn).toHaveBeenCalledTimes(1);
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });
});

