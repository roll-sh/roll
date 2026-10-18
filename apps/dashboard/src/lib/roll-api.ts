/** Client for the rolld control-plane API (Postgres: crates/pg-control/src/api.rs; Redis: redis-api.ts). */

export const ROLL_API = process.env.NEXT_PUBLIC_ROLL_API ?? "http://127.0.0.1:7070";

export type EndpointState = "starting" | "running" | "suspended";

export interface Endpoint {
  id: string;
  project_id: string;
  branch_id: string;
  kind: string;
  state: EndpointState;
  min_cu: number;
  max_cu: number;
  cu: number;
  suspend_timeout_s: number;
  host_port: number | null;
  last_active: number;
  created_at: number;
  connections: number;
}

export interface Branch {
  id: string;
  project_id: string;
  name: string;
  parent_id: string | null;
  created_at: number;
  endpoint: Endpoint | null;
  connection_uri: string | null;
  storage_bytes: number | null;
}

export interface Project {
  id: string;
  name: string;
  pg_version: number;
  region: string;
  created_at: number;
  branches: Branch[];
}

export interface Usage {
  period: { start: number; end: number; days: number };
  compute: { cu_hours: number; usd: number; active_hours: number };
  storage: { current_bytes: number; gib_months: number; usd: number };
  total_usd: number;
  projected_month_usd: number;
  always_on_month_usd: number;
  pricing: { usd_per_cu_hour: number; usd_per_gb_month: number };
  daily: { day: number; cu_hours: number }[];
}

export interface Health {
  ok: boolean;
  version: string;
  pg_versions: number[];
  default_pg_version: number;
  host_cpus: number;
  proxy: string;
}

export interface QueryResult {
  columns: string[];
  rows: (string | null)[][];
  affected: number;
}

export async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`${ROLL_API}${path}`, {
    method,
    headers: body === undefined ? undefined : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store",
  });
  const text = await res.text();
  const data = text ? JSON.parse(text) : null;
  if (!res.ok) throw new Error(data?.error ?? `${res.status} ${res.statusText}`);
  return data as T;
}

export const api = {
  health: () => call<Health>("GET", "/v1/health"),
  projects: () => call<Project[]>("GET", "/v1/projects"),
  createProject: (name: string, pg_version?: number) =>
    call<Project & { password: string }>("POST", "/v1/projects", { name, pg_version }),
  deleteProject: (id: string) => call<null>("DELETE", `/v1/projects/${id}`),
  createBranch: (project: string, name: string, parent?: string) =>
    call<{ took_ms: number }>("POST", `/v1/projects/${project}/branches`, { name, parent }),
  deleteBranch: (project: string, branch: string) => call<null>("DELETE", `/v1/projects/${project}/branches/${branch}`),
  start: (endpoint: string) => call<{ took_ms: number }>("POST", `/v1/endpoints/${endpoint}/start`),
  suspend: (endpoint: string) => call<Endpoint>("POST", `/v1/endpoints/${endpoint}/suspend`),
  updateEndpoint: (endpoint: string, body: { min_cu?: number; max_cu?: number; suspend_timeout_s?: number }) =>
    call<Endpoint>("PATCH", `/v1/endpoints/${endpoint}`, body),
  usage: (project: string, days = 30) => call<Usage>("GET", `/v1/projects/${project}/usage?days=${days}`),
  query: (endpoint: string, sql: string) =>
    call<{ results: QueryResult[]; took_ms: number }>("POST", `/v1/endpoints/${endpoint}/query`, { sql }),
};

export const CU_STEPS = [0.25, 0.5, 1, 2, 3, 4, 6, 8, 16, 32];

export function formatBytes(n: number | null | undefined) {
  if (n == null) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(n < 10 && i > 0 ? 1 : 0)} ${units[i]}`;
}

export function formatUsd(n: number) {
  if (n === 0) return "$0";
  if (n < 0.01) return `$${n.toFixed(4)}`;
  return `$${n.toFixed(2)}`;
}

/** Hide the password in a connection string. */
export function maskUri(uri: string) {
  return uri.replace(/(postgres:\/\/[^:]+:)[^@]+@/, "$1••••••••@");
}
