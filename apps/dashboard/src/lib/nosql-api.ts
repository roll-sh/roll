/**
 * roll nosql (ScyllaDB / Cassandra compatible): the control-plane API (`/v1/nosql/...`,
 * crates/nosql-control/src/api.rs, docs/nosql/contracts.md) and the pure helpers the dashboard
 * pages share (formatting, connection snippets, the branch tree).
 */

import { call } from "./roll-api";

export type Dialect = "scylla" | "cassandra";

export interface NosqlEndpoints {
  /** host:port of the CQL listener. */
  cql: string;
  /** host:port of ScyllaDB's shard-aware port. */
  cql_shard_aware: string | null;
  /** http://host:port of the DynamoDB-compatible API (absent until Alternator ships). */
  alternator: string | null;
  /** http://host:port of ScyllaDB's REST API (`nodetool`). */
  rest: string | null;
}

export interface NosqlDefaults {
  dialect: string;
  datacenter: string;
  rack: string;
  superuser: string;
  durability: string;
  idle_timeout: number;
  history_retention: number;
}

export interface NosqlPricing {
  usd_per_active_hour: number;
  usd_per_million_reads: number;
  usd_per_million_writes: number;
  usd_per_gb_month_s3: number;
  usd_per_gb_out: number;
  keyspaces_usd_per_million_writes: number;
  keyspaces_usd_per_million_reads: number;
  keyspaces_usd_per_gb_month: number;
  dynamodb_usd_per_million_writes: number;
  dynamodb_usd_per_million_reads: number;
  dynamodb_usd_per_gb_month: number;
  astra_usd_per_million_writes: number;
  astra_usd_per_million_reads: number;
  astra_usd_per_gb_month: number;
  scylla_cloud_usd_per_node_hour: number;
  scylla_cloud_min_nodes: number;
}

/** `GET /v1/nosql`. */
export interface NosqlInfo {
  version: { scylla: string; cassandra: string };
  dialects: Dialect[];
  endpoints: NosqlEndpoints;
  tls: boolean;
  domain: string | null;
  /** The single-mode database's id. */
  single: string | null;
  storage: string;
  defaults: NosqlDefaults;
  /** Accepted `settings` keys and each dialect's effective defaults. */
  settings: { keys: string[]; scylla: Record<string, unknown>; cassandra: Record<string, unknown> };
  pricing: NosqlPricing;
  /** Databases loaded right now. */
  loaded: number;
}

export interface NosqlMetrics {
  connections: number;
  connections_total: number;
  connection_ms: number;
  requests: number;
  errors: number;
  bytes_in: number;
  bytes_out: number;
  http_requests: number;
  loads: number;
}

/** A database (or branch) as `GET /v1/nosql/databases/{id}` shows it. */
export interface NosqlDb {
  id: string;
  name: string;
  dialect: Dialect;
  datacenter: string;
  rack: string;
  cluster_name: string;
  /** `cassandra.yaml` / `scylla.yaml` overrides. */
  settings: Record<string, unknown>;
  durability: "async" | "sync";
  /** Seconds without clients before scaling to zero; 0 = always on. */
  idle_timeout: number;
  /** Point-in-time restore window, seconds. */
  history_retention: number;
  read_only: boolean;
  /** `"ks.table.column": "mask_inner(1, 1)"`. */
  masking: Record<string, string>;
  /** Unix seconds. */
  expires_at: number | null;
  parent_id: string | null;
  parent_at: number | null;
  schema_only: boolean;
  created_at: number;
  /** Loaded ("running") or scaled to zero ("idle"). */
  state: "running" | "idle";
  endpoints: NosqlEndpoints;
  tls: boolean;
  /** SNI name when hosted with TLS. */
  server_name: string | null;
  credentials: { username: string; password: string; role: string };
  /** Bearer token of the server's HTTP data API. */
  http_token: string;
  /** Owned SSTable bytes (null until measured). */
  storage_bytes: number | null;
  connections: number;
  num_tokens: number | null;
  host_id: string | null;
  status: { loaded: boolean; users: number | null; idle_ms: number | null; metrics: NosqlMetrics };
}

export interface CreateNosqlReq {
  name: string;
  dialect?: Dialect;
  datacenter?: string;
  rack?: string;
  cluster_name?: string;
  settings?: Record<string, unknown>;
  durability?: "async" | "sync";
  idle_timeout?: number;
  history_retention?: number;
  read_only?: boolean;
  expires_at?: number;
  masking?: Record<string, string>;
}

/**
 * `PATCH`: only the fields given change. `settings` is merged (`null` removes a key), `masking`
 * replaces the policy, `expires_at: null` clears the expiry.
 */
export interface PatchNosqlReq {
  name?: string;
  settings?: Record<string, unknown>;
  idle_timeout?: number;
  durability?: "async" | "sync";
  history_retention?: number;
  masking?: Record<string, string>;
  read_only?: boolean;
  expires_at?: number | null;
}

export interface BranchReq {
  name: string;
  /** Unix seconds; default now. */
  at?: number;
  schema_only?: boolean;
  expires_at?: number;
  read_only?: boolean;
}

export interface NosqlDiff {
  parent_id: string;
  /** CQL, one statement per paragraph. */
  diff: string;
  statements: string[];
  /** False when the branch's DESCRIBE output wasn't available (statements are generated). */
  described: boolean;
}

export interface NosqlImport {
  id: string;
  db_id: string;
  /** The source URL with its password redacted. */
  from: string;
  /** Empty: every non-system keyspace. */
  keyspaces: string[];
  /** Refresh interval in seconds (null: once). */
  every: number | null;
  state: "queued" | "running" | "done" | "failed";
  rows: number;
  tables_done: number;
  tables_total: number;
  error: string | null;
  runs: number;
  started_at: number | null;
  finished_at: number | null;
  next_run_at: number | null;
  created_at: number;
}

export interface ImportReq {
  from: string;
  keyspaces?: string[];
  every?: number;
}

export interface CqlReq {
  query: string;
  keyspace?: string;
  page_state?: string;
  page_size?: number;
  trace?: boolean;
}

export interface CqlColumn {
  name: string;
  type: string;
  keyspace: string;
  table: string;
}

interface CqlCommon {
  warnings?: string[];
  tracing_id?: string | null;
  elapsed_ms?: number;
  /** The session keyspace after the statement (`USE` changes it). */
  keyspace?: string;
}

/** A console answer. CQL errors are results, like cqlsh shows them. */
export type CqlResult =
  | (CqlCommon & {
      kind: "rows";
      columns: CqlColumn[];
      /** cqlsh-formatted values (null for null). */
      rows: (string | null)[][];
      /** `SELECT JSON` encodings of the same values. */
      values: unknown[][];
      page_state: string | null;
    })
  | (CqlCommon & { kind: "void" })
  | (CqlCommon & { kind: "set_keyspace"; keyspace: string })
  | (CqlCommon & {
      kind: "schema_change";
      schema_change: { change: string; target: string; keyspace: string; name: string | null; arg_types: string[] };
    })
  | (CqlCommon & { kind?: undefined; error: string; code: number; code_name: string });

export type CqlRows = Extract<CqlResult, { kind: "rows" }>;
export type CqlError = Extract<CqlResult, { error: string }>;

export function isCqlError(r: CqlResult): r is CqlError {
  return "error" in r && typeof r.error === "string";
}

export interface SchemaColumn {
  name: string;
  type: string;
  kind: "partition_key" | "clustering" | "regular" | "static" | string;
  position: number;
  clustering_order: string | null;
}

export interface TableStats {
  sstable_count: number;
  live_bytes: number;
  total_bytes: number;
  memtable_bytes: number;
  partitions: number;
  reads: number;
  writes: number;
  tombstones: number;
}

export interface SchemaTable {
  name: string;
  id: string;
  columns: SchemaColumn[];
  partition_key: string[];
  clustering_key: string[];
  indexes: { name: string; kind: string; options: Record<string, string> }[];
  options: {
    comment: string;
    compaction: Record<string, string>;
    compression: Record<string, string>;
    default_time_to_live: number;
    gc_grace_seconds: number;
  };
  stats?: TableStats;
}

export interface SchemaView extends SchemaTable {
  base_table: string;
  where_clause: string;
}

export interface SchemaKeyspace {
  name: string;
  system: boolean;
  durable_writes: boolean;
  replication: Record<string, string>;
  tablets: boolean | null;
  tables: SchemaTable[];
  views: SchemaView[];
  types: { name: string; fields: { name: string; type: string }[] }[];
  functions: { name: string; argument_types: string[]; return_type: string; language: string }[];
  aggregates: { name: string; argument_types: string[]; return_type: string }[];
}

export interface NosqlSchema {
  dialect: Dialect;
  version: string;
  keyspaces: SchemaKeyspace[];
  /** `DESCRIBE SCHEMA` output. */
  describe: string | null;
}

export interface NosqlUsage {
  db_id: string;
  days: number;
  active_seconds: number;
  reads: number;
  writes: number;
  requests: number;
  connection_seconds: number;
  net_in_bytes: number;
  net_out_bytes: number;
  storage_bytes: number;
  cost: {
    compute_usd: number;
    read_usd: number;
    write_usd: number;
    storage_usd: number;
    egress_usd: number;
    total_usd: number;
    keyspaces_usd: number;
    dynamodb_usd: number;
    astra_usd: number;
    scylla_cloud_usd: number;
  };
}

export interface NosqlActivity {
  state: "running" | "idle";
  metrics: NosqlMetrics;
  users?: number;
  idle_ms?: number;
  /** Rows of `system.clients` / `system_views.clients`; `address` is `ip:port`. */
  clients: Record<string, unknown>[];
  /** ScyllaDB's `system_traces.node_slow_log`. */
  slow_queries: Record<string, unknown>[];
  warnings: unknown[];
}

const db = (id: string) => `/v1/nosql/databases/${encodeURIComponent(id)}`;

export const nosqlApi = {
  info: () => call<NosqlInfo>("GET", "/v1/nosql"),
  list: () => call<NosqlDb[]>("GET", "/v1/nosql/databases"),
  get: (id: string) => call<NosqlDb>("GET", db(id)),
  create: (req: CreateNosqlReq) => call<NosqlDb>("POST", "/v1/nosql/databases", req),
  update: (id: string, patch: PatchNosqlReq) => call<NosqlDb>("PATCH", db(id), patch),
  remove: (id: string) => call<{ deleted: string }>("DELETE", db(id)),
  branch: (id: string, req: BranchReq) => call<NosqlDb>("POST", `${db(id)}/branches`, req),
  /** Re-branch from the parent (latest, or `at`), keeping the id, endpoint and credentials. */
  reset: (id: string, at?: number) => call<NosqlDb>("POST", `${db(id)}/reset`, at === undefined ? {} : { at }),
  /** Restore in place; the replaced state is archived under the returned id. */
  restore: (id: string, at: number) => call<{ restored_to: number; archived: string }>("POST", `${db(id)}/restore`, { at }),
  diff: (id: string) => call<NosqlDiff>("GET", `${db(id)}/diff`),
  imports: (id: string) => call<NosqlImport[]>("GET", `${db(id)}/imports`),
  startImport: (id: string, req: ImportReq) => call<NosqlImport>("POST", `${db(id)}/imports`, req),
  rotatePassword: (id: string) => call<NosqlDb>("POST", `${db(id)}/password`, {}),
  cql: (id: string, req: CqlReq) => call<CqlResult>("POST", `${db(id)}/cql`, req),
  /** Loads the database (wakes it) to read its schema. */
  schema: (id: string) => call<NosqlSchema>("GET", `${db(id)}/schema`),
  usage: (id: string, days = 30) => call<NosqlUsage>("GET", `${db(id)}/usage?days=${days}`),
  activity: (id: string) => call<NosqlActivity>("GET", `${db(id)}/activity`),
  flush: (id: string) => call<{ flushed: boolean }>("POST", `${db(id)}/flush`, {}),
  compact: (id: string) => call<{ compacted_tables: number }>("POST", `${db(id)}/compact`, {}),
  unload: (id: string) => call<{ unloaded: boolean }>("POST", `${db(id)}/unload`, {}),
};

// ---------------------------------------------------------------------------------------------
// Labels and formatting.

export const DIALECT_LABEL: Record<Dialect, string> = { scylla: "ScyllaDB", cassandra: "Cassandra" };

export const IDLE_OPTIONS = [
  { label: "1 min", value: 60 },
  { label: "5 min", value: 300 },
  { label: "15 min", value: 900 },
  { label: "1 hour", value: 3600 },
  { label: "Always on", value: 0 },
];

/** The API caps the window at 35 days. */
export const RETENTION_OPTIONS = [
  { label: "None", value: 0 },
  { label: "1 hour", value: 3600 },
  { label: "24 hours", value: 86_400 },
  { label: "7 days", value: 7 * 86_400 },
  { label: "35 days", value: 35 * 86_400 },
];

export const EXPIRY_PRESETS = [
  { label: "24h", s: 86_400 },
  { label: "72h", s: 3 * 86_400 },
  { label: "7d", s: 7 * 86_400 },
  { label: "30d", s: 30 * 86_400 },
];

export const EVERY_OPTIONS = [
  { label: "Once", value: 0 },
  { label: "Every hour", value: 3600 },
  { label: "Every 6 hours", value: 6 * 3600 },
  { label: "Every 24 hours", value: 86_400 },
  { label: "Every 7 days", value: 7 * 86_400 },
];

/** Database and branch names: lowercase `[a-z0-9-]{1,40}`. */
export function validName(n: string) {
  return /^[a-z0-9-]{1,40}$/.test(n);
}

/** A datacenter, rack or cluster name: printable, no quotes, at most 64 characters. */
export function validLabel(n: string, allowEmpty = false) {
  if (!n) return allowEmpty;
  return n.length <= 64 && !/['"\u0000-\u001f\u007f]/.test(n);
}

/** Time left until `at` (Unix seconds), e.g. `2d 4h`, `3h 12m`, `4m 9s`; `null` once past. */
export function formatCountdown(at: number, nowS: number) {
  const s = at - nowS;
  if (s <= 0) return null;
  if (s >= 86_400) return `${Math.floor(s / 86_400)}d ${Math.floor((s % 86_400) / 3600)}h`;
  if (s >= 3600) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  if (s >= 60) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${s}s`;
}

/** Seconds as the shortest whole unit (`300` → `5m`). */
export function formatSpan(s: number) {
  if (s === 0) return "0s";
  if (s % 86_400 === 0) return `${s / 86_400}d`;
  if (s % 3600 === 0) return `${s / 3600}h`;
  if (s % 60 === 0) return `${s / 60}m`;
  return `${s}s`;
}

/** Long durations, e.g. connection time: `42s`, `12.5 min`, `3.2 h`. */
export function formatSeconds(s: number) {
  if (s < 60) return `${Math.round(s)}s`;
  if (s < 3600) return `${(s / 60).toFixed(1)} min`;
  return `${(s / 3600).toFixed(1)} h`;
}

/** Hide the password in a `cql://user:pass@host` URL. */
export function maskCqlUrl(url: string) {
  return url.replace(/^(cqls?:\/\/[^:/@]*:)[^@]+@/, "$1••••••••@");
}

export function maskSecret(s: string) {
  return s.length > 8 ? `${s.slice(0, 4)}••••••••` : "••••••••";
}

// ---------------------------------------------------------------------------------------------
// Connection details and driver snippets (the same content as `roll nosql connect`).

/** host and port of the CQL endpoint (IPv6 brackets removed). */
export function hostPort(d: Pick<NosqlDb, "endpoints">): [string, number] {
  const ep = d.endpoints.cql || "127.0.0.1:59042";
  const i = ep.lastIndexOf(":");
  if (i < 0) return [ep, 9042];
  const port = Number(ep.slice(i + 1));
  return [ep.slice(0, i).replace(/^\[|\]$/g, ""), Number.isFinite(port) && port > 0 ? port : 9042];
}

/** The `cql://` URL `roll nosql import --from` takes (another roll database as a source). */
export function cqlUrl(d: Pick<NosqlDb, "endpoints" | "credentials" | "tls">) {
  const [h, p] = hostPort(d);
  const host = h.includes(":") ? `[${h}]` : h;
  const u = encodeURIComponent(d.credentials.username);
  const pw = encodeURIComponent(d.credentials.password);
  return `cql://${u}:${pw}@${host}:${p}${d.tls ? "?ssl=true" : ""}`;
}

export const DRIVERS = ["java", "python", "go", "rust", "node", "cqlsh"] as const;
export type Driver = (typeof DRIVERS)[number];

export const DRIVER_LABEL: Record<Driver, string> = {
  java: "Java",
  python: "Python",
  go: "Go",
  rust: "Rust",
  node: "Node.js",
  cqlsh: "cqlsh",
};

type SnippetDb = Pick<NosqlDb, "name" | "endpoints" | "credentials" | "tls" | "server_name" | "datacenter">;

/**
 * A connection snippet with the right datacenter, TLS and SNI settings. Mirrors `snippets()` in
 * crates/roll-cli/src/nosql/mod.rs; `cqlsh` adds the stock cqlsh and the built-in `roll nosql cqlsh`.
 */
export function snippet(d: SnippetDb, driver: Driver, password = d.credentials.password) {
  const [h, p] = hostPort(d);
  const u = d.credentials.username;
  const pw = password;
  const dc = d.datacenter || "datacenter1";
  const tls = d.tls;
  const sni = d.server_name ?? h;
  switch (driver) {
    case "java":
      return `CqlSession session = CqlSession.builder()\n    .addContactPoint(InetSocketAddress.createUnresolved("${h}", ${p}))\n    .withLocalDatacenter("${dc}")\n    .withAuthCredentials("${u}", "${pw}")${
        tls ? "\n    .withSslContext(SSLContext.getDefault()) // SNI: the contact point's host name" : ""
      }\n    .build();\n`;
    case "python":
      return `from cassandra.cluster import Cluster\nfrom cassandra.auth import PlainTextAuthProvider\nfrom cassandra.policies import DCAwareRoundRobinPolicy, TokenAwarePolicy\n${
        tls ? "import ssl\n" : ""
      }cluster = Cluster(["${h}"], port=${p},\n    auth_provider=PlainTextAuthProvider("${u}", "${pw}"),\n    load_balancing_policy=TokenAwarePolicy(DCAwareRoundRobinPolicy(local_dc="${dc}"))${
        tls ? `,\n    ssl_context=ssl.create_default_context(), ssl_options={"server_hostname": "${sni}"}` : ""
      })\nsession = cluster.connect()\n`;
    case "go":
      return `cluster := gocql.NewCluster("${h}")\ncluster.Port = ${p}\ncluster.Authenticator = gocql.PasswordAuthenticator{Username: "${u}", Password: "${pw}"}\ncluster.PoolConfig.HostSelectionPolicy = gocql.TokenAwareHostPolicy(gocql.DCAwareRoundRobinPolicy("${dc}"))${
        tls ? `\ncluster.SslOpts = &gocql.SslOptions{Config: &tls.Config{ServerName: "${sni}"}, EnableHostVerification: true}` : ""
      }\nsession, err := cluster.CreateSession()\n`;
    case "rust":
      return `let session = scylla::client::session_builder::SessionBuilder::new()\n    .known_node("${h}:${p}")\n    .user("${u}", "${pw}")${
        tls ? `\n    .tls_context(Some(tls)) // a rustls ClientConfig; SNI "${sni}"` : ""
      }\n    .build()\n    .await?;\n// Local datacenter: "${dc}" (DefaultPolicy::builder().prefer_datacenter("${dc}".into()))\n`;
    case "node":
      return `const client = new cassandra.Client({\n  contactPoints: ['${h}:${p}'],\n  localDataCenter: '${dc}',\n  credentials: { username: '${u}', password: '${pw}' },${
        tls ? `\n  sslOptions: { servername: '${sni}' },` : ""
      }\n});\n`;
    case "cqlsh":
      return `# cqlsh (Python)\ncqlsh ${h} ${p} -u '${u}' -p '${pw}'${tls ? " --ssl" : ""}\n\n# or the built-in shell, no Python needed\nroll nosql cqlsh ${d.name}\n`;
  }
}

// ---------------------------------------------------------------------------------------------
// The branch tree.

export interface TreeRow {
  db: NosqlDb;
  depth: number;
  /** The parent's id when the parent no longer exists. */
  orphanOf: string | null;
}

/** Databases in tree order: each root (or orphan) followed by its branches, depth-first by name. */
export function branchTree(dbs: NosqlDb[]): TreeRow[] {
  const ids = new Set(dbs.map((d) => d.id));
  const kids = new Map<string, NosqlDb[]>();
  for (const d of dbs) {
    if (d.parent_id && ids.has(d.parent_id)) {
      const list = kids.get(d.parent_id) ?? [];
      list.push(d);
      kids.set(d.parent_id, list);
    }
  }
  const byName = (a: NosqlDb, b: NosqlDb) => a.name.localeCompare(b.name);
  const out: TreeRow[] = [];
  const seen = new Set<string>();
  const walk = (d: NosqlDb, depth: number, orphanOf: string | null) => {
    if (seen.has(d.id)) return;
    seen.add(d.id);
    out.push({ db: d, depth, orphanOf });
    for (const k of (kids.get(d.id) ?? []).sort(byName)) walk(k, depth + 1, null);
  };
  for (const d of [...dbs].sort(byName)) {
    if (!d.parent_id || !ids.has(d.parent_id)) walk(d, 0, d.parent_id && !ids.has(d.parent_id) ? d.parent_id : null);
  }
  // Cycles can't happen, but never drop a database.
  for (const d of dbs) if (!seen.has(d.id)) walk(d, 0, null);
  return out;
}

/** Every database below `id` (branches of branches too). */
export function descendants(dbs: NosqlDb[], id: string): NosqlDb[] {
  const out: NosqlDb[] = [];
  const queue = [id];
  while (queue.length) {
    const p = queue.shift()!;
    for (const d of dbs) {
      if (d.parent_id === p && !out.includes(d)) {
        out.push(d);
        queue.push(d.id);
      }
    }
  }
  return out;
}

// ---------------------------------------------------------------------------------------------
// Settings JSON.

/**
 * The `settings` patch turning `saved` into `draft`: changed and new keys with their values,
 * removed keys as `null` (the API merges settings).
 */
export function settingsPatch(saved: Record<string, unknown>, draft: Record<string, unknown>) {
  const patch: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(draft)) {
    if (JSON.stringify(saved[k]) !== JSON.stringify(v)) patch[k] = v;
  }
  for (const k of Object.keys(saved)) if (!(k in draft)) patch[k] = null;
  return patch;
}

/** Parse a JSON object typed by a user: the object, or an error message. */
export function parseObject(text: string): { value: Record<string, unknown> } | { error: string } {
  if (!text.trim()) return { value: {} };
  try {
    const v = JSON.parse(text);
    if (v === null || typeof v !== "object" || Array.isArray(v)) return { error: "Expected a JSON object" };
    return { value: v as Record<string, unknown> };
  } catch (e) {
    return { error: e instanceof Error ? e.message : String(e) };
  }
}

/** What an import source URL is: a live cluster (`cql://`), an SSTable backup (`s3://`,
 * `file://`), or `null` when it isn't a source the API takes. */
export function importSourceKind(from: string): "live" | "backup" | null {
  const f = from.trim();
  if (/^cqls?:\/\/.+/i.test(f)) return "live";
  if (/^s3:\/\/[^/?]+/i.test(f) || /^file:\/\/\/.+/i.test(f)) return "backup";
  return null;
}
