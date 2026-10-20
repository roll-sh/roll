/**
 * Serverless Redis: the control-plane API (`/v1/redis/...`, crates/redis-control/src/api.rs) and
 * the Upstash-compatible HTTP data API every database serves (crates/redis-server/src/http.rs).
 */

import { call } from "./roll-api";

export interface RedisDefaults {
  max_memory_mb: number;
  eviction_policy: string;
  idle_timeout_s: number;
  history_retention_s: number;
}

export interface RedisPricing {
  usd_per_gib_hour: number;
  usd_per_million_commands: number;
  usd_per_gb_month_s3: number;
  usd_per_gb_out: number;
  elasticache_usd_per_gb_hour: number;
  elasticache_usd_per_million_ecpu: number;
  elasticache_min_gb: number;
}

export interface RedisInfo {
  redis_version: string;
  region: string;
  /** host:port of the RESP endpoint. */
  resp: string;
  /** host:port of the HTTP data API. */
  http: string;
  tls: boolean;
  storage: string;
  defaults: RedisDefaults;
  pricing: RedisPricing;
  loaded: number;
}

export interface RedisConnection {
  redis_url: string;
  rest_url: string;
  rest_token: string;
  host: string;
  port: number;
}

/** Loaded in memory ("active") or scaled to zero ("idle"). */
export interface RedisStatus {
  state: "active" | "idle";
  used_memory?: number;
  keys?: number;
  connections?: number;
  commands?: number;
  idle_ms?: number;
  durable_offset?: number | null;
  offset?: number;
}

export interface ReplicaStatus {
  state: "active" | "idle";
  used_memory?: number;
  connections?: number;
  offset?: number;
  /** Bytes of the write stream the replica is behind its (loaded) primary. */
  lag_bytes?: number | null;
}

export interface RedisReplica extends RedisConnection {
  id: string;
  db_id: string;
  region: string;
  created_at: number;
  /** Absent on older rolld builds. */
  status?: ReplicaStatus;
}

export interface RedisSettings {
  max_memory_mb: number;
  eviction_policy: string;
  durability: "async" | "sync" | string;
  idle_timeout_s: number;
  history_retention_s: number;
}

export interface RedisDb extends RedisConnection, RedisSettings {
  id: string;
  name: string;
  region: string;
  parent_id: string | null;
  parent_at: number | null;
  created_at: number;
  status: RedisStatus;
  replicas: RedisReplica[];
}

export interface RedisUsage {
  db_id: string;
  days: number;
  active_seconds: number;
  gib_hours: number;
  commands: number;
  net_in_bytes: number;
  net_out_bytes: number;
  peak_memory_bytes: number;
  storage_bytes: number;
  cost: {
    memory_usd: number;
    compute_usd: number;
    storage_usd: number;
    egress_usd: number;
    total_usd: number;
    elasticache_serverless_usd: number;
  };
}

const db = (id: string) => `/v1/redis/databases/${encodeURIComponent(id)}`;

export const redisApi = {
  info: () => call<RedisInfo>("GET", "/v1/redis"),
  list: () => call<RedisDb[]>("GET", "/v1/redis/databases"),
  get: (id: string) => call<RedisDb>("GET", db(id)),
  create: (name: string, settings: Partial<RedisSettings>) =>
    call<RedisDb>("POST", "/v1/redis/databases", { name, ...settings }),
  update: (id: string, settings: Partial<RedisSettings> & { name?: string }) => call<RedisDb>("PATCH", db(id), settings),
  remove: (id: string) => call<{ deleted: string }>("DELETE", db(id)),
  branch: (id: string, name: string, at?: number) => call<RedisDb>("POST", `${db(id)}/branches`, { name, at }),
  restore: (id: string, at: number) =>
    call<{ restored_to: number; previous_state: string }>("POST", `${db(id)}/restore`, { at }),
  rotatePassword: (id: string) => call<RedisDb>("POST", `${db(id)}/password`, {}),
  addReplica: (id: string) => call<RedisReplica>("POST", `${db(id)}/replicas`, {}),
  deleteReplica: (replica: string) => call<{ deleted: string }>("DELETE", `/v1/redis/replicas/${encodeURIComponent(replica)}`),
  usage: (id: string, days = 30) => call<RedisUsage>("GET", `${db(id)}/usage?days=${days}`),
  unload: (id: string) => call<{ unloaded: boolean }>("POST", `${db(id)}/unload`, {}),
};

export const EVICTION_POLICIES = [
  "noeviction",
  "allkeys-lru",
  "allkeys-lfu",
  "allkeys-random",
  "volatile-lru",
  "volatile-lfu",
  "volatile-random",
  "volatile-ttl",
];

export const MEMORY_STEPS = [32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384];

export const IDLE_OPTIONS = [
  { label: "1 min", value: 60 },
  { label: "5 min", value: 300 },
  { label: "15 min", value: 900 },
  { label: "1 hour", value: 3600 },
  { label: "Always on", value: 0 },
];

export const RETENTION_OPTIONS = [
  { label: "None", value: 0 },
  { label: "1 hour", value: 3600 },
  { label: "24 hours", value: 86_400 },
  { label: "7 days", value: 7 * 86_400 },
  { label: "30 days", value: 30 * 86_400 },
];

/** Options plus the current value if it isn't one of them (set from the CLI, say). */
export function withCurrent(options: { label: string; value: number }[], value: number, fmt: (v: number) => string) {
  return options.some((o) => o.value === value) ? options : [...options, { label: fmt(value), value }];
}

export function formatMb(mb: number) {
  return mb >= 1024 && mb % 1024 === 0 ? `${mb / 1024} GB` : `${mb} MB`;
}

export function formatDuration(s: number) {
  if (s === 0) return "0s";
  if (s % 86_400 === 0) return `${s / 86_400}d`;
  if (s % 3600 === 0) return `${s / 3600}h`;
  if (s % 60 === 0) return `${s / 60}m`;
  return `${s}s`;
}

/** Elapsed seconds, e.g. `47s`, `1m 47s`, `2h 5m`. */
export function formatElapsed(s: number) {
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

export function formatCount(n: number | null | undefined) {
  if (n == null) return "—";
  if (n < 10_000) return n.toLocaleString("en-US");
  if (n < 1e6) return `${(n / 1e3).toFixed(1)}k`;
  if (n < 1e9) return `${(n / 1e6).toFixed(n < 1e7 ? 2 : 1)}M`;
  return `${(n / 1e9).toFixed(2)}B`;
}

/** Hide the password in a `redis://` URL. */
export function maskRedisUrl(url: string) {
  return url.replace(/^(rediss?:\/\/[^:]*:)[^@]+@/, "$1••••••••@");
}

export function maskToken(token: string) {
  const i = token.indexOf("_", token.indexOf("_") + 1);
  return i > 0 ? `${token.slice(0, i + 1)}••••••••` : "••••••••";
}

// ---------------------------------------------------------------------------------------------
// Data API (Upstash REST protocol). Replies are requested as raw RESP2 so the console can show
// them like redis-cli does (status vs bulk strings, integers, nils, nested errors).

export type Resp =
  | { t: "status"; v: string }
  | { t: "error"; v: string }
  | { t: "int"; v: string }
  | { t: "bulk"; v: Uint8Array }
  | { t: "double"; v: string }
  | { t: "bool"; v: boolean }
  | { t: "nil" }
  | { t: "array"; v: Resp[] }
  | { t: "map"; v: [Resp, Resp][] };

const CRLF = 13;

/** Parse every reply in a RESP2 (or RESP3) buffer. */
export function parseResp(buf: Uint8Array): Resp[] {
  const ascii = new TextDecoder("latin1");
  let pos = 0;
  const line = () => {
    let end = pos;
    while (end < buf.length && !(buf[end] === CRLF && buf[end + 1] === 10)) end++;
    const s = ascii.decode(buf.subarray(pos, end));
    pos = end + 2;
    return s;
  };
  const one = (): Resp => {
    const kind = String.fromCharCode(buf[pos++]);
    const head = line();
    switch (kind) {
      case "+":
        return { t: "status", v: utf8(ascii, buf.subarray(pos - head.length - 2, pos - 2)) };
      case "-":
      case "!":
        return { t: "error", v: kind === "!" ? bulkAt(Number(head)) : utf8(ascii, buf.subarray(pos - head.length - 2, pos - 2)) };
      case ":":
      case "(":
        return { t: "int", v: head };
      case ",":
        return { t: "double", v: head };
      case "#":
        return { t: "bool", v: head === "t" };
      case "_":
        return { t: "nil" };
      case "$":
      case "=": {
        const n = Number(head);
        if (n < 0) return { t: "nil" };
        const v = buf.slice(pos, pos + n);
        pos += n + 2;
        return { t: "bulk", v: kind === "=" ? v.subarray(4) : v };
      }
      case "*":
      case "~":
      case ">": {
        const n = Number(head);
        if (n < 0) return { t: "nil" };
        const v: Resp[] = [];
        for (let i = 0; i < n; i++) v.push(one());
        return { t: "array", v };
      }
      case "%": {
        const n = Number(head);
        const v: [Resp, Resp][] = [];
        for (let i = 0; i < n; i++) v.push([one(), one()]);
        return { t: "map", v };
      }
      default:
        throw new Error(`unexpected RESP type ${JSON.stringify(kind)}`);
    }
  };
  const bulkAt = (n: number) => {
    const s = utf8(ascii, buf.subarray(pos, pos + n));
    pos += n + 2;
    return s;
  };
  const out: Resp[] = [];
  while (pos < buf.length) out.push(one());
  return out;
}

function utf8(fallback: TextDecoder, b: Uint8Array) {
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(b);
  } catch {
    return fallback.decode(b);
  }
}

const strict = new TextDecoder("utf-8", { fatal: true });

/** Bytes as text: UTF-8 when valid, else redis-cli style `\xHH` escapes. */
export function bytesText(b: Uint8Array) {
  try {
    return strict.decode(b);
  } catch {
    let s = "";
    for (const c of b) s += c >= 0x20 && c < 0x7f ? String.fromCharCode(c) : `\\x${c.toString(16).padStart(2, "0")}`;
    return s;
  }
}

/** A reply as plain text (bulk/status/int/double), or null for nil/aggregates. */
export function respText(r: Resp | undefined): string | null {
  if (!r) return null;
  switch (r.t) {
    case "bulk":
      return bytesText(r.v);
    case "status":
    case "int":
    case "double":
    case "error":
      return r.v;
    case "bool":
      return r.v ? "1" : "0";
    default:
      return null;
  }
}

export function respArray(r: Resp | undefined): Resp[] {
  if (!r) return [];
  if (r.t === "array") return r.v;
  if (r.t === "map") return r.v.flat();
  return [];
}

export class RespError extends Error {}

/** A client for one database's HTTP data API, authenticated with its REST token. */
export function dataApi(conn: Pick<RedisConnection, "rest_url" | "rest_token">) {
  const post = async (path: string, body: unknown): Promise<Resp[]> => {
    const res = await fetch(`${conn.rest_url}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${conn.rest_token}`,
        "content-type": "application/json",
        "upstash-response-format": "resp2",
      },
      body: JSON.stringify(body),
      cache: "no-store",
    });
    if (res.headers.get("content-type")?.includes("application/json")) {
      const data = await res.json().catch(() => null);
      throw new RespError(data?.error ?? `${res.status} ${res.statusText}`);
    }
    if (!res.ok) throw new RespError(`${res.status} ${res.statusText}`);
    return parseResp(new Uint8Array(await res.arrayBuffer()));
  };
  return {
    /** One command; error replies come back as `{ t: "error" }`, not thrown. */
    run: async (args: string[]) => (await post("/", args))[0] ?? ({ t: "nil" } as Resp),
    /** Several commands in one round trip, in order, not atomic. */
    pipeline: (cmds: string[][]) => (cmds.length ? post("/pipeline", cmds) : Promise.resolve([])),
  };
}

/** Split a command line like redis-cli: whitespace, "double quotes" with escapes, 'single quotes'. */
export function splitArgs(line: string): string[] {
  const out: string[] = [];
  let i = 0;
  while (i < line.length) {
    while (i < line.length && /\s/.test(line[i])) i++;
    if (i >= line.length) break;
    let cur = "";
    while (i < line.length && !/\s/.test(line[i])) {
      const c = line[i];
      if (c === '"' || c === "'") {
        const q = c;
        i++;
        while (i < line.length && line[i] !== q) {
          if (q === '"' && line[i] === "\\" && i + 1 < line.length) {
            const n = line[i + 1];
            if (n === "x" && /^[0-9a-fA-F]{2}$/.test(line.slice(i + 2, i + 4))) {
              cur += String.fromCharCode(parseInt(line.slice(i + 2, i + 4), 16));
              i += 4;
              continue;
            }
            cur += ({ n: "\n", r: "\r", t: "\t", b: "\b", a: "\x07" } as Record<string, string>)[n] ?? n;
            i += 2;
            continue;
          }
          if (q === "'" && line[i] === "\\" && line[i + 1] === "'") {
            cur += "'";
            i += 2;
            continue;
          }
          cur += line[i++];
        }
        if (i >= line.length) throw new Error("unbalanced quotes");
        i++;
      } else {
        cur += c;
        i++;
      }
    }
    out.push(cur);
  }
  return out;
}

/** Quote a value like redis-cli does for bulk strings. */
export function quote(s: string) {
  return `"${s.replace(/[\\"\n\r\t]/g, (c) => ({ "\\": "\\\\", '"': '\\"', "\n": "\\n", "\r": "\\r", "\t": "\\t" })[c]!)}"`;
}
