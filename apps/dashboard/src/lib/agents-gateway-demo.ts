/**
 * The demo's model gateway (docs/code/gateway.md), in memory: a gateway on `ai-server` with four
 * upstreams (one with an open circuit), routes and an alias, three machines using it, a week of
 * usage, and a request log that keeps ticking, failovers included. It answers the host's
 * `gateway.*` commands the way the daemon does and the plan routes the way the relay does
 * (joining asks the host for a client key sealed to the member, then sets the member's
 * `roll-gateway` endpoint with that box), so every part of the Gateway UI has something real to show.
 */

import {
  GATEWAY_LIMITS,
  GATEWAY_PORT,
  type Attempt,
  type Command,
  type EndpointInfo,
  type GatewayClient,
  type GatewayInfo,
  type GatewayPlan,
  type GatewaySettings,
  type LogEntry,
  type Machine,
  type MachineId,
  type Reply,
  type UpstreamHealth,
  type UsageRow,
  type WireFormat,
} from "./agents-api";
import { checkSettings, loopbackUrl, memberEndpointName, type GatewayApi } from "./agents-gateway";
import { sealKey } from "./agents-seal";
import { ApiError } from "./code-api";

export interface DemoGatewayHost {
  machines(): Map<MachineId, Machine>;
  /** Show the gateway in the host's `MachineConfig.gateway` (its `hello`). */
  setInfo(machine: MachineId, info: GatewayInfo | undefined, emit?: boolean): void;
  /** Lay (or remove) a member's `roll-gateway` endpoint, as `endpoint.set`/`endpoint.remove` do. */
  command(machine: MachineId, c: Command): Promise<Reply>;
  /** Projects and threads to attribute requests to. */
  work(): { machine: MachineId; project: string; thread: string }[];
  after(ms: number, fn: () => void): void;
}

/** A request the demo makes up: who asked, for what, through which upstreams. */
interface Shape {
  model: string;
  format: WireFormat;
  attempts: Omit<Attempt, "ms">[];
  /** Tokens in, out, cache read. */
  size: [number, number, number];
}

/** Micro-dollars per million tokens, roughly list prices (in, out, cache read): the demo's estimate. */
const PRICE: Record<string, [number, number, number]> = {
  "claude-opus-5-5": [5_000_000, 25_000_000, 500_000],
  "claude-sonnet-4-6": [3_000_000, 15_000_000, 300_000],
  "claude-haiku-4-5": [1_000_000, 5_000_000, 100_000],
  "gpt-5.5-codex": [1_250_000, 10_000_000, 125_000],
  "gpt-5.5": [1_250_000, 10_000_000, 125_000],
};

const cost = (upstreamModel: string, [i, o, c]: [number, number, number]) => {
  const key = Object.keys(PRICE).find((k) => upstreamModel.includes(k));
  if (!key) return 0;
  const [pi, po, pc] = PRICE[key];
  return Math.round((i * pi + o * po + c * pc) / 1e6);
};

const now = () => Date.now();
const HOUR = 3_600_000;

/** A fixed-seed random, so the demo's week looks the same on every load. */
function rng(seed: number) {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 2 ** 32;
  };
}

export class DemoGateway {
  private plan: GatewayPlan | null = null;
  private host: MachineId | null = null;
  private settings: GatewaySettings | null = null;
  private keys = new Map<string, "sealed" | "reference" | "none">();
  private clients: GatewayClient[] = [];
  private health = new Map<string, UpstreamHealth>();
  private seen = new Map<string, number>();
  private log: LogEntry[] = [];
  private usage: (UsageRow & { model: string; machine: string; project: string; thread: string; upstream: string })[] = [];
  private seq = 0;
  private since = 0;
  private ticking = false;
  private random = rng(7);

  constructor(private h: DemoGatewayHost) {}

  // ── the seed ──

  /** The demo's gateway on `host`, used by `members` (the providers each lets use it; unset: all). */
  seed(host: MachineId, members: { machine: MachineId; providers?: string[] }[]) {
    const t = now();
    const tailnet = [100, 88, 12, 7].join(".");
    const bridge = [172, 18, 0, 1].join(".");
    this.host = host;
    this.settings = {
      enabled: true,
      port: GATEWAY_PORT,
      listen: [tailnet, bridge],
      upstreams: [
        { id: "anthropic", name: "Anthropic", kind: "anthropic" },
        { id: "bedrock", name: "Amazon Bedrock", kind: "bedrock", region: "us-east-1", models: ["anthropic.claude-*", "us.anthropic.claude-*"], key_ref: "op://Dev/Bedrock/credential" },
        { id: "openai", name: "OpenAI", kind: "openai" },
        { id: "gpu-box", name: "GPU box (Ollama)", kind: "openai_compatible", base_url: `http://${[100, 88, 12, 40].join(".")}:11434/v1`, models: ["qwen3-coder*"], responses: true, allow_private: true },
      ],
      routes: [
        { model: "claude-opus-5-5", targets: [{ upstream: "anthropic" }, { upstream: "bedrock", model: "us.anthropic.claude-opus-5-5" }] },
        { model: "claude-sonnet-4-6", targets: [{ upstream: "anthropic" }, { upstream: "bedrock", model: "us.anthropic.claude-sonnet-4-6" }] },
        { model: "fast", targets: [{ upstream: "anthropic", model: "claude-haiku-4-5" }, { upstream: "bedrock", model: "us.anthropic.claude-haiku-4-5" }] },
      ],
      prices: [{ model: "qwen3-coder*", input: 0, output: 0, cache_read: 0, cache_write: 0 }],
      log_bodies: true,
    };
    this.keys = new Map([
      ["anthropic", "sealed"],
      ["bedrock", "reference"],
      ["openai", "sealed"],
      ["gpu-box", "none"],
    ]);
    this.since = t - 26 * HOUR;
    this.plan = {
      host,
      urls: [
        { label: "Tailscale", url: "http://ai-server.tail4c1d.ts.net:8790" },
        { label: "Docker sandboxes", url: `http://${bridge}:8790` },
      ],
      members: [],
      new_sandboxes: true,
      fallback: true,
      acknowledged_at: t - 26 * HOUR,
      updated_at: t - 26 * HOUR,
    };
    for (const [i, { machine: m, providers }] of members.entries()) {
      const c: GatewayClient = { id: `gwc_demo${(i + 1).toString(16).padStart(11, "0")}`, name: this.h.machines().get(m)?.name ?? m, machine: m, created_at: t - 26 * HOUR + i * 60_000 };
      this.clients.push(c);
      this.plan.members!.push({ machine: m, url: m === host ? loopbackUrl(GATEWAY_PORT) : this.plan.urls![0].url, ...(providers ? { providers } : {}), client: c.id, joined_at: c.created_at });
    }
    for (const u of this.settings.upstreams ?? []) this.health.set(u.id, { upstream: u.id, circuit: "closed", failures: 0, requests_hour: 0, last_ok_at: t - 20_000 });
    // The GPU box is off: five refused connections in a row opened its circuit.
    this.health.set("gpu-box", { upstream: "gpu-box", circuit: "open", failures: 5, open_until: t + 4 * 60_000, last_error: "connect: connection refused", last_error_at: t - 70_000, requests_hour: 3 });
    this.history(t);
    this.publish(false);
  }

  /** The members' endpoint, as the relay sets it when they join. */
  endpointFor(machine: MachineId): EndpointInfo | null {
    const member = this.plan?.members?.find((m) => m.machine === machine);
    const host = this.host && this.h.machines().get(this.host);
    if (!member || !host || !this.plan) return null;
    const providers = member.providers?.length ? member.providers : (this.h.machines().get(machine)?.info?.settings?.providers ?? []).map((p) => p.id);
    return {
      id: GATEWAY_LIMITS.endpointId,
      name: memberEndpointName(host.name),
      kind: "gateway",
      base_url: member.url,
      ...(providers.length ? { providers } : {}),
      ...(this.plan.fallback ? { fallback: true } : {}),
      key: "sealed",
      models: [...(this.settings?.routes ?? []).map((r) => ({ id: r.model })), { id: "gpt-5.5-codex" }, { id: "gpt-5.5" }, { id: "qwen3-coder" }],
      checked_at: now(),
    };
  }

  /** What the gateway's machine reports. */
  private info(): GatewayInfo | undefined {
    if (!this.settings) return undefined;
    const ups = this.settings.upstreams ?? [];
    const addresses = this.settings.enabled ? [`${[127, 0, 0, 1].join(".")}:${this.settings.port}`, `[::1]:${this.settings.port}`, ...(this.settings.listen ?? []).map((a) => `${a}:${this.settings!.port}`)] : [];
    return {
      settings: this.settings,
      keys: ups.map((u) => ({ upstream: u.id, key: u.key_ref ? "reference" : (this.keys.get(u.id) ?? "none") })),
      clients: [...this.clients],
      run: this.settings.enabled ? { state: "running", addresses, since: this.since } : { state: "stopped" },
    };
  }

  private publish(emit = true) {
    if (this.host) this.h.setInfo(this.host, this.info(), emit);
  }

  // ── made-up traffic ──

  private shapes(): Shape[] {
    const r = this.random;
    const big = (): [number, number, number] => [Math.round(4_000 + r() * 30_000), Math.round(300 + r() * 2_500), Math.round(20_000 + r() * 90_000)];
    const small = (): [number, number, number] => [Math.round(1_000 + r() * 6_000), Math.round(100 + r() * 600), Math.round(r() * 8_000)];
    return [
      { model: "claude-opus-5-5", format: "messages", attempts: [{ upstream: "anthropic", model: "claude-opus-5-5", status: 200 }], size: big() },
      { model: "claude-opus-5-5", format: "messages", attempts: [{ upstream: "anthropic", model: "claude-opus-5-5", status: 200 }], size: big() },
      { model: "claude-sonnet-4-6", format: "messages", attempts: [{ upstream: "anthropic", model: "claude-sonnet-4-6", status: 200 }], size: big() },
      { model: "fast", format: "messages", attempts: [{ upstream: "anthropic", model: "claude-haiku-4-5", status: 200 }], size: small() },
      { model: "gpt-5.5-codex", format: "responses", attempts: [{ upstream: "openai", model: "gpt-5.5-codex", status: 200 }], size: big() },
      { model: "gpt-5.5", format: "chat", attempts: [{ upstream: "openai", model: "gpt-5.5", status: 200 }], size: small() },
      // Failover: Anthropic is overloaded, Bedrock takes over before the first byte.
      {
        model: "claude-sonnet-4-6",
        format: "messages",
        attempts: [
          { upstream: "anthropic", model: "claude-sonnet-4-6", status: 529, error: "overloaded" },
          { upstream: "bedrock", model: "us.anthropic.claude-sonnet-4-6", status: 200 },
        ],
        size: big(),
      },
    ];
  }

  /** One request through the gateway at `at`. */
  private request(at: number, live: boolean): LogEntry {
    const r = this.random;
    const work = this.h.work();
    const w = work[Math.floor(r() * work.length)] ?? { machine: this.host ?? "", project: "", thread: "" };
    const shapes = this.shapes();
    let shape = shapes[Math.floor(r() * shapes.length)];
    // Now and then someone asks for the GPU box's model while it's off: nothing else serves it.
    if (live && r() < 0.08) shape = { model: "qwen3-coder", format: "chat", attempts: [], size: [0, 0, 0] };
    const attempts: Attempt[] = shape.attempts.map((a) => ({ ...a, ms: a.status === 200 ? Math.round(400 + r() * 900) : Math.round(60 + r() * 200) }));
    const last = attempts[attempts.length - 1];
    const ok = last?.status === 200;
    const [input, output, cached] = shape.size;
    const client = this.clients.find((c) => c.machine === w.machine);
    const e: LogEntry = {
      seq: ++this.seq,
      at,
      ...(client ? { client: client.id, machine: w.machine } : {}),
      ...(w.project ? { project: w.project } : {}),
      ...(w.thread ? { thread: w.thread } : {}),
      format: shape.format,
      model: shape.model,
      stream: shape.format !== "chat" || r() < 0.5,
      status: last ? last.status : 503,
      ms: attempts.reduce((a, x) => a + x.ms, 0) + (ok ? Math.round(output * 12) : 0),
      ...(ok ? { first_byte_ms: attempts.reduce((a, x) => a + x.ms, 0) } : {}),
      attempts,
      input_tokens: ok ? input : 0,
      output_tokens: ok ? output : 0,
      cache_read_tokens: ok ? cached : 0,
      cache_write_tokens: ok ? Math.round(input * 0.1) : 0,
      cost_micros: ok ? cost(last.model, shape.size) : 0,
      ...(last ? (ok ? {} : { error: last.error ?? `${last.status}` }) : { error: "no upstream can take qwen3-coder now: GPU box (Ollama)'s circuit is open" }),
      ...(this.settings?.log_bodies && live ? { has_body: true } : {}),
    };
    // Usage, as the gateway rolls it up by the hour.
    const upstream = ok ? last.upstream : (attempts[0]?.upstream ?? "");
    this.usage.push({
      at: Math.floor(at / HOUR) * HOUR,
      key: "",
      model: e.model,
      machine: e.machine ?? "",
      project: e.project ?? "",
      thread: e.thread ?? "",
      upstream,
      requests: 1,
      errors: ok ? 0 : 1,
      input_tokens: e.input_tokens,
      output_tokens: e.output_tokens,
      cache_read_tokens: e.cache_read_tokens,
      cache_write_tokens: e.cache_write_tokens,
      cost_micros: e.cost_micros,
    });
    for (const a of attempts) {
      const hh = this.health.get(a.upstream);
      if (!hh) continue;
      hh.requests_hour += live ? 1 : 0;
      if (a.status === 200) Object.assign(hh, { last_ok_at: at, p50_ms: hh.p50_ms ? Math.round(hh.p50_ms * 0.9 + a.ms * 0.1) : a.ms });
      else Object.assign(hh, { last_error: `${a.status} ${a.error ?? ""}`.trim(), last_error_at: at });
    }
    if (e.client) this.seen.set(e.client, at);
    return e;
  }

  /** A week of requests, a few a minute in the working hours, fewer at night. */
  private history(t: number) {
    const start = t - 7 * 24 * HOUR;
    for (let at = start; at < t - 60_000; ) {
      const hour = new Date(at).getUTCHours();
      const busy = hour >= 7 && hour <= 19 ? 1 : 0.25;
      at += Math.round((90_000 + this.random() * 240_000) / busy);
      const e = this.request(at, false);
      if (t - at < 2 * HOUR) this.log.push(e);
    }
    for (const hh of this.health.values()) if (hh.upstream !== "gpu-box") hh.requests_hour = Math.round(40 + this.random() * 120);
    this.log.sort((a, b) => b.seq - a.seq);
    this.health.get("anthropic")!.p50_ms = 640;
    this.health.get("bedrock")!.p50_ms = 910;
    this.health.get("openai")!.p50_ms = 520;
  }

  /** New requests every couple of seconds while anyone looks (the timers stop with the demo). */
  private tick() {
    if (this.ticking) return;
    this.ticking = true;
    const next = () =>
      this.h.after(1800 + Math.round(this.random() * 1600), () => {
        if (!this.settings?.enabled) return void (this.ticking = false);
        this.log.unshift(this.request(now(), true));
        if (this.log.length > 2000) this.log.length = 2000;
        next();
      });
    next();
  }

  // ── the host's commands ──

  async handle(machine: MachineId, c: Extract<Command, { type: `gateway.${string}` }>): Promise<Reply> {
    const bad = (message: string): Reply => ({ type: "error", code: "invalid", message });
    const missing: Reply = { type: "error", code: "not_found", message: "the gateway isn't set up on this machine" };
    if (c.type === "gateway.configure") {
      const checked = checkSettings(c.settings);
      if ("error" in checked) return bad(checked.error);
      if (this.host && this.host !== machine) return bad("the gateway runs on another machine: remove it there first");
      const was = this.settings?.enabled;
      this.host = machine;
      this.settings = checked.settings;
      for (const u of checked.settings.upstreams ?? []) if (!this.health.has(u.id)) this.health.set(u.id, { upstream: u.id, circuit: "closed", failures: 0, requests_hour: 0 });
      if (checked.settings.enabled && !was) this.since = now();
      this.publish();
      if (checked.settings.enabled) this.tick();
      return { type: "ok" };
    }
    if (machine !== this.host || !this.settings) return missing;
    switch (c.type) {
      case "gateway.key": {
        if (!this.settings.upstreams?.some((u) => u.id === c.upstream)) return { type: "error", code: "not_found", message: `no upstream ${c.upstream}` };
        // The demo can't open a sealed key (only the daemon can): it notes that one is set.
        if (c.key?.type === "set") this.keys.set(c.upstream, "sealed");
        else if (c.key?.type === "clear") this.keys.set(c.upstream, "none");
        this.publish();
        return { type: "ok" };
      }
      case "gateway.client.issue": {
        const t = now();
        const client: GatewayClient = { id: `gwc_${Math.floor(this.random() * 2 ** 48).toString(16).padStart(16, "0").slice(0, 16)}`, name: c.name, ...(c.machine ? { machine: c.machine } : {}), created_at: t };
        this.clients = [...this.clients.filter((x) => !c.machine || x.machine !== c.machine), client];
        // Minted, sealed to the machine that will use it as its `roll-gateway` endpoint's key, dropped.
        const sealed = await sealKey(c.seal_key, GATEWAY_LIMITS.endpointId, `rgk_demo_${client.id}`);
        this.publish();
        return { type: "gateway_client", client, sealed };
      }
      case "gateway.client.revoke": {
        if (!this.clients.some((x) => x.id === c.client)) return { type: "error", code: "not_found", message: `no client ${c.client}` };
        this.clients = this.clients.filter((x) => x.id !== c.client);
        this.publish();
        return { type: "ok" };
      }
      case "gateway.test": {
        const ups = (this.settings.upstreams ?? []).filter((u) => !c.upstream || u.id === c.upstream);
        if (c.upstream && !ups.length) return { type: "error", code: "not_found", message: `no upstream ${c.upstream}` };
        await new Promise<void>((r) => this.h.after(500, r));
        return {
          type: "gateway_tested",
          checks: ups.map((u) => {
            const key = u.key_ref ? "reference" : this.keys.get(u.id);
            if (u.id === "gpu-box") return { upstream: u.id, ok: false, ms: 3, models: 0, error: "GPU box (Ollama) didn't answer: connect: connection refused" };
            if (key === "none" && u.kind !== "openai_compatible") return { upstream: u.id, ok: false, ms: 0, models: 0, error: `${u.name} has no key` };
            return { upstream: u.id, ok: true, ms: Math.round(120 + this.random() * 300), models: u.models?.length ?? Math.round(8 + this.random() * 30) };
          }),
        };
      }
      case "gateway.remove": {
        this.settings = null;
        this.clients = [];
        this.keys.clear();
        this.health.clear();
        this.log = [];
        this.usage = [];
        const host = this.host;
        this.host = null;
        this.h.setInfo(host, undefined);
        return { type: "ok" };
      }
      case "gateway.status": {
        const info = this.info()!;
        return {
          type: "gateway_status",
          run: info.run,
          upstreams: (this.settings.upstreams ?? []).map((u) => ({ ...(this.health.get(u.id) ?? { upstream: u.id, circuit: "closed" as const, failures: 0, requests_hour: 0 }) })),
          clients_seen: [...this.seen.entries()].map(([client, at]) => ({ client, at })),
          models: [...(this.settings.routes ?? []).map((r) => r.model), "gpt-5.5", "gpt-5.5-codex", "qwen3-coder"],
        };
      }
      case "gateway.usage": {
        const step = c.bucket === "hour" ? HOUR : 24 * HOUR;
        const by = c.by ?? "model";
        const sums = new Map<string, UsageRow>();
        for (const u of this.usage) {
          if (u.at < c.from || u.at >= c.to) continue;
          const at = Math.floor(u.at / step) * step;
          const key = u[by];
          const k = `${at}\u0000${key}`;
          const s = sums.get(k) ?? { at, key, requests: 0, errors: 0, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0, cost_micros: 0 };
          s.requests += u.requests;
          s.errors += u.errors;
          s.input_tokens += u.input_tokens;
          s.output_tokens += u.output_tokens;
          s.cache_read_tokens += u.cache_read_tokens;
          s.cache_write_tokens += u.cache_write_tokens;
          s.cost_micros += u.cost_micros;
          sums.set(k, s);
        }
        const rows = [...sums.values()].sort((a, b) => a.at - b.at || a.key.localeCompare(b.key));
        const truncated = rows.length > GATEWAY_LIMITS.usageRows;
        return { type: "gateway_usage", by, bucket: c.bucket ?? "day", from: c.from, to: c.to, rows: rows.slice(-GATEWAY_LIMITS.usageRows), ...(truncated ? { truncated } : {}) };
      }
      case "gateway.log": {
        const limit = Math.min(c.limit || GATEWAY_LIMITS.logPage, GATEWAY_LIMITS.logPage);
        const keep = (e: LogEntry) =>
          (c.after === undefined || e.seq > c.after) && (c.before === undefined || e.seq < c.before) && (!c.errors || e.status >= 400 || !!e.error || e.attempts.length > 1);
        return { type: "gateway_log", entries: this.log.filter(keep).slice(0, limit) };
      }
      case "gateway.log.body": {
        const e = this.log.find((x) => x.seq === c.seq);
        if (!e?.has_body) return { type: "error", code: "not_found", message: "no body was kept for that request" };
        const request = JSON.stringify({ model: e.model, max_tokens: 32000, stream: e.stream, messages: [{ role: "user", content: "Fix the flaky queue test in crates/code-control/tests/queue.rs" }] }, null, 2);
        const response =
          e.format === "messages"
            ? `event: message_start\ndata: {"type":"message_start","message":{"model":"${e.model}","usage":{"input_tokens":${e.input_tokens}}}}\n\nevent: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I'll look at the lease check first."}}\n\n`
            : `data: {"type":"response.output_text.delta","delta":"I'll look at the lease check first."}\n\n`;
        return { type: "gateway_log_body", seq: e.seq, request, response };
      }
    }
  }

  // ── the plan, as the relay keeps it ──

  api(): GatewayApi {
    const owner = () => this.start();
    const plan = () => (this.plan ? structuredClone(this.plan) : null);
    const need = () => {
      if (!this.plan) throw new ApiError("no gateway plan", 404, null);
      return this.plan;
    };
    return {
      get: async () => (owner(), plan()),
      save: async (_o, next) => {
        owner();
        if (!this.h.machines().has(next.host)) throw new ApiError(`no machine ${next.host}`, 422, null);
        if (this.plan && this.plan.host !== next.host && (this.plan.members ?? []).length) throw new ApiError("remove the machines first: they use the gateway on its current computer", 409, null);
        this.plan = { ...next, members: this.plan?.members ?? [], updated_at: now() };
        return plan()!;
      },
      remove: async () => {
        owner();
        if ((this.plan?.members ?? []).length) throw new ApiError("remove the machines first", 409, null);
        this.plan = null;
      },
      join: async (_o, join) => {
        owner();
        const p = need();
        const m = this.h.machines().get(join.machine);
        if (!m) throw new ApiError(`no machine ${join.machine}`, 404, null);
        const url = join.url ?? (join.machine === p.host ? loopbackUrl(this.settings?.port ?? GATEWAY_PORT) : p.urls?.[0]?.url);
        if (!url) throw new ApiError("the plan has no URL for members to use", 422, null);
        const record = (error?: string, client?: string) => {
          const members = (p.members ?? []).filter((x) => x.machine !== join.machine);
          p.members = [...members, { machine: join.machine, url, ...(join.providers?.length ? { providers: join.providers } : {}), ...(client ? { client } : {}), joined_at: now(), ...(error ? { error } : {}) }];
        };
        if (!m.online || !m.info?.seal_key) return record("the machine isn't connected");
        const issued = await this.h.command(p.host, { type: "gateway.client.issue", name: m.name, machine: m.id, seal_key: m.info.seal_key });
        if (issued.type !== "gateway_client") return record(issued.type === "error" ? issued.message : "the gateway's computer didn't answer");
        record(undefined, issued.client.id);
        const spec = this.endpointFor(m.id)!;
        const { key: _k, models: _m, checked_at: _c, ...endpoint } = spec;
        // The box passes on unchanged: only ciphertext crosses the relay.
        const set = await this.h.command(m.id, { type: "endpoint.set", endpoint, key: { type: "set", sealed: issued.sealed } });
        if (set.type === "error") record(set.message, issued.client.id);
      },
      leave: async (_o, machine) => {
        owner();
        const p = need();
        const member = p.members?.find((x) => x.machine === machine);
        if (!member) throw new ApiError(`${machine} doesn't use the gateway`, 404, null);
        p.members = (p.members ?? []).filter((x) => x.machine !== machine);
        // Best effort, as the relay does: the endpoint goes, then the key is revoked.
        await this.h.command(machine, { type: "endpoint.remove", endpoint: GATEWAY_LIMITS.endpointId }).catch(() => {});
        if (member.client) await this.h.command(p.host, { type: "gateway.client.revoke", client: member.client }).catch(() => {});
      },
    };
  }

  /** Start the ticking log once anyone reads the gateway. */
  start() {
    if (this.settings?.enabled) this.tick();
  }
}
