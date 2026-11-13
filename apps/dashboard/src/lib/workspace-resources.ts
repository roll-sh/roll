/**
 * The products' resources as the project canvas shows them: one shape for every kind, read from
 * each product's own API (the workspace only stores which project a resource is in and where).
 * Costs are each product's own usage report for this month so far; the month's projection is
 * the same rate carried to its end.
 */

import { appsApi, deployStatus, hostOf, instanceStateLabel, instanceTone, mainEnvironment, PRODUCTION, sourceLabel, type Service } from "./apps-api";
import { codeApi, type Repo } from "./code-api";
import { DIALECT_LABEL, nosqlApi, type NosqlDb } from "./nosql-api";
import { redisApi, type RedisDb } from "./redis-api";
import { api, call, formatBytes, maskUri, streams, type Product, type Project as PgProject, type StreamInfo } from "./roll-api";
import { qapi, type VHost } from "./queues-api";
import { storageApi, type Bucket } from "./storage-api";
import { RESOURCE_KINDS, type PlacedResource, type ResourceKind } from "./workspace-api";

export type ResourceState = "online" | "idle" | "starting" | "failed" | "missing";

/** A resource as a canvas card and the inspector show it. */
export interface ResourceView {
  kind: ResourceKind;
  ref: string;
  name: string;
  /** The line under the name: an address, an engine. */
  sub: string;
  state: ResourceState;
  stateLabel: string;
  /** A small mono note after the state on the card ("$0.00/h" for a service asleep). */
  note?: string;
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
  queue: "Queues",
  tasks: "Tasks",
  repo: "Repository",
  service: "Service",
};

const KIND_PRODUCT: Record<ResourceKind, Product> = {
  postgres: "postgres",
  redis: "redis",
  stream: "streams",
  bucket: "storage",
  nosql: "nosql",
  queue: "queues",
  tasks: "tasks",
  repo: "code",
  service: "apps",
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

function vhost(v: VHost): ResourceView {
  const asleep = v.state === "sleeping" || v.sleeping;
  return {
    kind: "queue",
    ref: v.name,
    name: v.name,
    sub: maskUri(v.amqp_url),
    state: asleep ? "idle" : "online",
    stateLabel: asleep ? "Scaled to zero" : "Running",
    meta: formatBytes(v.stored_bytes),
    connect: maskUri(v.amqp_url),
    href: `/dashboard/queues/queues?vhost=${encodeURIComponent(v.name)}`,
    facts: [
      ["queues", String(v.queues)],
      ["messages", String(v.messages_ready + v.messages_unacked)],
      ["connections", String(v.connections)],
    ],
  };
}

/** A roll apps service: its main environment's state, URL and latest deploy. */
export function service(s: Service): ResourceView {
  const env = mainEnvironment(s);
  const st = env?.state ?? "none";
  const latest = env?.latest ? deployStatus(env.latest.status) : null;
  const pending = latest?.tone === "busy";
  const instances = s.run.min_instances === s.run.max_instances ? `${s.run.min_instances}` : `${s.run.min_instances}–${s.run.max_instances}`;
  return {
    kind: "service",
    ref: s.id,
    name: s.name,
    sub: hostOf(env?.url) || sourceLabel(s.source),
    // A deploy on its way is busy whatever the instances are doing.
    state: pending ? "starting" : instanceTone(st),
    stateLabel: pending ? `${latest.label}…` : instanceStateLabel(st),
    // Asleep or frozen meters nothing: say so where the state is.
    note: !pending && (st === "stopped" || st === "suspended") ? "$0.00/h" : undefined,
    meta: latest && !pending ? latest.label : `${instances} ${instances === "1" ? "instance" : "instances"}`,
    connect: env?.url ?? undefined,
    href: `/dashboard/apps/${encodeURIComponent(s.id)}`,
    facts: [
      ["source", sourceLabel(s.source)],
      ["type", s.run.type],
      ["environments", s.environments.map((e) => e.name).join(", ") || "—"],
      ["instances", instances],
      ["size", `${s.run.cpu} vCPU · ${s.run.memory_mb} MB`],
    ],
  };
}

function tenant(id: string): ResourceView {
  return {
    kind: "tasks",
    ref: id,
    name: id,
    sub: `/t/${id}/v1/tasks`,
    state: "online",
    stateLabel: "Tenant",
    meta: "",
    href: "/dashboard/tasks",
    facts: [["api", `/t/${id}/v1/tasks`]],
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
    on("queue") ? qapi.vhosts().then((l) => l.map(vhost), () => []) : [],
    on("tasks") ? call<{ tenants: string[] }>("GET", "/_roll/tenants").then((t) => t.tenants.map(tenant), () => []) : [],
    on("repo") ? codeApi.repos().then((l) => l.map(repo), () => []) : [],
    on("service") ? appsApi.services().then((l) => l.map(service), () => []) : [],
  ]);
  return { views: new Map(lists.flat().map((v) => [resourceKey(v), v])), kinds };
}

/** A product's resources as rules name them (their ids), with their names: the token builder's picker. */
export function resourcesByProduct(inv: Inventory): Record<string, { id: string; name: string }[]> {
  const out: Record<string, { id: string; name: string }[]> = {};
  for (const v of inv.views.values()) {
    const product = KIND_PRODUCT[v.kind];
    (out[product] ??= []).push({ id: v.ref, name: v.name });
  }
  for (const list of Object.values(out)) list.sort((a, b) => a.name.localeCompare(b.name));
  return out;
}

/** What can be made from the canvas (repos are connected, not made, here; a service has its own flow). */
export const CREATABLE: ResourceKind[] = ["service", "postgres", "redis", "nosql", "stream", "bucket"];

export const KIND_BLURB: Record<ResourceKind, string> = {
  postgres: "Branches, scales to zero",
  redis: "Cache, queues, pub/sub",
  nosql: "ScyllaDB / Cassandra",
  stream: "Kafka-compatible topics",
  bucket: "S3-compatible storage",
  queue: "RabbitMQ-compatible vhost",
  tasks: "Messages, schedules, workflows",
  repo: "Source for this project",
  service: "Your code, scales to zero",
};

/** What a new resource of `kind` would be called in `project` (unique among `taken`). */
export function suggestName(kind: ResourceKind, project: string, taken: Set<string>) {
  const base = `${project}-${{ postgres: "db", redis: "cache", nosql: "nosql", stream: "events", bucket: "files", queue: "jobs", tasks: "tasks", repo: "repo", service: "web" }[kind]}`;
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
    case "queue":
      await qapi.createVhost({ name });
      return name;
    case "tasks":
      await call("POST", "/_roll/tenants", { id: name });
      return name;
    case "repo":
      throw new Error("Repositories are connected, not created, from a project.");
    case "service":
      throw new Error("A service needs a source: make it with New service.");
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
    case "bucket":
      return (await storageApi.usage(ref, days)).cost.total_usd;
    case "stream": {
      // Streams estimate a month from recent traffic; the part of it this month has used.
      const u = await streams.usage(ref, days * 24);
      return (u.estimate.monthly_usd.total * m.elapsed) / m.days;
    }
    case "queue":
    case "tasks":
    case "repo":
      return 0;
    case "service":
      return (await appsApi.usage(ref, PRODUCTION, days)).total_usd;
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
