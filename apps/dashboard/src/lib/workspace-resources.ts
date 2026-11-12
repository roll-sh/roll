/**
 * The products' resources as the project canvas shows them: one shape for every kind, read from
 * each product's own API (the workspace only stores which project a resource is in and where).
 * Costs are each product's own usage report for this month so far; the month's projection is
 * the same rate carried to its end.
 */

import { codeApi, type Repo } from "./code-api";
import { DIALECT_LABEL, nosqlApi, type NosqlDb } from "./nosql-api";
import { redisApi, type RedisDb } from "./redis-api";
import { formatCount, searchApi, type SearchCluster } from "./search-api";
import { api, formatBytes, maskUri, streams, type Product, type Project as PgProject, type StreamInfo } from "./roll-api";
import { storageApi, type Bucket } from "./storage-api";
import { RESOURCE_KINDS, type PlacedResource, type ResourceKind } from "./workspace-api";

export type ResourceState = "online" | "idle" | "starting" | "missing";

/** A resource as a canvas card and the inspector show it. */
export interface ResourceView {
  kind: ResourceKind;
  ref: string;
  name: string;
  /** The line under the name: an address, an engine. */
  sub: string;
  state: ResourceState;
  stateLabel: string;
  /** Small mono figure on the card's right (memory, size, topics). */
  meta: string;
  /** A tab under the card (a database's storage), like a volume. */
  attached?: string;
  /** How to reach it, secrets masked. */
  connect?: string;
  /** What Copy puts on the clipboard: `connect` with its secret. */
  copy?: string;
  /** The product's own page for everything else. */
  href: string;
  facts: [string, string][];
}

export const KIND_LABEL: Record<ResourceKind, string> = {
  postgres: "Postgres",
  redis: "Redis",
  stream: "Stream",
  bucket: "Bucket",
  nosql: "NoSQL",
  search: "Search",
  repo: "Repository",
};

const KIND_PRODUCT: Record<ResourceKind, Product> = {
  postgres: "postgres",
  redis: "redis",
  stream: "streams",
  bucket: "storage",
  nosql: "nosql",
  search: "search",
  repo: "code",
};

const key = (kind: ResourceKind, ref: string) => `${kind}:${ref}`;
export const resourceKey = (r: { kind: ResourceKind; ref: string }) => key(r.kind, r.ref);

const maskRedis = (url: string) => url.replace(/(rediss?:\/\/[^:]*:)[^@]+@/, "$1••••••••@");

function pg(p: PgProject): ResourceView {
  const main = p.branches.find((b) => b.parent_id === null) ?? p.branches[0];
  const ep = main?.endpoint;
  const state: ResourceState = !ep ? "idle" : ep.state === "running" ? "online" : ep.state === "starting" ? "starting" : "idle";
  return {
    kind: "postgres",
    ref: p.id,
    name: p.name,
    sub: `Postgres ${p.pg_version} · ${p.branches.length} ${p.branches.length === 1 ? "branch" : "branches"}`,
    state,
    stateLabel: state === "online" ? "Running" : state === "starting" ? "Starting" : "Scaled to zero",
    meta: ep ? `${ep.min_cu}–${ep.max_cu} CU` : "",
    attached: main ? `${main.name} · ${formatBytes(main.storage_bytes)}` : undefined,
    connect: main?.connection_uri ? maskUri(main.connection_uri) : undefined,
    copy: main?.connection_uri ?? undefined,
    href: "/dashboard/databases",
    facts: [
      ["region", p.region],
      ["compute", ep ? `${ep.min_cu} – ${ep.max_cu} CU` : "—"],
      ["connections", ep ? String(ep.connections) : "0"],
      ["sleeps after", ep ? (ep.suspend_timeout_s ? `${Math.round(ep.suspend_timeout_s / 60)} min idle` : "never") : "—"],
      ["branches", p.branches.map((b) => b.name).join(", ")],
    ],
  };
}

function redis(d: RedisDb): ResourceView {
  const active = d.status.state === "active";
  return {
    kind: "redis",
    ref: d.id,
    name: d.name,
    sub: `${d.host}:${d.port}`,
    state: active ? "online" : "idle",
    stateLabel: active ? "Active" : "Idle",
    meta: d.status.used_memory != null ? formatBytes(d.status.used_memory) : `${d.max_memory_mb} MB max`,
    connect: maskRedis(d.redis_url),
    copy: d.redis_url,
    href: `/dashboard/redis/${encodeURIComponent(d.id)}`,
    facts: [
      ["region", d.region],
      ["memory", `${d.max_memory_mb} MB max`],
      ["eviction", d.eviction_policy],
      ["keys", d.status.keys != null ? String(d.status.keys) : "—"],
      ["sleeps after", d.idle_timeout_s ? `${Math.round(d.idle_timeout_s / 60)} min idle` : "never"],
    ],
  };
}

function stream(s: StreamInfo): ResourceView {
  return {
    kind: "stream",
    ref: s.id,
    name: s.name,
    sub: s.bootstrap,
    state: "online",
    stateLabel: "Ready",
    meta: `${s.topics} ${s.topics === 1 ? "topic" : "topics"}`,
    connect: s.bootstrap,
    href: "/dashboard/streams",
    facts: [
      ["region", s.region],
      ["topics", String(s.topics)],
      ["partitions", String(s.partitions)],
      ["in", formatBytes(s.throughput.total_in_bytes)],
      ["out", formatBytes(s.throughput.total_out_bytes)],
    ],
  };
}

function bucket(b: Bucket): ResourceView {
  return {
    kind: "bucket",
    ref: b.name,
    name: b.name,
    sub: b.endpoint,
    state: "online",
    stateLabel: b.public ? "Public" : "Private",
    meta: formatBytes(b.bytes),
    attached: `${b.objects.toLocaleString("en-US")} objects`,
    connect: b.endpoint,
    href: `/dashboard/storage/${encodeURIComponent(b.name)}`,
    facts: [
      ["region", b.region],
      ["objects", b.objects.toLocaleString("en-US")],
      ["size", formatBytes(b.bytes)],
      ["versioning", String(b.versioning)],
      ["trash", `${b.recover_days} days`],
    ],
  };
}

function nosql(d: NosqlDb): ResourceView {
  const running = d.state === "running";
  return {
    kind: "nosql",
    ref: d.id,
    name: d.name,
    sub: `${DIALECT_LABEL[d.dialect]} · ${d.endpoints.cql}`,
    state: running ? "online" : "idle",
    stateLabel: running ? "Running" : "Scaled to zero",
    meta: formatBytes(d.storage_bytes),
    connect: d.endpoints.cql,
    href: `/dashboard/nosql/${encodeURIComponent(d.id)}`,
    facts: [
      ["engine", DIALECT_LABEL[d.dialect]],
      ["datacenter", d.datacenter],
      ["connections", String(d.connections)],
      ["sleeps after", d.idle_timeout ? `${Math.round(d.idle_timeout / 60)} min idle` : "never"],
    ],
  };
}

function search(c: SearchCluster): ResourceView {
  const loaded = c.status.state === "loaded";
  const engine = c.flavor === "opensearch" ? "OpenSearch" : "Elasticsearch";
  return {
    kind: "search",
    ref: c.id,
    name: c.name,
    sub: `${engine} · ${c.host}:${c.port}`,
    state: loaded ? "online" : "idle",
    stateLabel: loaded ? "Loaded" : "Scaled to zero",
    meta: formatBytes(c.stored_bytes),
    attached: `${c.indices} ${c.indices === 1 ? "index" : "indices"} · ${formatCount(c.docs)} docs`,
    connect: c.url,
    href: `/dashboard/search/${encodeURIComponent(c.id)}`,
    facts: [
      ["engine", engine],
      ["user", c.username],
      ["indices", String(c.indices)],
      ["documents", formatCount(c.docs)],
      ["sleeps after", c.idle_timeout_s ? `${Math.round(c.idle_timeout_s / 60)} min idle` : "never"],
    ],
  };
}

function repo(r: Repo): ResourceView {
  return {
    kind: "repo",
    ref: r.full_name,
    name: r.name,
    sub: r.full_name,
    state: "online",
    stateLabel: r.default_branch,
    meta: r.source === "github" ? "GitHub" : r.visibility,
    href: `/${r.owner}/${r.name}`,
    facts: [
      ["default branch", r.default_branch],
      ["source", r.source === "github" ? "GitHub mirror" : "roll"],
      ["visibility", r.visibility],
    ],
  };
}

/** A placed resource its product no longer has (deleted elsewhere). */
export function missing(p: { kind: ResourceKind; ref: string }): ResourceView {
  return {
    kind: p.kind,
    ref: p.ref,
    name: p.ref.split("/").pop() ?? p.ref,
    sub: "not found",
    state: "missing",
    stateLabel: "Missing",
    meta: "",
    href: "/dashboard",
    facts: [],
  };
}

export interface Inventory {
  /** Every resource, keyed by `kind:ref`. */
  views: Map<string, ResourceView>;
  /** The kinds whose product this rolld runs. */
  kinds: Set<ResourceKind>;
}

/** Every resource of every product this rolld runs. Off products are skipped. */
export async function loadInventory(): Promise<Inventory> {
  const products = await api.info().then(
    (i) => new Set(i.products),
    () => null,
  );
  const on = (k: ResourceKind) => !products || products.has(KIND_PRODUCT[k]);
  const kinds = new Set(RESOURCE_KINDS.filter(on));
  const lists = await Promise.all([
    on("postgres") ? api.projects().then((l) => l.map(pg), () => []) : [],
    on("redis") ? redisApi.list().then((l) => l.map(redis), () => []) : [],
    on("stream") ? streams.list().then((l) => l.map(stream), () => []) : [],
    on("bucket") ? storageApi.buckets().then((l) => l.map(bucket), () => []) : [],
    on("nosql") ? nosqlApi.list().then((l) => l.map(nosql), () => []) : [],
    on("search") ? searchApi.list().then((l) => l.map(search), () => []) : [],
    on("repo") ? codeApi.repos().then((l) => l.map(repo), () => []) : [],
  ]);
  return { views: new Map(lists.flat().map((v) => [resourceKey(v), v])), kinds };
}

/** What can be made from the canvas (repos are connected, not made, here). */
export const CREATABLE: ResourceKind[] = ["postgres", "redis", "nosql", "search", "stream", "bucket"];

export const KIND_BLURB: Record<ResourceKind, string> = {
  postgres: "Branches, scales to zero",
  redis: "Cache, queues, pub/sub",
  nosql: "ScyllaDB / Cassandra",
  search: "Elasticsearch-compatible",
  stream: "Kafka-compatible topics",
  bucket: "S3-compatible storage",
  repo: "Source for this project",
};

/** What a new resource of `kind` would be called in `project` (unique among `taken`). */
export function suggestName(kind: ResourceKind, project: string, taken: Set<string>) {
  const base = `${project}-${{ postgres: "db", redis: "cache", nosql: "nosql", search: "search", stream: "events", bucket: "files", repo: "repo" }[kind]}`;
  let name = base;
  for (let i = 2; taken.has(name); i++) name = `${base}-${i}`;
  return name;
}

/** Creates the resource in its product and returns its ref. */
export async function createResource(kind: ResourceKind, name: string): Promise<string> {
  switch (kind) {
    case "postgres":
      return (await api.createProject(name)).id;
    case "redis":
      return (await redisApi.create(name, {})).id;
    case "stream":
      return (await streams.create(name)).id;
    case "bucket":
      return (await storageApi.createBucket(name)).name;
    case "nosql":
      return (await nosqlApi.create({ name })).id;
    case "search":
      return (await searchApi.create(name, {})).id;
    case "repo":
      throw new Error("Repositories are connected, not created, from a project.");
  }
}

// ── this month's spend ──

/** Dollars as a bill shows them: cents, and whole dollars from $1,000. */
export const usd = (n: number) => (n >= 1000 ? `$${Math.round(n).toLocaleString("en-US")}` : `$${n.toFixed(2)}`);

export interface Month {
  /** Days of the month so far, today included. */
  elapsed: number;
  days: number;
  label: string;
  end: string;
}

export function monthOf(now: Date): Month {
  const days = new Date(now.getFullYear(), now.getMonth() + 1, 0).getDate();
  const label = now.toLocaleString("en-US", { month: "long" });
  return { elapsed: now.getDate(), days, label, end: `${label.slice(0, 3)} ${days}` };
}

/** The month's total at the rate so far. */
export const projectMonth = (spent: number, m: Month) => (m.elapsed ? (spent / m.elapsed) * m.days : spent);

/** This month's spend so far on one resource, in USD (0 for kinds without a bill). */
export async function spentThisMonth(kind: ResourceKind, ref: string, m: Month): Promise<number> {
  const days = m.elapsed;
  switch (kind) {
    case "postgres":
      return (await api.usage(ref, days)).total_usd;
    case "redis":
      return (await redisApi.usage(ref, days)).cost.total_usd;
    case "nosql":
      return (await nosqlApi.usage(ref, days)).cost.total_usd;
    case "search":
      return (await searchApi.usage(ref, days)).cost.total_usd;
    case "bucket":
      return (await storageApi.usage(ref, days)).cost.total_usd;
    case "stream": {
      // Streams estimate a month from recent traffic; the part of it this month has used.
      const u = await streams.usage(ref, days * 24);
      return (u.estimate.monthly_usd.total * m.elapsed) / m.days;
    }
    case "repo":
      return 0;
  }
}

// ── canvas geometry ──

export const CARD_W = 208;
export const CARD_H = 96;
const GAP_X = 56;
const GAP_Y = 44;

/** A free spot for a new card: the first cell of a grid, row by row, no card overlaps. */
export function freeSpot(taken: { x: number; y: number }[], cols = 3): { x: number; y: number } {
  const clear = (x: number, y: number) =>
    taken.every((t) => Math.abs(t.x - x) >= CARD_W + 16 || Math.abs(t.y - y) >= CARD_H + 24);
  for (let row = 0; row < 50; row++)
    for (let col = 0; col < cols; col++) {
      const x = 48 + col * (CARD_W + GAP_X);
      const y = 48 + row * (CARD_H + GAP_Y);
      if (clear(x, y)) return { x, y };
    }
  return { x: 48, y: 48 };
}

/** Where a placed resource's card is, as the canvas and the sidebar's map draw it. */
export const cardBox = (r: Pick<PlacedResource, "x" | "y">) => ({ x: r.x, y: r.y, w: CARD_W, h: CARD_H });
