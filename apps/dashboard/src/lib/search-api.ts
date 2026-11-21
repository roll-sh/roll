/**
 * roll search: the control-plane API (`/v1/search/...`, crates/search-control/src/api.rs).
 * Clusters speak the Elasticsearch REST API (or OpenSearch's) on their own port; the dashboard
 * reaches them only through the control plane, so it never holds a password: an index's
 * requests go through its per-index routes (what a user granted only some indices may call),
 * everything else through the request console.
 */

import { apiHeaders, call, onApiStatus, ROLL_API } from "./roll-api";
import { withSession } from "./code-token";
import { analyticsQuery, type AnalyticsRange, type SearchAnalytics } from "./search-analytics";
import type { SuggestionsStatus } from "./search-suggestions";
import type { MetricsRange, SearchMetrics, SearchSlowLog } from "./search-performance";
import type { AuditPage } from "./search-audit";
import type { LinkBody, RemoteLink } from "./search-remotes";

export type Flavor = "elasticsearch" | "opensearch";

/** The prefix of a cluster's environment variables, as its flavor's clients read them
 * (`ELASTICSEARCH_URL`, `OPENSEARCH_PASSWORD`; roll apps' links name them the same). */
export function envPrefix(flavor: Flavor) {
  return flavor === "opensearch" ? "OPENSEARCH" : "ELASTICSEARCH";
}

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
  history_retention_s: number;
  /** How long a deleted cluster can be restored (`--search-deleted-retention-days`; 0: never). */
  deleted_retention_s: number;
  defaults: SearchDefaults;
  pricing: SearchPricing;
  /** A signed-in person's (not an admin's): how many clusters, and always-on ones, they may own
   * (0: no cap). Past one, a create, branch or switch to always-on answers 409 saying so. */
  caps?: { clusters: number; always_on: number };
  /** The operator's (and admins') only: where the node keeps its data, and its node-wide figures. */
  storage?: string;
  loaded?: number;
  cache?: { used_bytes: number; budget_bytes: number };
}

/** Loaded in memory, or asleep (scaled to zero; wakes on the next request). */
export interface SearchStatus {
  state: "loaded" | "asleep";
  in_flight?: number;
  indices?: number;
  /** Loaded with every index open: live documents (the cluster's `docs` is the last metered). */
  docs?: number;
  cluster_uuid?: string;
  /** Asleep: when a background job (a watch, a transform, ILM, a rollup) wakes it (unix s). */
  wakes_at?: number;
}

export interface SearchSettings {
  flavor: Flavor;
  idle_timeout_s: number;
  max_in_flight: number;
  max_body_mb: number;
  destructive_requires_name: boolean;
}

/** One entry of a cluster's IP allowlist (roll apps' `allow_ips` have the same shape). */
export interface IpRule {
  /** Canonical: `203.0.113.7/32`, `10.0.0.0/8`, `2001:db8::/32`. */
  cidr: string;
  description?: string;
}

/** What `PATCH /v1/search/clusters/{id}` takes. */
export type SearchClusterPatch = Partial<SearchSettings> & {
  /** Only these networks reach the cluster's endpoint ([]: anyone). */
  ip_allowlist?: (string | IpRule)[];
};

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
  /** Of `stored_bytes`, what a branch references that its parent's latest commit points don't. */
  own_bytes: number;
  /** The signed-in person (`usr_…`) whose cluster it is; null for the operator's and admins'. */
  owner?: string | null;
  /** Only these networks reach its endpoint ([]: anyone). The dashboard and API aren't filtered. */
  ip_allowlist: IpRule[];
  url: string;
  host: string;
  port: number;
  username: string;
  status: SearchStatus;
  branches: BranchRef[];
  /** Only in the responses that create the cluster or rotate its password. */
  password?: string;
}

/** A deleted cluster kept for restoring (`GET /v1/search/clusters?deleted=true`). */
export interface DeletedSearchCluster extends SearchSettings {
  id: string;
  /** The name it had (it restores under it unless another cluster took it). */
  name: string;
  region: string;
  parent_id: string | null;
  created_at: number;
  indices: number;
  docs: number;
  /** Still billed as storage until it is purged. */
  stored_bytes: number;
  owner?: string | null;
  /** Unix seconds. */
  deleted_at: number;
  /** Unix seconds: when its storage is removed for good. */
  purge_at: number;
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

/** What an API key may do: `data` is an app's role (every index, templates, pipelines and
 * lifecycle policies, no security); `admin` is everything the cluster's own user may do. */
export type KeyAccess = "data" | "admin";

/** A new API key (`POST …/keys`), shown once: `api_key` for `Authorization: ApiKey …`, or on an
 * OpenSearch-flavor cluster an internal user's `username` and `password`. */
export interface NewSearchKey {
  id: string;
  name: string;
  api_key?: string;
  username?: string;
  password?: string;
  url: string;
  expires_at?: number;
}

/** A key as the cluster lists it (`GET /_security/api_key`). */
export interface SearchKeyInfo {
  id: string;
  name: string | null;
  creation: number;
  expiration?: number;
  invalidated: boolean;
  role_descriptors?: Record<string, unknown>;
}

/** The role `access: "data"` keys carry (search-control's `link_role`). */
export const DATA_ROLE = "roll_link";

const cl = (id: string) => `/v1/search/clusters/${encodeURIComponent(id)}`;

export const searchApi = {
  info: () => call<SearchInfo>("GET", "/v1/search"),
  list: () => call<SearchCluster[]>("GET", "/v1/search/clusters"),
  get: (id: string) => call<SearchCluster>("GET", cl(id)),
  create: (name: string, settings: Partial<SearchSettings>) =>
    call<SearchCluster>("POST", "/v1/search/clusters", { name, ...settings }),
  update: (id: string, settings: SearchClusterPatch) => call<SearchCluster>("PATCH", cl(id), settings),
  remove: (id: string) => call<{ deleted: string; name: string }>("DELETE", cl(id)),
  deleted: () => call<DeletedSearchCluster[]>("GET", "/v1/search/clusters?deleted=true"),
  /** Without `name` it comes back under its own (409 when another cluster took it). */
  restore: (id: string, name?: string) => call<SearchCluster>("POST", `${cl(id)}/restore`, name ? { name } : {}),
  branch: (id: string, name: string, at?: number) => call<SearchCluster>("POST", `${cl(id)}/branches`, { name, at }),
  rotatePassword: (id: string) => call<SearchCluster>("POST", `${cl(id)}/password`, {}),
  unload: (id: string) => call<{ unloaded: boolean }>("POST", `${cl(id)}/unload`, {}),
  indices: (id: string) => call<SearchIndex[]>("GET", `${cl(id)}/indices`),
  /** A string body is sent as is (the control plane passes it through byte for byte). */
  request: (id: string, method: string, path: string, body?: unknown) =>
    call<ConsoleResponse>("POST", `${cl(id)}/request`, { method, path, body }),
  usage: (id: string, days = 30) => call<SearchUsage>("GET", `${cl(id)}/usage?days=${days}`),
  metrics: (id: string, range: MetricsRange) => call<SearchMetrics>("GET", `${cl(id)}/metrics?range=${range}`),
  slowlog: (id: string) => call<SearchSlowLog>("GET", `${cl(id)}/slowlog`),
  analytics: (id: string, index: string | null, range: AnalyticsRange) => call<SearchAnalytics>("GET", `${cl(id)}/analytics?${analyticsQuery(index, range)}`),
  /** An index's query suggestions: settings, last build, heaviest suggestions (never wakes the cluster). */
  suggestions: (id: string, index: string) => call<SuggestionsStatus>("GET", `${cl(id)}/indices/${encodeURIComponent(index)}/suggestions`),
  rebuildSuggestions: (id: string, index: string) => call<SuggestionsStatus>("POST", `${cl(id)}/indices/${encodeURIComponent(index)}/suggestions/rebuild`, {}),
  createKey: (id: string, name: string, access: KeyAccess, expiresInS?: number) =>
    call<NewSearchKey>("POST", `${cl(id)}/keys`, { name, access, expires_in_s: expiresInS }),
  revokeKey: (id: string, key: string) => call<null>("DELETE", `${cl(id)}/keys/${encodeURIComponent(key)}`),
  /** The remote clusters roll holds credentials for (never wakes the cluster, never the key). */
  remotes: (id: string) => call<{ remotes: RemoteLink[] }>("GET", `${cl(id)}/remotes`),
  /** Links another of the person's clusters as remote `alias` (roll makes its cross-cluster key). */
  linkRemote: (id: string, alias: string, body: LinkBody) => call<unknown>("PUT", `${cl(id)}/remotes/${encodeURIComponent(alias)}`, body),
  /** Unlinks it: its settings and credentials go, a key roll made is revoked. */
  unlinkRemote: (id: string, alias: string) => call<null>("DELETE", `${cl(id)}/remotes/${encodeURIComponent(alias)}`),
  /** The endpoints whose key comes from a roll secret, and the projects whose secrets may key one. */
  inferenceSecrets: (id: string) => call<InferenceSecrets>("GET", `${cl(id)}/inference/secrets`),
  /** An endpoint keyed by a roll secret: `config` is the `_inference` body without `api_key`. */
  createInferenceFromSecret: (id: string, e: NewInferenceFromSecret) =>
    call<{ endpoint: unknown; binding: InferenceBinding }>("POST", `${cl(id)}/inference/secrets`, e),
  /** The cluster's security audit log, newest first (never wakes it). `from`/`to`: Unix ms. */
  audit: (id: string, q: { from?: number; to?: number; event?: string; size?: number } = {}) => {
    const params = new URLSearchParams();
    for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== "") params.set(k, String(v));
    const qs = params.toString();
    return call<AuditPage>("GET", `${cl(id)}/audit${qs ? `?${qs}` : ""}`);
  },
};

/** An inference endpoint whose key follows a roll secret (a `search-inference` sync). */
export interface InferenceBinding {
  endpoint: string;
  project: string;
  environment: string;
  path: string;
  key: string;
  /** The sync (roll secrets' `…/syncs/{id}`: run it to push the key now, delete it to stop). */
  sync: string;
  status: "pending" | "ok" | "error";
  error: string | null;
  last_synced_at: number | null;
  /** When the sync was made (Unix ms): branches copied since hold the key, and no sync follows them. */
  bound_at: number;
}

export interface InferenceSecrets {
  /** The projects whose canvas holds the cluster (and stands for it) that the caller may read. */
  projects: { id: string; name: string }[];
  bindings: InferenceBinding[];
}

export interface NewInferenceFromSecret {
  inference_id: string;
  task_type?: string;
  config: Record<string, unknown>;
  secret: { project?: string; environment: string; path?: string; key: string };
}

/**
 * An Elasticsearch error body's reason (`error.reason`, or the first root cause's), followed by
 * the innermost `caused_by` reason when the outer one doesn't already say it: a parse failure's
 * outer reason is a generic wrapper ("Failed to build [query_rule] …") and the cause is the news.
 */
export function esReason(body: unknown, status: number): string {
  const e = (body as { error?: unknown } | null)?.error;
  if (typeof e === "string") return e;
  if (e && typeof e === "object") {
    type Cause = { type?: string; reason?: string; caused_by?: Cause };
    const o = e as Cause & { root_cause?: { reason?: string }[] };
    const reason = o.reason ?? o.root_cause?.[0]?.reason;
    let inner: Cause | undefined = o.caused_by;
    for (let i = 0; inner?.caused_by && i < 16; i++) inner = inner.caused_by;
    const cause = inner?.reason && reason && !reason.includes(inner.reason) ? `: ${inner.reason}` : "";
    if (reason) return (o.type ? `${o.type}: ${reason}` : reason) + cause;
  }
  return `HTTP ${status}`;
}

/**
 * One REST request through the console: the response parsed as JSON, and its text (for what
 * must stay exact: `_source`, numbers beyond 2^53). Throws Elasticsearch's reason on 4xx/5xx.
 * A string body is sent byte for byte; anything else is encoded with `JSON.stringify`.
 */
export async function esRaw<T = unknown>(id: string, method: Method, path: string, body?: unknown): Promise<{ value: T; text: string }> {
  const route = perIndexRoute(method, path);
  const r = route ? await perIndexCall(id, route, body) : await searchApi.request(id, method, path, body);
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

/**
 * The per-index control-plane route an Elasticsearch request maps onto, or `null` when it has
 * none (cluster APIs, which take the console). `path` is as the console takes it: an encoded
 * index first, then the API, then the query string (passed through).
 */
export function perIndexRoute(method: string, path: string): { method: string; path: string } | null {
  const [pathname, query] = path.split("?", 2);
  const seg = pathname.split("/").filter(Boolean);
  const index = seg[0];
  if (!index || index.startsWith("_")) return null;
  const q = query ? `?${query}` : "";
  const at = (rest: string, m = method) => ({ method: m, path: `/indices/${index}${rest}${q}` });
  const [api, id, extra] = [seg[1], seg[2], seg[3]];
  if (extra !== undefined) return null;
  if (api === undefined) return ["GET", "PUT", "DELETE"].includes(method) ? at("") : null;
  if (id !== undefined) return api === "_doc" && ["GET", "PUT", "DELETE"].includes(method) ? at(`/docs/${id}`) : null;
  switch (api) {
    case "_doc":
      return method === "POST" ? at("/docs") : null;
    case "_mapping":
      return method === "GET" || method === "PUT" ? at("/mapping") : null;
    case "_settings":
      return method === "GET" ? at("/settings") : null;
    case "_refresh":
      return method === "POST" ? at("/refresh") : null;
    case "_analyze":
      return method === "GET" || method === "POST" ? at("/analyze", "POST") : null;
    case "_search":
      return method === "GET" || method === "POST" ? at("/search", "POST") : null;
    case "_bulk":
      return method === "POST" ? at("/bulk") : null;
    case "_delete_by_query":
      return method === "POST" ? at("/delete_by_query") : null;
    default:
      return null;
  }
}

/** A per-index route's answer as Elasticsearch gave it: status and exact text. */
async function perIndexCall(id: string, route: { method: string; path: string }, body?: unknown): Promise<{ status: number; body: string }> {
  const text = body === undefined ? undefined : typeof body === "string" ? body : JSON.stringify(body);
  const ndjson = route.path.includes("/bulk");
  const headers = { ...(apiHeaders(text) ?? {}), ...(text !== undefined ? { "content-type": ndjson ? "application/x-ndjson" : "application/json" } : {}) };
  const res = await fetch(`${ROLL_API}/v1/search/clusters/${encodeURIComponent(id)}${route.path}`, withSession({ method: route.method, headers, body: text, cache: "no-store" }));
  onApiStatus(res.status);
  return { status: res.status, body: await res.text() };
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

/** A window such as the deleted-cluster retention: whole days in words ("7 days"). */
export function formatWindow(s: number) {
  if (s > 0 && s % 86_400 === 0) {
    const d = s / 86_400;
    return `${d} day${d === 1 ? "" : "s"}`;
  }
  return formatDuration(s);
}

/** The URL with credentials embedded (`http://user:password@host:port`). */
export function urlWithPassword(url: string, user: string, password: string) {
  const i = url.indexOf("://");
  if (i < 0) return url;
  return `${url.slice(0, i + 3)}${encodeURIComponent(user)}:${encodeURIComponent(password)}@${url.slice(i + 3)}`;
}
