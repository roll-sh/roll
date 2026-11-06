/**
 * roll's model gateway in the dashboard (docs/code/gateway.md):
 *
 * - **The plan** on the relay (`GatewayApi`): which machine hosts the gateway, the URLs members
 *   reach it by, and which machines use it (`/v1/code/agents/{owner}/gateway`, its `members`).
 *   Joining a machine is the relay's: it asks the host for a client key sealed to that machine and
 *   passes the box on in `endpoint.set`, so neither the relay nor the browser sees a client key.
 * - **The host's commands** (`gatewayOn`): `gateway.configure`, an upstream's key sealed to the
 *   host in this browser (`gateway.key`, `sealUpstreamKey`), test, status, usage, the log.
 * - **The settings checked** as the daemon checks them (agent-proto `GatewaySettings::check`), the
 *   upstream kinds' presets, the URLs a member may use, **the notice's words** (one place: every
 *   `GatewayNotice` says them), and the sums the usage charts draw.
 *
 * Limits that aren't settings: upstreams take API keys and cloud credentials only (never a
 * Claude.ai or ChatGPT sign-in), requests go out as the gateway (never dressed as another
 * client), and a 429 cools that upstream down rather than trying another key of the provider.
 */

import {
  GATEWAY_LIMITS,
  GATEWAY_PORT,
  GATEWAY_RETENTION,
  gatewayPlanD,
  type GatewayJoin,
  type GatewayPlan,
  type GatewaySettings,
  type GatewayUrl,
  type KeyChange,
  type LogEntry,
  type Machine,
  type MachineId,
  type ProviderId,
  type Reply,
  type RouteSpec,
  type UpstreamKind,
  type UpstreamSpec,
  type UsageBucket,
  type UsageBy,
  type UsageReport,
  type UsageRow,
} from "./agents-api";
import type { AgentsClient } from "./agents-client";
import type { Call } from "./automations-api";
import { ApiError } from "./code-api";
import { decode } from "./decode";
import { sealUpstreamKey } from "./agents-seal";

const enc = encodeURIComponent;

// ── the plan on the relay ──

/** The owner's gateway plan (owner only: agent tokens get a 403). `get` answers `null` before one is set. */
export interface GatewayApi {
  get(owner: string): Promise<GatewayPlan | null>;
  /** Set the plan (host, URLs, toggles, the notice's acknowledgement). Changing the host while machines use it is refused. */
  save(owner: string, plan: GatewayPlan): Promise<GatewayPlan>;
  /** Forget the plan (members first: `leave` each). */
  remove(owner: string): Promise<void>;
  /** Join a machine: the relay asks the host for its key, sealed to the machine, and sets it there. */
  join(owner: string, join: GatewayJoin): Promise<void>;
  /** The machine stops using it: its endpoint goes and its client key is revoked. */
  leave(owner: string, machine: MachineId): Promise<void>;
}

/** `GatewayApi` over the relay. */
export function createGatewayApi(call: Call): GatewayApi {
  const base = (owner: string) => `/v1/code/agents/${enc(owner)}/gateway`;
  return {
    get: async (owner) => {
      try {
        const v = await call("GET", base(owner));
        return v === null || v === undefined || v === "" ? null : decode(gatewayPlanD, v);
      } catch (e) {
        if (e instanceof ApiError && e.status === 404) return null;
        throw e;
      }
    },
    save: async (owner, plan) => decode(gatewayPlanD, await call("PUT", base(owner), plan)),
    remove: async (owner) => {
      await call("DELETE", base(owner));
    },
    join: async (owner, join) => {
      await call("POST", `${base(owner)}/members`, join);
    },
    leave: async (owner, machine) => {
      await call("DELETE", `${base(owner)}/members/${enc(machine)}`);
    },
  };
}

// ── the host's commands ──

type R<T extends Reply["type"]> = Extract<Reply, { type: T }>;

/** The gateway's commands to its host, the keys sealed to it here. */
export function gatewayOn(client: AgentsClient, host: Machine) {
  const run = <T extends Reply["type"]>(command: Parameters<AgentsClient["run"]>[1], type: T) => client.run<Reply>(host.id, command, type as Reply["type"]) as Promise<R<T>>;
  return {
    configure: (settings: GatewaySettings) => run({ type: "gateway.configure", settings }, "ok"),
    /** A typed key sealed to the host for `upstream` (`null` clears it). The plaintext goes nowhere else. */
    setKey: async (upstream: string, key: string | null) => run({ type: "gateway.key", upstream, key: await upstreamKeyChange(host.info?.seal_key, upstream, key) }, "ok"),
    test: (upstream?: string) => run({ type: "gateway.test", ...(upstream ? { upstream } : {}) }, "gateway_tested"),
    status: () => run({ type: "gateway.status" }, "gateway_status"),
    usage: (from: number, to: number, by: UsageBy, bucket: UsageBucket) => run({ type: "gateway.usage", from, to, by, bucket }, "gateway_usage"),
    log: (q: { after?: number; before?: number; errors?: boolean; limit?: number }) =>
      run(
        {
          type: "gateway.log",
          ...(q.after !== undefined ? { after: q.after } : {}),
          ...(q.before !== undefined ? { before: q.before } : {}),
          limit: Math.min(q.limit ?? 100, GATEWAY_LIMITS.logPage),
          ...(q.errors ? { errors: true } : {}),
        },
        "gateway_log",
      ),
    body: (seq: number) => run({ type: "gateway.log.body", seq }, "gateway_log_body"),
    revoke: (clientId: string) => run({ type: "gateway.client.revoke", client: clientId }, "ok"),
    remove: () => run({ type: "gateway.remove" }, "ok"),
  };
}

export type GatewayCommands = ReturnType<typeof gatewayOn>;

const KEY_MAX = 64 * 1024;

/** A key typed in the browser as `gateway.key` takes it: sealed to the host for that upstream. */
export async function upstreamKeyChange(hostSealKey: string | undefined, upstream: string, key: string | null): Promise<KeyChange> {
  if (key === null) return { type: "clear" };
  // A Vertex service account is JSON over several lines; anything else is one line.
  const trimmed = key.trim();
  if (!trimmed || trimmed.length > KEY_MAX || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/.test(trimmed)) throw new Error(`a key is 1 to ${KEY_MAX} bytes of text`);
  if (!hostSealKey) throw new Error("the gateway's computer runs a roll too old to take keys from roll: update it");
  return { type: "set", sealed: await sealUpstreamKey(hostSealKey, upstream, trimmed) };
}

// ── the upstream kinds ──

export interface KindPreset {
  label: string;
  /** What it speaks, for the row. */
  speaks: string;
  /** The base URL's placeholder, or `null` when it has a fixed one. */
  url: string | null;
  needsUrl: boolean;
  region?: string;
  project?: boolean;
  /** Patterns it needs (Bedrock can't list its models). */
  models?: string;
  /** `optional`: it can go without one (a local model server); every other kind refuses requests without it. */
  key: { label: string; placeholder: string; hint: string; multiline?: boolean; optional?: boolean };
}

/** The kinds, in the order the picker offers them. */
export const KIND_PRESETS: Record<UpstreamKind, KindPreset> = {
  anthropic: {
    label: "Anthropic",
    speaks: "Messages",
    url: null,
    needsUrl: false,
    key: { label: "API key", placeholder: "sk-ant-…", hint: "An API key from console.anthropic.com. Never a Claude.ai login: subscriptions don't go through a gateway." },
  },
  openai: {
    label: "OpenAI",
    speaks: "Chat and Responses",
    url: null,
    needsUrl: false,
    key: { label: "API key", placeholder: "sk-…", hint: "An API key from platform.openai.com. Never a ChatGPT login." },
  },
  azure_openai: {
    label: "Azure OpenAI",
    speaks: "Chat and Responses",
    url: "https://<resource>.openai.azure.com",
    needsUrl: true,
    key: { label: "API key", placeholder: "Key 1 or Key 2", hint: "The resource's key. Model names are your deployment names." },
  },
  bedrock: {
    label: "Amazon Bedrock",
    speaks: "Messages",
    url: null,
    needsUrl: false,
    region: "us-east-1",
    models: "anthropic.claude-*",
    key: { label: "Credentials", placeholder: "Bedrock API key, or ACCESS_KEY_ID:SECRET_ACCESS_KEY", hint: "A Bedrock API key, or an access key pair for SigV4." },
  },
  vertex: {
    label: "Google Vertex AI",
    speaks: "Messages (Claude), Chat",
    url: null,
    needsUrl: false,
    region: "us-east5",
    project: true,
    key: { label: "Service account JSON", placeholder: '{ "type": "service_account", … }', hint: "The service account's key file, pasted whole.", multiline: true },
  },
  openrouter: {
    label: "OpenRouter",
    speaks: "Chat",
    url: null,
    needsUrl: false,
    key: { label: "API key", placeholder: "sk-or-…", hint: "An API key from openrouter.ai." },
  },
  anthropic_compatible: {
    label: "Anthropic-compatible",
    speaks: "Messages",
    url: "https://gateway.example.com",
    needsUrl: true,
    key: { label: "API key", placeholder: "Key", hint: "Sent as x-api-key and a bearer token." },
  },
  openai_compatible: {
    label: "OpenAI-compatible",
    speaks: "Chat (Responses when it says so)",
    url: "https://models.example.com/v1",
    needsUrl: true,
    key: { label: "API key", placeholder: "Key (optional)", hint: "Sent as a bearer token. A local model server may need none.", optional: true },
  },
};

/** The base URL an upstream is reached at, for its row. */
export function upstreamUrl(u: UpstreamSpec): string {
  if (u.base_url) return u.base_url;
  if (u.kind === "anthropic") return "https://api.anthropic.com";
  if (u.kind === "openai") return "https://api.openai.com/v1";
  if (u.kind === "openrouter") return "https://openrouter.ai/api/v1";
  if (u.kind === "bedrock") return `bedrock-runtime.${u.region ?? "?"}.amazonaws.com`;
  if (u.kind === "vertex") return `${u.region ?? "?"}-aiplatform.googleapis.com · ${u.project ?? "?"}`;
  return "";
}

/** An id for a new upstream from its name, not one of `taken`. */
export function upstreamId(name: string, taken: Iterable<string>): string {
  const used = new Set(taken);
  const base =
    name
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 48) || "upstream";
  if (!used.has(base)) return base;
  for (let i = 2; ; i++) if (!used.has(`${base}-${i}`)) return `${base}-${i}`;
}

// ── settings, checked as the daemon checks them (agent-proto `GatewaySettings::check`) ──

const bytes = (s: string) => new TextEncoder().encode(s).length;
const ID = /^[a-z0-9_-]{1,64}$/;
const PLAIN = /^[A-Za-z0-9._-]{1,64}$/;
const CONTROL = /[\u0000-\u001f\u007f]/;
const RESERVED_HEADERS = new Set(["authorization", "x-api-key", "api-key", "proxy-authorization", "cookie", "host", "content-length", "transfer-encoding"]);

function checkModel(m: string): string | null {
  if (!m || bytes(m) > GATEWAY_LIMITS.model || /\s/.test(m) || CONTROL.test(m)) return `${JSON.stringify(m)} isn't a model name (1 to ${GATEWAY_LIMITS.model} bytes, no spaces)`;
  return null;
}

function checkPattern(p: string): string | null {
  return checkModel(p) ?? (p.replace(/\*+$/, "").includes("*") ? `${JSON.stringify(p)}: a model pattern has at most one *, at its end` : null);
}

/** `http(s)://host[:port][/path]`, no credentials, query or fragment. */
export function checkUrl(url: string, what = "an upstream's base URL"): string | null {
  const rest = url.startsWith("https://") ? url.slice(8) : url.startsWith("http://") ? url.slice(7) : null;
  if (rest === null) return `${what} starts with http:// or https://`;
  const authority = rest.split(/[/?]/)[0];
  if (url.length > GATEWAY_LIMITS.url || !authority || /[\s#?]/.test(url) || CONTROL.test(url)) return `${what} is http(s)://host[:port][/path], at most ${GATEWAY_LIMITS.url} bytes`;
  if (authority.includes("@")) return "put the key in the key field, not in the URL";
  return null;
}

/** Whether `s` is an IPv4 address literal. */
export function isIPv4(s: string): boolean {
  const parts = s.split(".");
  return parts.length === 4 && parts.every((p) => /^(0|[1-9]\d{0,2})$/.test(p) && Number(p) <= 255);
}

/** Whether `s` is an IPv6 address literal (no zone). */
export function isIPv6(s: string): boolean {
  if (!s.includes(":") || /[^0-9a-fA-F:.]/.test(s)) return false;
  const halves = s.split("::");
  if (halves.length > 2) return false;
  const groups = (h: string) => (h === "" ? [] : h.split(":"));
  const all = halves.flatMap(groups);
  let n = 0;
  for (const [i, g] of all.entries()) {
    if (i === all.length - 1 && g.includes(".")) {
      if (!isIPv4(g)) return false;
      n += 2;
    } else if (/^[0-9a-fA-F]{1,4}$/.test(g)) n += 1;
    else return false;
  }
  return halves.length === 2 ? n < 8 : n === 8;
}

/** Why the gateway can't listen on `a` (an IP literal it names), or `null`. */
export function listenError(a: string): string | null {
  const everywhere = "name each address the gateway listens on: the unspecified address would open it to every network";
  if (isIPv4(a)) {
    const b = a.split(".").map(Number);
    if (b.every((x) => x === 0)) return everywhere;
    if (b[0] >= 224 && b[0] <= 239) return `${a} is a multicast address`;
    return null;
  }
  if (isIPv6(a)) {
    if (/^[0:]+$/.test(a)) return everywhere;
    if (/^ff/i.test(a)) return `${a} is a multicast address`;
    return null;
  }
  return `${JSON.stringify(a)} isn't an IP address (the gateway listens on addresses, not names)`;
}

/** An upstream trimmed as the daemon keeps it, or why it can't be kept. */
export function checkUpstream(u: UpstreamSpec): { upstream: UpstreamSpec } | { error: string } {
  const p = KIND_PRESETS[u.kind];
  if (!ID.test(u.id)) return { error: `an upstream id is 1 to ${GATEWAY_LIMITS.id} of a-z, 0-9, - and _` };
  const name = u.name.trim();
  if (!name || bytes(name) > GATEWAY_LIMITS.name || CONTROL.test(name) || /[<>]/.test(name)) return { error: `an upstream's name is one line of 1 to ${GATEWAY_LIMITS.name} bytes, without < or >` };
  const text = (v?: string) => v?.trim() || undefined;
  const base_url = text(u.base_url)?.replace(/\/+$/, "");
  const region = text(u.region);
  const project = text(u.project);
  const key_ref = text(u.key_ref);
  if (base_url) {
    const e = checkUrl(base_url);
    if (e) return { error: e };
  } else if (p.needsUrl) return { error: `${p.label} needs a base URL` };
  if (u.kind === "bedrock" && !(region && PLAIN.test(region))) return { error: "Amazon Bedrock needs a region (us-east-1)" };
  if (u.kind === "vertex" && !(region && PLAIN.test(region) && project && PLAIN.test(project))) return { error: "Google Vertex AI needs a project and a region (us-east5)" };
  if ((region && !PLAIN.test(region)) || (project && !PLAIN.test(project))) return { error: "a region or project is letters, digits, -, _ and ." };
  if (u.responses && u.kind !== "openai_compatible") return { error: "only an OpenAI-compatible upstream says whether it speaks Responses" };
  const models = (u.models ?? []).map((m) => m.trim()).filter(Boolean);
  if (models.length > GATEWAY_LIMITS.models) return { error: `at most ${GATEWAY_LIMITS.models} models per upstream` };
  for (const m of models) {
    const e = checkPattern(m);
    if (e) return { error: e };
  }
  if (u.kind === "bedrock" && models.length === 0) return { error: "name the models Amazon Bedrock serves (anthropic.claude-*)" };
  const headers = (u.headers ?? []).map(([k, v]) => [k.trim().toLowerCase(), v] as [string, string]).filter(([k]) => k);
  if (headers.length > GATEWAY_LIMITS.headers) return { error: `at most ${GATEWAY_LIMITS.headers} headers` };
  for (const [k, v] of headers) {
    if (!/^[a-z0-9_-]{1,128}$/.test(k) || bytes(v) > GATEWAY_LIMITS.header || CONTROL.test(v)) return { error: `${JSON.stringify(k)} isn't a header name and value on one line` };
    if (RESERVED_HEADERS.has(k)) return { error: `${k} is set by the gateway (put a key in the key field)` };
  }
  if (key_ref && (!key_ref.startsWith("op://") || key_ref.length > 512 || /\s/.test(key_ref))) return { error: "a 1Password reference looks like op://vault/item/field" };
  const out: UpstreamSpec = { id: u.id, name, kind: u.kind };
  if (base_url) out.base_url = base_url;
  if (region) out.region = region;
  if (project) out.project = project;
  if (models.length) out.models = models;
  if (u.responses) out.responses = true;
  if (headers.length) out.headers = headers;
  if (u.allow_private) out.allow_private = true;
  if (key_ref) out.key_ref = key_ref;
  if (u.disabled) out.disabled = true;
  return { upstream: out };
}

/** The settings trimmed as the daemon keeps them, or why they can't be kept. */
export function checkSettings(s: GatewaySettings): { settings: GatewaySettings } | { error: string } {
  if (!Number.isInteger(s.port) || s.port < 1024 || s.port > 65535) return { error: "the gateway's port is 1024 to 65535" };
  const listen = (s.listen ?? []).map((a) => a.trim()).filter(Boolean);
  if (listen.length > GATEWAY_LIMITS.listen) return { error: `at most ${GATEWAY_LIMITS.listen} listen addresses` };
  for (const a of listen) {
    const e = listenError(a);
    if (e) return { error: e };
  }
  const ups = s.upstreams ?? [];
  if (ups.length > GATEWAY_LIMITS.upstreams) return { error: `at most ${GATEWAY_LIMITS.upstreams} upstreams` };
  const upstreams: UpstreamSpec[] = [];
  const ids = new Set<string>();
  for (const u of ups) {
    const c = checkUpstream(u);
    if ("error" in c) return c;
    if (ids.has(c.upstream.id)) return { error: `two upstreams are called ${c.upstream.id}` };
    ids.add(c.upstream.id);
    upstreams.push(c.upstream);
  }
  const rs = s.routes ?? [];
  if (rs.length > GATEWAY_LIMITS.routes) return { error: `at most ${GATEWAY_LIMITS.routes} routes` };
  const routes: RouteSpec[] = [];
  const models = new Set<string>();
  for (const r of rs) {
    const model = r.model.trim();
    const e = checkPattern(model);
    if (e) return { error: e };
    if (models.has(model)) return { error: `two routes are for ${model}` };
    models.add(model);
    if (r.targets.length === 0 || r.targets.length > GATEWAY_LIMITS.targets) return { error: `a route has 1 to ${GATEWAY_LIMITS.targets} targets` };
    const targets = [];
    for (const t of r.targets) {
      if (!ids.has(t.upstream)) return { error: `the route for ${model} names ${t.upstream}, which isn't an upstream` };
      const m = t.model?.trim();
      if (m) {
        const me = checkModel(m);
        if (me) return { error: me };
      }
      targets.push(m ? { upstream: t.upstream, model: m } : { upstream: t.upstream });
    }
    routes.push({ model, targets });
  }
  const prices = (s.prices ?? []).map((p) => ({ ...p, model: p.model.trim() }));
  if (prices.length > GATEWAY_LIMITS.models) return { error: `at most ${GATEWAY_LIMITS.models} prices` };
  for (const p of prices) {
    const e = checkPattern(p.model);
    if (e) return { error: e };
  }
  const retention = s.retention_days ?? GATEWAY_RETENTION;
  if (!Number.isInteger(retention) || retention < 1 || retention > 3650) return { error: "usage is kept 1 to 3650 days" };
  const out: GatewaySettings = { enabled: s.enabled, port: s.port };
  if (listen.length) out.listen = listen;
  if (upstreams.length) out.upstreams = upstreams;
  if (routes.length) out.routes = routes;
  if (prices.length) out.prices = prices;
  if (s.log_bodies) out.log_bodies = true;
  if (s.affinity === false) out.affinity = false;
  if (retention !== GATEWAY_RETENTION) out.retention_days = retention;
  return { settings: out };
}

/** New settings: on, the default port, no upstreams yet. (A function: agents-api imports this module.) */
export const newSettings = (): GatewaySettings => ({ enabled: true, port: GATEWAY_PORT });

// ── reaching it ──

/** The loopback address, spelled from its parts. */
export const LOOPBACK = [127, 0, 0, 1].join(".");

/** The URL the host itself uses (loopback). */
export const loopbackUrl = (port: number) => `http://${LOOPBACK}:${port}`;

/** The labels the owner picks from for an address (any other is fine too). */
export const URL_LABELS = ["Tailscale", "LAN", "Docker sandboxes"] as const;

/** The URLs `machine` may reach the gateway by: loopback on the host itself, then the owner's. */
export function urlChoices(plan: Pick<GatewayPlan, "host" | "urls">, port: number, machine: MachineId): GatewayUrl[] {
  const own = plan.urls ?? [];
  return machine === plan.host ? [{ label: "This computer", url: loopbackUrl(port) }, ...own] : own;
}

/** The URL a machine joins with by default: loopback on the host, else the plan's first. */
export function defaultUrl(plan: Pick<GatewayPlan, "host" | "urls">, port: number, machine: MachineId): string | undefined {
  return urlChoices(plan, port, machine)[0]?.url;
}

/** `http://<address>:<port>` from an address the owner typed (an IP literal or a name). */
export function urlFor(address: string, port: number): string {
  const a = address.trim();
  return `http://${isIPv6(a) ? `[${a}]` : a}:${port}`;
}

/**
 * Why a machine can't reach the gateway by any URL it has, or `null`. A hosted sandbox has no route
 * to the owner's computers (docs/code/gateway.md, "Hosted sandboxes"); anything else needs an
 * address in the plan.
 */
/** A sandbox roll hosts elsewhere (not Docker on the owner's computer): no route to the gateway. */
export const hostedSandbox = (machine: Pick<Machine, "sandbox">): boolean => !!machine.sandbox && machine.sandbox.backend !== "docker";

/**
 * The providers of `machine` that another of its endpoints serves now. A provider uses one endpoint,
 * so joining the gateway with them takes them from it (the daemon's `endpoint.set`): the wizard
 * asks first.
 */
export function onOtherEndpoints(machine: Machine): { provider: ProviderId; name: string; endpoint: string }[] {
  const names = new Map((machine.info?.providers ?? []).map((p) => [p.id, p.name]));
  const out: { provider: ProviderId; name: string; endpoint: string }[] = [];
  for (const e of machine.info?.settings?.endpoints ?? []) {
    if (e.id === GATEWAY_LIMITS.endpointId) continue;
    for (const p of e.providers ?? []) out.push({ provider: p, name: names.get(p) ?? p, endpoint: e.name || e.id });
  }
  return out;
}

/**
 * What a machine joins the gateway with: every provider (unset), or, when some are on another
 * endpoint, the others and those `moved` (an empty list: nothing joins).
 */
export function joinProviders(machine: Machine, moved: ReadonlySet<ProviderId>): ProviderId[] | undefined {
  const taken = onOtherEndpoints(machine).filter((t) => !moved.has(t.provider));
  if (!taken.length) return undefined;
  const kept = new Set(taken.map((t) => t.provider));
  return (machine.info?.providers ?? []).map((p) => p.id).filter((p) => !kept.has(p));
}

export function reachProblem(machine: Machine, plan: Pick<GatewayPlan, "host" | "urls">): string | null {
  if (machine.id === plan.host) return null;
  if (hostedSandbox(machine)) return "A hosted sandbox can't reach your computers: give it its own keys or login.";
  if (!(plan.urls ?? []).length) return "Add an address it can reach the gateway at (Tailscale, LAN or Docker) first.";
  return null;
}

// ── the notice (the user's explicit requirement: one wording, everywhere) ──

export type NoticeWho =
  /** A machine's own settings and its providers. */
  | { kind: "this" }
  /** Machines named (setup, the Gateway section's list). */
  | { kind: "machines"; names: readonly string[] }
  /** The new-sandbox dialog. */
  | { kind: "sandboxes" };

/** "a", "a and b", "a, b and c". */
export function listNames(names: readonly string[]): string {
  if (names.length <= 1) return names[0] ?? "";
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

/**
 * The notice: every AI request from the members goes out from the host, providers see the host's
 * network and IP address, and what happens when the host is offline (requests fail; with
 * fallback, the agents use their own logins or keys instead).
 */
export function noticeText(host: string, who: NoticeWho, fallback: boolean): string {
  const one = who.kind === "this" || (who.kind === "machines" && who.names.length === 1);
  const subject = who.kind === "this" ? "this machine" : who.kind === "sandboxes" ? "new sandboxes" : listNames(who.names) || "these machines";
  const again = who.kind === "machines" && !one ? "these machines" : subject;
  const not = who.kind === "this" ? "this machine's" : who.kind === "sandboxes" ? "the sandboxes'" : one ? `${subject}'s` : "these machines'";
  const tail = fallback ? `, and ${one ? "its" : "their"} agents use their own logins or keys instead.` : " until it's back.";
  return `All AI requests from ${subject} go out from ${host}. Providers see ${host}'s network and IP address, not ${not}. If ${host} is offline, requests from ${again} fail${tail}`;
}

/** What the host itself is told: its own agents' requests already go out from it. */
export function hostNoticeText(host: string): string {
  return `${host} runs the gateway: its own agents' AI requests go out from ${host} as before, and so do the requests of every machine that uses it.`;
}

/** The name the members' endpoint carries (and their providers' "via …"). */
export const memberEndpointName = (host: string) => `roll gateway on ${host}`;

// ── the request log ──

/** At most this many entries stay in the live tail. */
export const TAIL_CAP = 500;

/** Newer entries over the ones shown, newest first, each `seq` once, at most `cap`. */
export function mergeLog(shown: readonly LogEntry[], fresh: readonly LogEntry[], cap = TAIL_CAP): LogEntry[] {
  const bySeq = new Map<number, LogEntry>();
  for (const e of shown) bySeq.set(e.seq, e);
  for (const e of fresh) bySeq.set(e.seq, e);
  return [...bySeq.values()].sort((a, b) => b.seq - a.seq).slice(0, cap);
}

/** A failover: more than one upstream was tried. */
export const isFailover = (e: LogEntry) => e.attempts.length > 1;
/** What the errors filter keeps (the daemon's rule). */
export const isLogError = (e: LogEntry) => e.status >= 400 || !!e.error || isFailover(e);

// ── usage ──

export type UsageMetric = "cost" | "tokens" | "requests";

export const metricOf = (r: UsageRow, m: UsageMetric): number =>
  m === "cost" ? r.cost_micros : m === "requests" ? r.requests : r.input_tokens + r.output_tokens + r.cache_read_tokens + r.cache_write_tokens;

/** The key the folded series is drawn under. */
export const OTHER = "\u0000other";

export interface UsageSeries {
  key: string;
  values: number[];
  total: number;
}

export interface UsageStack {
  /** Bucket starts, oldest first, every bucket in the range (empty ones too). */
  at: number[];
  /** The biggest groups first, then `OTHER` when some were folded. */
  series: UsageSeries[];
  /** Per bucket, all series. */
  totals: number[];
  max: number;
}

const DAY = 86_400_000;
const HOUR = 3_600_000;

/**
 * The report as stacked columns: one per bucket from `from` to `to`, the `top` biggest groups by
 * `metric` and the rest folded into one series (so colours never cycle).
 */
/** The report's buckets: what the chart has a column for (at most 400). */
function buckets(report: Pick<UsageReport, "from" | "to" | "bucket">): { step: number; at: number[] } {
  const step = report.bucket === "hour" ? HOUR : DAY;
  const at: number[] = [];
  for (let t = Math.floor(report.from / step) * step; t < report.to && at.length < 400; t += step) at.push(t);
  return { step, at };
}

/** The rows the chart draws (inside its buckets): the totals add up these, so the two agree. */
export function rowsInRange(report: Pick<UsageReport, "rows" | "from" | "to" | "bucket">): UsageRow[] {
  const { step, at } = buckets(report);
  if (!at.length) return [];
  const first = at[0];
  const end = at[at.length - 1] + step;
  return report.rows.filter((r) => {
    const b = Math.floor(r.at / step) * step;
    return b >= first && b < end;
  });
}

export function stackUsage(report: Pick<UsageReport, "rows" | "from" | "to" | "bucket">, metric: UsageMetric, top = 4): UsageStack {
  const { step, at } = buckets(report);
  const index = new Map(at.map((t, i) => [t, i]));
  const byKey = new Map<string, number[]>();
  for (const r of report.rows) {
    const i = index.get(Math.floor(r.at / step) * step);
    if (i === undefined) continue;
    let v = byKey.get(r.key);
    if (!v) byKey.set(r.key, (v = new Array<number>(at.length).fill(0)));
    v[i] += metricOf(r, metric);
  }
  const all = [...byKey.entries()].map(([key, values]) => ({ key, values, total: values.reduce((a, b) => a + b, 0) })).sort((a, b) => b.total - a.total || a.key.localeCompare(b.key));
  const rest = all.slice(top);
  const series = rest.length ? [...all.slice(0, top), { key: OTHER, values: at.map((_, i) => rest.reduce((a, s) => a + s.values[i], 0)), total: rest.reduce((a, s) => a + s.total, 0) }] : all;
  const totals = at.map((_, i) => series.reduce((a, s) => a + s.values[i], 0));
  return { at, series: series.filter((s) => s.total > 0), totals, max: Math.max(0, ...totals) };
}

export interface UsageTotals {
  requests: number;
  errors: number;
  tokens: number;
  cost_micros: number;
  cache_read_tokens: number;
}

export function usageTotals(rows: readonly UsageRow[]): UsageTotals {
  const t: UsageTotals = { requests: 0, errors: 0, tokens: 0, cost_micros: 0, cache_read_tokens: 0 };
  for (const r of rows) {
    t.requests += r.requests;
    t.errors += r.errors;
    t.tokens += metricOf(r, "tokens");
    t.cost_micros += r.cost_micros;
    t.cache_read_tokens += r.cache_read_tokens;
  }
  return t;
}

/** A clean axis top over `max` (1, 2, 2.5 or 5 times a power of ten). */
export function niceMax(max: number): number {
  if (max <= 0) return 1;
  const p = 10 ** Math.floor(Math.log10(max));
  for (const m of [1, 2, 2.5, 5, 10]) if (m * p >= max) return m * p;
  return 10 * p;
}

/** 1,284 → "1.3K"; 4,200,000 → "4.2M". */
export function compact(n: number): string {
  if (n < 1000) return String(Math.round(n));
  for (const [v, s] of [
    [1e9, "B"],
    [1e6, "M"],
    [1e3, "K"],
  ] as const) {
    if (n >= v) {
      const x = n / v;
      return `${x >= 100 ? Math.round(x) : x.toFixed(1).replace(/\.0$/, "")}${s}`;
    }
  }
  return String(n);
}

/** `$12.34`, `$1.2K`; micro-dollars in. */
export function usd(micros: number): string {
  const d = micros / 1e6;
  if (d >= 10_000) return `$${compact(d)}`;
  if (d >= 100) return `$${Math.round(d).toLocaleString("en-US")}`;
  if (d >= 0.01 || d === 0) return `$${d.toFixed(2)}`;
  return "<$0.01";
}

export const formatMetric = (v: number, m: UsageMetric) => (m === "cost" ? usd(v) : compact(v));

/** An axis tick: under a cent, enough decimals that the ticks differ (they all read "<$0.01"). */
export function axisTick(v: number, m: UsageMetric, max: number): string {
  if (m !== "cost" || v === 0 || max / 1e6 >= 0.01) return formatMetric(v, m);
  const digits = Math.min(6, Math.ceil(-Math.log10(max / 1e6)) + 1);
  return `$${(v / 1e6).toFixed(digits)}`;
}
