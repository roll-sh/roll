/**
 * roll search: the control-plane API (`/v1/search/...`, crates/search-control/src/api.rs).
 * Clusters speak the Elasticsearch REST API (or OpenSearch's) on their own port; the dashboard
 * reaches them only through the control plane's request console, so it never holds a password.
 */

import { ROLL_API } from "./roll-api";

/**
 * The search control plane: rolld's API by default, or a fleet's (the `ApiUrl` output of
 * deploy/search/aws/roll-search.yaml) with `NEXT_PUBLIC_ROLL_SEARCH_API`. A fleet's API wants
 * its token (`ApiTokenSecret`) as `NEXT_PUBLIC_ROLL_SEARCH_TOKEN`; it ends up in the browser
 * bundle, so only build a dashboard with it for people who may hold that token.
 */
export const SEARCH_API = (process.env.NEXT_PUBLIC_ROLL_SEARCH_API || ROLL_API).replace(/\/+$/, "");
const SEARCH_TOKEN = process.env.NEXT_PUBLIC_ROLL_SEARCH_TOKEN || undefined;

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers["content-type"] = "application/json";
  if (SEARCH_TOKEN) headers.authorization = `Bearer ${SEARCH_TOKEN}`;
  const res = await fetch(`${SEARCH_API}${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store",
  });
  const text = await res.text();
  let data: { error?: string } | null = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    // A proxy or load balancer answering with text: report it as it is.
    if (!res.ok) throw new Error(`${res.status} ${text.slice(0, 200)}`);
    throw new Error(`unexpected response from ${SEARCH_API}: ${text.slice(0, 200)}`);
  }
  if (!res.ok) throw new Error(data?.error ?? `${res.status} ${res.statusText}`);
  return data as T;
}

export type Flavor = "elasticsearch" | "opensearch";

export interface SearchDefaults {
  flavor: Flavor;
  idle_timeout_s: number;
  max_in_flight: number;
  max_body_mb: number;
  destructive_requires_name: boolean;
}

export interface SearchPricing {
  usd_per_gib_hour: number;
  usd_per_million_requests: number;
  usd_per_gb_month_s3: number;
  elastic_cloud_usd_per_month: number;
  elastic_cloud_gb_per_block: number;
  opensearch_instance_usd_per_hour: number;
  opensearch_gp3_usd_per_gb_month: number;
  opensearch_min_storage_gb: number;
  opensearch_serverless_usd_per_ocu_hour: number;
  opensearch_serverless_min_ocu: number;
  opensearch_serverless_usd_per_gb_month: number;
}

export interface SearchInfo {
  version: string;
  opensearch_version: string;
  lucene_version: string;
  region: string;
  /** host:port of the REST listener. */
  http: string;
  tls: boolean;
  domain: string | null;
  storage: string;
  history_retention_s: number;
  defaults: SearchDefaults;
  pricing: SearchPricing;
  loaded: number;
  cache: { used_bytes: number; budget_bytes: number };
}

/** Loaded in memory, or asleep (scaled to zero; wakes on the next request). */
export interface SearchStatus {
  state: "loaded" | "asleep";
  in_flight?: number;
  indices?: number;
  cluster_uuid?: string;
}

export interface SearchSettings {
  flavor: Flavor;
  idle_timeout_s: number;
  max_in_flight: number;
  max_body_mb: number;
  destructive_requires_name: boolean;
}

export interface BranchRef {
  id: string;
  name: string;
  parent_at: number | null;
  created_at: number;
}

export interface SearchCluster extends SearchSettings {
  id: string;
  name: string;
  region: string;
  parent_id: string | null;
  parent_name: string | null;
  parent_at: number | null;
  created_at: number;
  password_rotated_at: number | null;
  /** Last figures metering saw (refreshed while loaded and after each unload). */
  indices: number;
  docs: number;
  stored_bytes: number;
  url: string;
  host: string;
  port: number;
  username: string;
  status: SearchStatus;
  branches: BranchRef[];
  /** Only in the responses that create the cluster or rotate its password. */
  password?: string;
}

export interface SearchIndex {
  index: string;
  uuid: string;
  health: "green" | "yellow" | "red" | string;
  status: "open" | "close" | string;
  primaries: number | null;
  replicas: number | null;
  docs: number | null;
  deleted_docs: number | null;
  size_bytes: number | null;
  primary_size_bytes: number | null;
}

export interface ConsoleResponse {
  status: number;
  headers: [string, string][];
  /** The body as the cluster sent it (so JSON keeps Elasticsearch's key order); empty for HEAD. */
  body: string;
  /** The body is JSON. */
  json: boolean;
  took_ms: number;
  truncated: boolean;
}

export interface SearchUsage {
  cluster_id: string;
  days: number;
  hours: number;
  loaded_seconds: number;
  loaded_gib_hours: number;
  requests: number;
  peak_loaded_bytes: number;
  stored_gib_months: number;
  stored_bytes: number;
  cost: {
    memory_usd: number;
    requests_usd: number;
    storage_usd: number;
    total_usd: number;
    elastic_cloud_usd: number;
    opensearch_service_usd: number;
    opensearch_serverless_usd: number;
  };
}

const cl = (id: string) => `/v1/search/clusters/${encodeURIComponent(id)}`;

export const searchApi = {
  info: () => call<SearchInfo>("GET", "/v1/search"),
  list: () => call<SearchCluster[]>("GET", "/v1/search/clusters"),
  get: (id: string) => call<SearchCluster>("GET", cl(id)),
  create: (name: string, settings: Partial<SearchSettings>) =>
    call<SearchCluster>("POST", "/v1/search/clusters", { name, ...settings }),
  update: (id: string, settings: Partial<SearchSettings>) => call<SearchCluster>("PATCH", cl(id), settings),
  remove: (id: string) => call<{ deleted: string; name: string }>("DELETE", cl(id)),
  branch: (id: string, name: string, at?: number) => call<SearchCluster>("POST", `${cl(id)}/branches`, { name, at }),
  rotatePassword: (id: string) => call<SearchCluster>("POST", `${cl(id)}/password`, {}),
  unload: (id: string) => call<{ unloaded: boolean }>("POST", `${cl(id)}/unload`, {}),
  indices: (id: string) => call<SearchIndex[]>("GET", `${cl(id)}/indices`),
  /** A string body is sent as is (the control plane passes it through byte for byte). */
  request: (id: string, method: string, path: string, body?: unknown) =>
    call<ConsoleResponse>("POST", `${cl(id)}/request`, { method, path, body }),
  usage: (id: string, days = 30) => call<SearchUsage>("GET", `${cl(id)}/usage?days=${days}`),
};

/** An Elasticsearch error body's reason (`error.reason`, or the first root cause's). */
export function esReason(body: unknown, status: number): string {
  const e = (body as { error?: unknown } | null)?.error;
  if (typeof e === "string") return e;
  if (e && typeof e === "object") {
    const o = e as { type?: string; reason?: string; root_cause?: { reason?: string }[] };
    const reason = o.reason ?? o.root_cause?.[0]?.reason;
    if (reason) return o.type ? `${o.type}: ${reason}` : reason;
  }
  return `HTTP ${status}`;
}

/**
 * One REST request through the console: the response parsed as JSON, and its text (for what
 * must stay exact: `_source`, numbers beyond 2^53). Throws Elasticsearch's reason on 4xx/5xx.
 * A string body is sent byte for byte; anything else is encoded with `JSON.stringify`.
 */
export async function esRaw<T = unknown>(id: string, method: Method, path: string, body?: unknown): Promise<{ value: T; text: string }> {
  const r = await searchApi.request(id, method, path, body);
  let parsed: unknown = null;
  if (r.body) {
    try {
      parsed = JSON.parse(r.body);
    } catch {
      parsed = r.body;
    }
  }
  if (r.status >= 400) throw new Error(esReason(parsed, r.status));
  return { value: parsed as T, text: r.body };
}

/** [`esRaw`]'s parsed response. */
export async function esJson<T = unknown>(id: string, method: Method, path: string, body?: unknown): Promise<T> {
  return (await esRaw<T>(id, method, path, body)).value;
}

/** An index name as a path segment. */
export const indexPath = (index: string) => `/${encodeURIComponent(index)}`;

/** Same rule as the control plane: a DNS label (it is the Basic auth user and the host label). */
export const NAME_RE = /^[a-z](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
export const NAME_HINT = "1–63 lowercase letters, digits or -, starting with a letter.";

export const IDLE_OPTIONS = [
  { label: "1 min", value: 60 },
  { label: "5 min", value: 300 },
  { label: "15 min", value: 900 },
  { label: "1 hour", value: 3600 },
  { label: "Always loaded", value: 0 },
];

export const IN_FLIGHT_STEPS = [4, 8, 16, 32, 64, 128, 256, 512];
export const BODY_MB_STEPS = [1, 10, 50, 100, 200, 500, 1024];

export const METHODS = ["GET", "POST", "PUT", "DELETE", "HEAD"] as const;
export type Method = (typeof METHODS)[number];

export { parseConsole, type ConsoleInput } from "./search-console";

export function formatCount(n: number | null | undefined) {
  if (n == null) return "—";
  if (n < 10_000) return n.toLocaleString("en-US");
  if (n < 1e6) return `${(n / 1e3).toFixed(1)}k`;
  if (n < 1e9) return `${(n / 1e6).toFixed(n < 1e7 ? 2 : 1)}M`;
  return `${(n / 1e9).toFixed(2)}B`;
}

export function formatDuration(s: number) {
  if (s === 0) return "0s";
  if (s % 86_400 === 0) return `${s / 86_400}d`;
  if (s % 3600 === 0) return `${s / 3600}h`;
  if (s % 60 === 0) return `${s / 60}m`;
  return `${s}s`;
}

/** The URL with credentials embedded (`http://user:password@host:port`). */
export function urlWithPassword(url: string, user: string, password: string) {
  const i = url.indexOf("://");
  if (i < 0) return url;
  return `${url.slice(0, i + 3)}${encodeURIComponent(user)}:${encodeURIComponent(password)}@${url.slice(i + 3)}`;
}
