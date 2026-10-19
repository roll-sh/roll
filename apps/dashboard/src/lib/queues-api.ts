/** Client for the roll queues API (`/v1/queues`, crates/mq-control). See docs/queues/contracts.md. */

import { ROLL_API } from "./roll-api";

export { ROLL_API, formatBytes, formatUsd } from "./roll-api";

export type QueueType = "classic" | "quorum" | "stream";

export interface Endpoints {
  amqp?: string;
  amqps?: string;
  mqtt?: string;
  stomp?: string;
  stream?: string;
  http?: string;
  management?: string;
}

export interface Overview {
  version: string;
  node: string;
  cluster_name: string;
  uptime_s: number;
  endpoints: Endpoints;
  totals: {
    vhosts: number;
    /** Vhosts scaled to zero; their last-known queues and messages are in the totals. */
    sleeping_vhosts?: number;
    connections: number;
    channels: number;
    exchanges: number;
    queues: number;
    consumers: number;
    messages_ready: number;
    messages_unacked: number;
    stored_bytes: number;
  };
  rates: { publish: number; deliver: number; ack: number };
  storage: { backend: string; metadata: string };
}

export interface RatePoint {
  t: number;
  publish: number;
  deliver: number;
  ack: number;
  ready: number;
  unacked: number;
  connections: number;
}

export interface Rates {
  step_s: number;
  points: RatePoint[];
}

export interface VHost {
  name: string;
  description: string;
  default_queue_type: string;
  state: "running" | "sleeping";
  /** Scaled to zero: queues, messages and bytes are the last-known totals (data is kept). */
  sleeping?: boolean;
  connections: number;
  queues: number;
  messages_ready: number;
  messages_unacked: number;
  stored_bytes: number;
  rates: { publish: number; deliver: number };
  limits: Record<string, number>;
  amqp_url: string;
  /** Only on create with `credentials: true`; the password is never shown again. */
  credentials?: VHostCredentials;
}

/** A user for one vhost and a ready-to-paste URL per enabled protocol. */
export interface VHostCredentials {
  user: string;
  password: string;
  urls: Partial<Record<"amqp" | "amqps" | "stream" | "stomp" | "mqtt" | "http" | "management", string | null>>;
}

export interface Queue {
  name: string;
  vhost: string;
  type: QueueType;
  durable: boolean;
  auto_delete: boolean;
  exclusive: boolean;
  arguments: Record<string, unknown>;
  policy: string | null;
  state: "running";
  messages_ready: number;
  messages_unacked: number;
  messages: number;
  consumers: number;
  bytes: number;
  rates: { publish: number; deliver: number; ack: number; redeliver: number };
  dead_letter_exchange: string | null;
  dead_lettered: number;
  is_dlq: boolean;
  first_offset?: number;
  next_offset?: number;
}

export interface Consumer {
  consumer_tag: string;
  channel: number;
  connection: number;
  ack_required: boolean;
  exclusive: boolean;
  priority: number;
  active: boolean;
  prefetch: number;
}

export interface Binding {
  source: string;
  destination: string;
  destination_type: "queue" | "exchange";
  routing_key: string;
  arguments: Record<string, unknown>;
}

export interface QueuePoint {
  t: number;
  publish: number;
  deliver: number;
  ack: number;
  ready: number;
  unacked: number;
}

/** `GET .../queues/{q}`: `consumers` is the count; `consumer_details` lists them (as in RabbitMQ). */
export type QueueDetail = Queue & { consumer_details: Consumer[]; bindings: Binding[] };

export interface Exchange {
  name: string;
  vhost: string;
  type: string;
  durable: boolean;
  auto_delete: boolean;
  internal: boolean;
  arguments: Record<string, unknown>;
  bindings: number;
  rates: { publish_in: number; publish_out: number };
}

export interface XDeath {
  queue: string;
  reason: string;
  count: number;
  exchange: string;
  routing_keys: string[];
}

export interface BrowsedMessage {
  seq: number;
  exchange: string;
  routing_key: string;
  redelivered: boolean;
  delivery_count: number;
  properties: Record<string, unknown>;
  payload: string;
  payload_encoding: "string" | "base64";
  payload_bytes: number;
  x_death?: XDeath[];
  /** When the broker received the message (not in the contract; rolld sends it). */
  received_ms?: number;
}

export interface Connection {
  id: number;
  name: string;
  protocol: string;
  vhost: string;
  user: string;
  peer: string;
  tls: boolean;
  channels: number;
  connected_at_ms: number;
  recv_oct: number;
  send_oct: number;
  client: string | null;
}

export interface Channel {
  id: number;
  connection: number;
  number: number;
  vhost: string;
  user: string;
  prefetch: number;
  unacked: number;
  consumers: number;
  confirm: boolean;
  transactional: boolean;
}

export interface Policy {
  vhost: string;
  name: string;
  pattern: string;
  apply_to: string;
  definition: Record<string, unknown>;
  priority: number;
  operator: boolean;
}

export interface Permission {
  vhost: string;
  configure: string;
  write: string;
  read: string;
}

export interface User {
  name: string;
  tags: string[];
  permissions: Permission[];
}

export interface Usage {
  period: { start: number; end: number; days: number };
  messages: { published: number; delivered: number; usd: number };
  storage: { current_bytes: number; gb_months: number; usd: number };
  connections: { hours: number; usd: number };
  total_usd: number;
  projected_month_usd: number;
  pricing: {
    usd_per_million_messages: number;
    usd_per_gb_month: number;
    usd_per_connection_hour: number;
    basis: string;
  };
  compare: { name: string; plan: string; monthly_usd: number; note: string }[];
  daily: {
    day: number;
    published: number;
    delivered: number;
    stored_bytes: number;
    connection_hours: number;
    usd: number;
  }[];
}

export interface Shovel {
  name: string;
  vhost?: string;
  /** "starting" | "running" | "terminated" (failed; it retries, see `reason`). */
  state: string;
  reason?: string | null;
  src: string;
  dest: string;
  transferred: number;
  /** Messages left in the source queue; null until the shovel has read it. */
  remaining: number | null;
}

export interface MigrateRequest {
  source: { management_url: string; username: string; password: string; vhost?: string; amqp_uri?: string };
  move_messages?: boolean;
  /** Also import the source's own shovels and federation upstreams (they start running here). */
  include_shovels?: boolean;
}

/** Something a definitions import or migration left out (or, with `partial`, changed). */
export interface SkippedItem {
  kind: string;
  vhost?: string;
  name: string;
  reason: string;
  partial?: boolean;
}

export interface MigrateResult {
  imported: { vhosts: number; exchanges: number; queues: number; bindings: number; policies: number; users: number };
  shovels: string[];
  skipped?: SkippedItem[];
}

export interface QueuesHealth {
  ok: boolean;
  version?: string;
  node?: string;
  endpoints?: Endpoints;
}

export interface PublishBody {
  exchange: string;
  routing_key: string;
  body: string;
  body_encoding?: "utf8" | "base64";
  properties?: Record<string, unknown>;
}

/** rolld also reports how many queues received the message. */
export interface PublishResult {
  routed: boolean | number;
  queues?: number;
}

export interface BindingBody {
  source: string;
  destination: string;
  destination_type: "queue" | "exchange";
  routing_key?: string;
  arguments?: Record<string, unknown>;
}

/** The API answered with an error (`{"error": "..."}`). */
export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

/** rolld could not be reached at all (not running, wrong port, or CORS). */
export class OfflineError extends Error {}

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  let res: Response;
  try {
    res = await fetch(`${ROLL_API}${path}`, {
      method,
      headers: body === undefined ? undefined : { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: "no-store",
    });
  } catch {
    throw new OfflineError(`Can't reach rolld at ${ROLL_API}`);
  }
  const text = await res.text();
  let data: unknown = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = text;
    }
  }
  if (!res.ok) {
    const msg =
      data && typeof data === "object" && "error" in data
        ? String((data as { error: unknown }).error)
        : typeof data === "string" && data.length < 300 && data.trim()
          ? data.trim()
          : `${res.status} ${res.statusText}`;
    throw new ApiError(msg, res.status);
  }
  return data as T;
}

const e = encodeURIComponent;
const vh = (vhost: string) => `/v1/queues/vhosts/${e(vhost)}`;

export const qapi = {
  health: () => call<QueuesHealth>("GET", "/v1/queues/health"),
  overview: () => call<Overview>("GET", "/v1/queues/overview"),
  rates: (seconds = 300) => call<Rates>("GET", `/v1/queues/rates?seconds=${seconds}`),
  usage: (days = 30) => call<Usage>("GET", `/v1/queues/usage?days=${days}`),

  vhosts: () => call<VHost[]>("GET", "/v1/queues/vhosts"),
  createVhost: (body: { name: string; description?: string; default_queue_type?: string; credentials?: boolean }) =>
    call<VHost>("POST", "/v1/queues/vhosts", body),
  /** Issues (or rotates) the vhost's own user. */
  vhostCredentials: (vhost: string) => call<VHostCredentials>("POST", `${vh(vhost)}/credentials`, {}),
  deleteVhost: (vhost: string) => call<null>("DELETE", vh(vhost)),
  suspendVhost: (vhost: string) => call<unknown>("POST", `${vh(vhost)}/suspend`),
  wakeVhost: (vhost: string) => call<unknown>("POST", `${vh(vhost)}/wake`),

  queues: (vhost: string) => call<Queue[]>("GET", `${vh(vhost)}/queues`),
  declareQueue: (
    vhost: string,
    body: { name: string; type?: QueueType; durable?: boolean; auto_delete?: boolean; arguments?: Record<string, unknown> },
  ) => call<Queue>("POST", `${vh(vhost)}/queues`, body),
  queue: async (vhost: string, name: string) =>
    call<QueueDetail>("GET", `${vh(vhost)}/queues/${e(name)}`),
  deleteQueue: (vhost: string, name: string) => call<null>("DELETE", `${vh(vhost)}/queues/${e(name)}`),
  /** The last few minutes of a queue's depth and rates (empty while it has been idle). */
  queueRates: (vhost: string, name: string, seconds = 300) =>
    call<{ step_s: number; points: QueuePoint[] }>("GET", `${vh(vhost)}/queues/${e(name)}/rates?seconds=${seconds}`),
  purge: (vhost: string, name: string) => call<{ purged: number }>("POST", `${vh(vhost)}/queues/${e(name)}/purge`),
  messages: (vhost: string, name: string, offset = 0, count = 50) =>
    call<BrowsedMessage[]>("GET", `${vh(vhost)}/queues/${e(name)}/messages?offset=${offset}&count=${count}`),
  replay: (
    vhost: string,
    name: string,
    body: { count?: number; target_exchange?: string; target_routing_key?: string },
  ) => call<{ replayed: number; skipped?: number }>("POST", `${vh(vhost)}/queues/${e(name)}/replay`, body),

  exchanges: (vhost: string) => call<Exchange[]>("GET", `${vh(vhost)}/exchanges`),
  declareExchange: (
    vhost: string,
    body: {
      name: string;
      type: string;
      durable?: boolean;
      auto_delete?: boolean;
      internal?: boolean;
      arguments?: Record<string, unknown>;
    },
  ) => call<Exchange>("POST", `${vh(vhost)}/exchanges`, body),
  deleteExchange: (vhost: string, name: string) => call<null>("DELETE", `${vh(vhost)}/exchanges/${e(name)}`),
  bindings: (vhost: string) => call<Binding[]>("GET", `${vh(vhost)}/bindings`),
  bind: (vhost: string, body: BindingBody) => call<unknown>("POST", `${vh(vhost)}/bindings`, body),
  unbind: (vhost: string, body: BindingBody) => call<unknown>("DELETE", `${vh(vhost)}/bindings`, body),
  publish: (vhost: string, body: PublishBody) => call<PublishResult>("POST", `${vh(vhost)}/publish`, body),

  connections: () => call<Connection[]>("GET", "/v1/queues/connections"),
  closeConnection: (id: number) => call<null>("DELETE", `/v1/queues/connections/${id}`),
  channels: () => call<Channel[]>("GET", "/v1/queues/channels"),

  policies: () => call<Policy[]>("GET", "/v1/queues/policies"),
  putPolicy: (
    vhost: string,
    name: string,
    body: { pattern: string; apply_to: string; definition: Record<string, unknown>; priority: number },
  ) => call<unknown>("PUT", `/v1/queues/policies/${e(vhost)}/${e(name)}`, body),
  deletePolicy: (vhost: string, name: string) => call<null>("DELETE", `/v1/queues/policies/${e(vhost)}/${e(name)}`),

  users: () => call<User[]>("GET", "/v1/queues/users"),
  putUser: (name: string, body: { password?: string; tags?: string[] }) =>
    call<unknown>("PUT", `/v1/queues/users/${e(name)}`, body),
  deleteUser: (name: string) => call<null>("DELETE", `/v1/queues/users/${e(name)}`),
  putPermissions: (vhost: string, user: string, body: { configure: string; write: string; read: string }) =>
    call<unknown>("PUT", `/v1/queues/permissions/${e(vhost)}/${e(user)}`, body),

  definitions: () => call<unknown>("GET", "/v1/queues/definitions"),
  importDefinitions: (defs: unknown) =>
    call<{ ok: boolean; skipped?: SkippedItem[] }>("POST", "/v1/queues/definitions", defs),
  migrate: (body: MigrateRequest) => call<MigrateResult>("POST", "/v1/queues/migrate", body),
  migration: () => call<{ shovels: Shovel[] }>("GET", "/v1/queues/migrate"),
  stopMigration: () => call<{ stopped: string[] }>("DELETE", "/v1/queues/migrate"),
};

/* ---------- formatting ---------- */

/** 1234 → "1,234"; large numbers compact ("1.2M"). */
export function formatNum(n: number | null | undefined) {
  if (n == null || !Number.isFinite(n)) return "—";
  const a = Math.abs(n);
  if (a >= 1e9) return `${(n / 1e9).toFixed(a >= 1e10 ? 0 : 1)}B`;
  if (a >= 1e6) return `${(n / 1e6).toFixed(a >= 1e7 ? 0 : 1)}M`;
  if (a >= 1e5) return `${Math.round(n / 1e3)}k`;
  if (Number.isInteger(n)) return n.toLocaleString("en-US");
  return n.toFixed(a < 10 ? 2 : 1);
}

/** Messages per second. */
export function formatRate(n: number | null | undefined) {
  if (n == null || !Number.isFinite(n)) return "—";
  if (n === 0) return "0/s";
  if (n < 0.1) return `${n.toFixed(2)}/s`;
  if (n < 10) return `${n.toFixed(1)}/s`;
  return `${formatNum(Math.round(n))}/s`;
}

export function formatDuration(s: number) {
  if (s < 60) return `${Math.floor(s)}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

export function ago(unixS: number) {
  const s = Math.max(0, Math.floor(Date.now() / 1000 - unixS));
  if (s < 60) return `${s}s ago`;
  return `${formatDuration(s)} ago`;
}

/** Display name for an exchange; the default exchange has the empty name. */
export function exchangeLabel(name: string) {
  return name === "" ? "(AMQP default)" : name;
}

/** Dashboard URL of a queue's detail page. */
export function queueHref(vhost: string, name: string) {
  return `/dashboard/queues/queues/${e(vhost)}/${e(name)}`;
}

/** Route params may or may not arrive percent-decoded; decode once if they still look encoded. */
export function decodeParam(v: string | string[] | undefined) {
  const s = Array.isArray(v) ? v.join("/") : (v ?? "");
  if (!/%[0-9a-f]{2}/i.test(s)) return s;
  try {
    return decodeURIComponent(s);
  } catch {
    return s;
  }
}

export function errorMessage(err: unknown) {
  return err instanceof Error ? err.message : String(err);
}
