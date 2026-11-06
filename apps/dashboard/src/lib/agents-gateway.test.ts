import { afterEach, describe, expect, it, vi } from "vitest";
import type { Command, GatewaySettings, LogEntry, UsageRow } from "./agents-api";
import { AgentsClient } from "./agents-client";
import { DemoOwner } from "./agents-demo";
import { checkSettings, createGatewayApi, gatewayOn, isIPv6, joinProviders, listenError, LOOPBACK, mergeLog, OTHER, reachProblem, stackUsage, urlChoices, urlFor } from "./agents-gateway";
import { ApiError } from "./code-api";

const PUB = "KMIZzg7UJuV-Nxs3gUarEXCHzKlAkKSSSCaXrdLz4zg";

describe("settings, checked as the daemon checks them", () => {
  const base: GatewaySettings = { enabled: true, port: 8790, upstreams: [{ id: "anthropic", name: " Anthropic ", kind: "anthropic" }] };

  it("trims and keeps only what differs from the defaults", () => {
    const r = checkSettings({ ...base, affinity: true, retention_days: 400, log_bodies: false });
    expect(r).toEqual({ settings: { enabled: true, port: 8790, upstreams: [{ id: "anthropic", name: "Anthropic", kind: "anthropic" }] } });
  });

  it("refuses what the daemon refuses, in its words", () => {
    const err = (s: Partial<GatewaySettings>) => {
      const r = checkSettings({ ...base, ...s });
      return "error" in r ? r.error : "";
    };
    const unspecified = [0, 0, 0, 0].join(".");
    expect(err({ port: 80 })).toMatch(/1024 to 65535/);
    expect(err({ listen: [unspecified] })).toMatch(/every network/);
    expect(err({ listen: ["::"] })).toMatch(/every network/);
    expect(err({ listen: ["gw.local"] })).toMatch(/isn't an IP address/);
    expect(err({ listen: [[100, 64, 0, 10].join(".")] })).toBe("");
    expect(err({ upstreams: [{ id: "c", name: "C", kind: "openai_compatible" }] })).toMatch(/needs a base URL/);
    expect(err({ upstreams: [{ id: "b", name: "B", kind: "bedrock", region: "us-east-1" }] })).toMatch(/name the models/);
    expect(err({ upstreams: [{ id: "v", name: "V", kind: "vertex", region: "us-east5" }] })).toMatch(/project and a region/);
    expect(err({ upstreams: [{ id: "a", name: "A", kind: "anthropic", base_url: "https://k:sk@x" }] })).toMatch(/key field/);
    expect(err({ upstreams: [{ id: "a", name: "A", kind: "anthropic", headers: [["Authorization", "Bearer x"]] }] })).toMatch(/set by the gateway/);
    expect(err({ upstreams: [{ id: "a", name: "A", kind: "anthropic", models: ["a*b"] }] })).toMatch(/one \*/);
    expect(err({ upstreams: [{ id: "a", name: "A", kind: "anthropic", responses: true }] })).toMatch(/Responses/);
    expect(err({ routes: [{ model: "fast", targets: [{ upstream: "nope" }] }] })).toMatch(/isn't an upstream/);
    expect(err({ routes: [{ model: "fast", targets: [] }] })).toMatch(/1 to 16 targets/);
    expect(err({ retention_days: 0 })).toMatch(/1 to 3650/);
  });

  it("knows IP literals", () => {
    expect(isIPv6("fd7a:115c:a1e0::1")).toBe(true);
    expect(isIPv6("::1")).toBe(true);
    expect(isIPv6("1:2:3:4:5:6:7:8:9")).toBe(false);
    expect(listenError(LOOPBACK)).toBeNull();
    expect(listenError([224, 0, 0, 1].join("."))).toMatch(/multicast/);
    expect(urlFor("fd7a::1", 8790)).toBe("http://[fd7a::1]:8790");
  });

});

describe("reaching the gateway", () => {
  const plan = { host: "mch_h", urls: [{ label: "Tailscale", url: "http://h.ts.net:8790" }] };
  const m = (id: string, sandbox?: string) => ({ id, name: id, online: true, info: null, sandbox: sandbox ? { backend: sandbox, size: "s", status: "running" as const, hourly_cents: 0 } : null, appearance: {}, created_at: 0, last_seen: 0 });

  it("offers loopback on the host itself, and the owner's addresses", () => {
    expect(urlChoices(plan, 8790, "mch_h").map((u) => u.label)).toEqual(["This computer", "Tailscale"]);
    expect(urlChoices(plan, 8790, "mch_h")[0].url).toBe(`http://${LOOPBACK}:8790`);
    expect(urlChoices(plan, 8790, "mch_x").map((u) => u.label)).toEqual(["Tailscale"]);
  });

  it("says when a machine can't reach it", () => {
    expect(reachProblem(m("mch_h"), { host: "mch_h" })).toBeNull();
    expect(reachProblem(m("mch_x"), { host: "mch_h" })).toMatch(/Add an address/);
    expect(reachProblem(m("mch_x", "agent37"), plan)).toMatch(/hosted sandbox/);
    expect(reachProblem(m("mch_x", "docker"), plan)).toBeNull();
  });

  it("joins without the providers another endpoint serves, unless they're moved", () => {
    const providers = [{ id: "claude", name: "Claude Code" }, { id: "codex", name: "Codex" }];
    const proxy = { id: "cpa", name: "CLIProxy", kind: "openai", base_url: "http://cpa:8317", providers: ["claude"], key: "sealed" };
    const gw = { ...proxy, id: "roll-gateway", providers: ["codex"] };
    const with_ = (endpoints: unknown[]) => ({ ...m("mch_x"), info: { providers, settings: { endpoints } } }) as unknown as Parameters<typeof joinProviders>[0];
    // Nothing elsewhere (the gateway's own endpoint doesn't count): every provider, as before.
    expect(joinProviders(with_([gw]), new Set())).toBeUndefined();
    // Claude Code stays on the proxy; moved, it joins too; nothing left, nothing joins.
    expect(joinProviders(with_([proxy]), new Set())).toEqual(["codex"]);
    expect(joinProviders(with_([proxy]), new Set(["claude"]))).toBeUndefined();
    expect(joinProviders(with_([proxy, { ...proxy, id: "other", providers: ["codex"] }]), new Set())).toEqual([]);
  });
});

describe("the request log", () => {
  const e = (seq: number, over: Partial<LogEntry> = {}): LogEntry => ({
    seq,
    at: seq,
    format: "messages",
    model: "m",
    stream: true,
    status: 200,
    ms: 1,
    attempts: [],
    input_tokens: 0,
    output_tokens: 0,
    cache_read_tokens: 0,
    cache_write_tokens: 0,
    cost_micros: 0,
    ...over,
  });

  it("merges a tail newest first, each entry once, capped", () => {
    const shown = [e(3), e(2), e(1)];
    expect(mergeLog(shown, [e(5), e(4), e(3, { status: 500 })]).map((x) => [x.seq, x.status])).toEqual([
      [5, 200],
      [4, 200],
      [3, 500],
      [2, 200],
      [1, 200],
    ]);
    expect(mergeLog(shown, [e(4)], 2).map((x) => x.seq)).toEqual([4, 3]);
  });
});

describe("usage", () => {
  const DAY = 86_400_000;
  const row = (day: number, key: string, cost: number, requests = 1): UsageRow => ({ at: day * DAY, key, requests, errors: 0, input_tokens: 10, output_tokens: 5, cache_read_tokens: 0, cache_write_tokens: 0, cost_micros: cost });

  it("stacks days, every day in the range, the biggest groups first and the rest as one", () => {
    const rows = [row(0, "a", 5), row(0, "b", 3), row(2, "a", 1), row(2, "c", 2), row(1, "d", 1), row(2, "e", 1)];
    const s = stackUsage({ rows, from: 0, to: 3 * DAY, bucket: "day" }, "cost", 2);
    expect(s.at).toEqual([0, DAY, 2 * DAY]);
    expect(s.series.map((x) => x.key)).toEqual(["a", "b", OTHER]);
    expect(s.series[0].values).toEqual([5, 0, 1]);
    expect(s.series[2].values).toEqual([0, 1, 3]);
    expect(s.totals).toEqual([8, 1, 4]);
    expect(s.max).toBe(8);
    expect(stackUsage({ rows, from: 0, to: 3 * DAY, bucket: "day" }, "requests", 9).series).toHaveLength(5);
  });

});

describe("the relay client", () => {
  it("calls the plan routes, and reads a missing plan as none", async () => {
    const calls: [string, string, unknown][] = [];
    const api = createGatewayApi(async (method, path, body) => {
      calls.push([method, path, body]);
      if (method === "GET") throw new ApiError("no plan", 404, null);
      if (method === "PUT") return body;
      return null;
    });
    expect(await api.get("acme")).toBeNull();
    const plan = { host: "mch_1", acknowledged_at: 1, updated_at: 0 };
    expect(await api.save("acme", plan)).toEqual(plan);
    await api.join("acme", { machine: "mch_2", url: "http://h:8790" });
    await api.leave("acme", "mch_2");
    await api.remove("acme");
    expect(calls.map(([m, p]) => `${m} ${p}`)).toEqual([
      "GET /v1/code/agents/acme/gateway",
      "PUT /v1/code/agents/acme/gateway",
      "POST /v1/code/agents/acme/gateway/members",
      "DELETE /v1/code/agents/acme/gateway/members/mch_2",
      "DELETE /v1/code/agents/acme/gateway",
    ]);
  });
});

describe("the host's commands", () => {
  it("seal an upstream's key to the host: the plaintext never leaves the browser", async () => {
    const sent: Command[] = [];
    const client = { run: vi.fn(async (_m: string, c: Command) => (sent.push(c), { type: "ok" })) } as unknown as AgentsClient;
    const host = { id: "mch_h", name: "h", online: true, info: { seal_key: PUB } } as never;
    const gw = gatewayOn(client, host);
    await gw.setKey("anthropic", "sk-ant-secret-123");
    await gw.setKey("openai", null);
    await expect(gatewayOn(client, { id: "mch_old", info: {} } as never).setKey("x", "k")).rejects.toThrow(/too old/);
    expect(JSON.stringify(sent)).not.toContain("sk-ant-secret");
    expect(sent[0]).toMatchObject({ type: "gateway.key", upstream: "anthropic", key: { type: "set", sealed: { v: 1 } } });
    expect(sent[1]).toEqual({ type: "gateway.key", upstream: "openai", key: { type: "clear" } });
    await gw.log({ after: 4, errors: true, limit: 999 });
    expect(sent[2]).toEqual({ type: "gateway.log", after: 4, limit: 200, errors: true });
  });
});

describe("the gateway in the demo", () => {
  let demo: DemoOwner;
  afterEach(() => demo?.dispose());

  it("runs on ai-server for three machines, answers every query, and joins and leaves like the relay", async () => {
    demo = new DemoOwner(0.01, false);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    const services = demo.services();
    const plan = await services.gateway!.get("demo");
    expect(plan?.host).toBe("mch_demo_srv");
    expect(plan?.members?.map((m) => m.machine)).toEqual(["mch_demo_srv", "mch_demo_ada", "mch_demo_dock"]);
    await vi.waitFor(() => expect(client.data.machines.size).toBe(4));
    const machine = (id: string) => client.data.machines.get(id)!.value;
    const host = machine("mch_demo_srv");
    expect(host.info?.settings?.gateway?.run.state).toBe("running");
    expect(host.info?.settings?.gateway?.settings.upstreams).toHaveLength(4);
    // Members carry the managed endpoint; their providers say where they go.
    const ada = machine("mch_demo_ada").info!;
    expect(ada.settings?.endpoints?.find((e) => e.id === "roll-gateway")).toMatchObject({ kind: "gateway", name: "roll gateway on ai-server", fallback: true, providers: ["claude", "codex"] });
    expect(ada.providers.find((p) => p.id === "claude")?.detail).toBe("via roll gateway on ai-server");
    expect(ada.providers.find((p) => p.id === "acp:gemini")?.detail).toBe("via CLIProxyAPI");

    const gw = gatewayOn(client, host);
    const status = await gw.status();
    expect(status.upstreams.find((u) => u.upstream === "gpu-box")?.circuit).toBe("open");
    const week = await gw.usage(Date.now() - 7 * 86_400_000, Date.now(), "model", "day");
    expect(week.rows.length).toBeGreaterThan(10);
    expect(new Set(week.rows.map((r) => r.key))).toContain("fast");
    const log = await gw.log({ limit: 200 });
    expect(log.entries.length).toBeGreaterThan(0);
    const failovers = await gw.log({ errors: true, limit: 200 });
    expect(failovers.entries.every((e) => e.status >= 400 || e.error || e.attempts.length > 1)).toBe(true);
    expect(failovers.entries.some((e) => e.attempts.length > 1 && e.status === 200)).toBe(true);
    const tested = await gw.test();
    expect(tested.checks.map((c) => [c.upstream, c.ok])).toEqual([
      ["anthropic", true],
      ["bedrock", true],
      ["openai", true],
      ["gpu-box", false],
    ]);

    // build-box joins: the host mints a key sealed to it, the member's endpoint gets the box.
    await services.gateway!.join("demo", { machine: "mch_demo_box", url: plan!.urls![0].url, providers: ["codex"] });
    const joined = (await services.gateway!.get("demo"))!.members!.find((m) => m.machine === "mch_demo_box")!;
    expect(joined.client).toMatch(/^gwc_/);
    expect(joined.error).toBeUndefined();
    await vi.waitFor(() => expect(machine("mch_demo_box").info?.settings?.endpoints?.find((e) => e.id === "roll-gateway")?.key).toBe("sealed"));
    expect(machine("mch_demo_srv").info?.settings?.gateway?.clients.some((c) => c.id === joined.client)).toBe(true);
    // A key never shows anywhere the dashboard can read.
    expect(JSON.stringify([...client.data.machines.values()])).not.toContain("rgk_");

    await services.gateway!.leave("demo", "mch_demo_box");
    await vi.waitFor(() => expect(machine("mch_demo_box").info?.settings?.endpoints?.some((e) => e.id === "roll-gateway")).toBe(false));
    expect(machine("mch_demo_srv").info?.settings?.gateway?.clients.some((c) => c.id === joined.client)).toBe(false);
    await expect(services.gateway!.save("demo", { ...plan!, host: "mch_demo_ada" })).rejects.toThrow(/remove the machines first/);
    release();
  });
});
