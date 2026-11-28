// The made-up rolld behind the marketing screenshots (see shots.mjs): project "shop" of owner
// "acme", with two services, Postgres, Redis, a stream, a bucket and a search cluster, plus two
// more projects for the sidebar. Shapes follow apps/dashboard/src/lib/*-api.ts.

const NOW = Math.floor(Date.now() / 1000);
const HOUR = 3600;
const DAY = 86400;
const ago = (s) => NOW - s;

export const SHOP = "prj_shop";

const PG = "proj_main";
const REDIS = "rdb_cache";
const STREAM = "st_orders";
const BUCKET = "assets";
const SEARCH = "sc_catalog";
const WEB = "svc_web";
const API = "svc_api";

/**
 * Where shop's cards sit: the columns' x, and how far down each card's own y goes. The shots are
 * 1360 wide, so the columns pack left and the rows close up to fit the canvas.
 */
const LAYOUTS = { wide: { columns: [70, 420, 770], rows: 0.82 } };
let layout = LAYOUTS.wide;
/** catalog is asleep on the canvas, and loaded on its own page where Discover just woke it. */
let searchLoaded = false;

/** Sets what differs between shots: the canvas layout and whether catalog is loaded. */
export function setScene({ layout = "wide", loaded = false } = {}) {
  layout = LAYOUTS[layout];
  searchLoaded = loaded;
}
/** A placed card whose x follows the layout of the moment (JSON.stringify reads the getter). */
const place = (kind, ref, col, y, added_at) => ({
  kind,
  ref,
  get x() {
    return layout.columns[col];
  },
  get y() {
    return Math.round(y * layout.rows);
  },
  added_at,
});

const projects = [
  {
    id: SHOP,
    name: "shop",
    description: "The storefront, its API and everything they run on.",
    icon: { kind: "monogram", color: "teal" },
    created_at: ago(62 * DAY),
    resources: [
      place("service", WEB, 0, 270, ago(60 * DAY)),
      place("service", API, 0, 590, ago(60 * DAY)),
      place("postgres", PG, 1, 160, ago(61 * DAY)),
      place("redis", REDIS, 1, 420, ago(40 * DAY)),
      place("stream", STREAM, 1, 680, ago(30 * DAY)),
      place("bucket", BUCKET, 2, 250, ago(55 * DAY)),
      place("search", SEARCH, 2, 550, ago(20 * DAY)),
    ],
  },
  { id: "prj_docs", name: "docs", description: "", icon: { kind: "monogram", color: "violet" }, created_at: ago(90 * DAY), resources: [] },
  {
    id: "prj_billing",
    name: "billing-worker",
    description: "",
    icon: { kind: "monogram", color: "rose" },
    created_at: ago(14 * DAY),
    resources: [],
  },
];

const info = {
  version: "0.42.0",
  node: "fra-1",
  products: ["postgres", "redis", "streams", "tasks", "storage", "nosql", "queues", "code", "apps", "search"],
};

const endpoint = (branch, state, cu) => ({
  id: `ep_${branch}`,
  project_id: PG,
  branch_id: `br_${branch}`,
  kind: "read_write",
  state,
  min_cu: 0.25,
  max_cu: 1,
  cu,
  suspend_timeout_s: 300,
  host_port: 55432,
  last_active: state === "running" ? NOW : ago(3 * HOUR),
  created_at: ago(61 * DAY),
  connections: state === "running" ? 12 : 0,
});

const branch = (name, parent, created, state, cu, bytes) => ({
  id: `br_${name.replace(/[^a-z0-9]/g, "")}`,
  project_id: PG,
  name,
  parent_id: parent,
  created_at: created,
  endpoint: endpoint(name.replace(/[^a-z0-9]/g, ""), state, cu),
  connection_uri: `postgres://app:••••@db.roll.sh:5432/main?options=endpoint%3Dep_${name.replace(/[^a-z0-9]/g, "")}`,
  storage_bytes: bytes,
});

const pgProject = {
  id: PG,
  name: "main",
  pg_version: 18,
  region: "eu-central",
  created_at: ago(61 * DAY),
  branches: [
    branch("main", null, ago(61 * DAY), "running", 0.5, 2_840_000_000),
    branch("dev", "br_main", ago(9 * DAY), "suspended", 0, 96_000_000),
    branch("pr-1240", "br_main", ago(5 * HOUR), "running", 0.25, 31_000_000),
  ],
};

const redisDb = {
  id: REDIS,
  name: "cache",
  region: "eu-central",
  parent_id: null,
  parent_at: null,
  created_at: ago(40 * DAY),
  redis_url: "rediss://default:••••@cache.redis.roll.sh:6379",
  rest_url: "https://cache.redis.roll.sh",
  rest_token: "••••",
  host: "cache.redis.roll.sh",
  port: 6379,
  max_memory_mb: 256,
  eviction_policy: "allkeys-lru",
  durability: "everysec",
  idle_timeout_s: 600,
  history_retention_s: 7 * DAY,
  status: { state: "idle", used_memory: 18_400_000, keys: 48_211 },
  replicas: [],
};

const T0 = Date.now();

/** Bytes keep flowing, so the page's throughput (a delta between polls) reads a steady rate. */
const streamInfo = () => {
  const dt = (Date.now() - T0) / 1000;
  return {
    id: STREAM,
    name: "orders",
    region: "eu-central",
    created_at: ago(30 * DAY),
    cluster_id: "clu_orders",
    bootstrap: "orders.stream.roll.sh:9092",
    http: "https://orders.stream.roll.sh",
    topics: 3,
    partitions: 10,
    throughput: { total_in_bytes: Math.round(41_200_000_000 + dt * 1_840_000), total_out_bytes: Math.round(118_000_000_000 + dt * 5_310_000) },
  };
};

const topics = [
  { id: "tp_orders", name: "orders", partitions: 6, created_at: ago(30 * DAY), configs: { "retention.ms": "-1" } },
  { id: "tp_payments", name: "payments", partitions: 3, created_at: ago(30 * DAY), configs: {} },
  { id: "tp_dlq", name: "orders.dlq", partitions: 1, created_at: ago(12 * DAY), configs: { "cleanup.policy": "compact" } },
];

const streamDetail = () => ({
  ...streamInfo(),
  topic_list: topics,
  keys: [
    { username: "orders-api", label: "api (production)", created_at: ago(30 * DAY) },
    { username: "orders-worker", label: "fulfilment worker", created_at: ago(21 * DAY) },
  ],
});

const topicDetail = (name) => {
  const t = topics.find((x) => x.name === name);
  if (!t) return null;
  const hw = [1_284_112, 1_279_840, 1_291_006, 1_276_455, 1_288_731, 1_282_190];
  const partition_stats = Array.from({ length: t.partitions }, (_, p) => ({
    partition: p,
    log_start_offset: 0,
    high_watermark: hw[p % hw.length],
    last_stable_offset: hw[p % hw.length],
    local_bytes: 61_000_000 + p * 1_300_000,
    remote_bytes: 1_020_000_000 + p * 9_000_000,
    pending_upload_bytes: p % 2 ? 210_000 : 0,
    segments: 3,
    chunks: 41 + p,
    producers: 2,
    max_timestamp: Date.now() - p * 800,
  }));
  return {
    ...t,
    records: partition_stats.reduce((a, b) => a + b.high_watermark, 0),
    local_bytes: partition_stats.reduce((a, b) => a + b.local_bytes, 0),
    remote_bytes: partition_stats.reduce((a, b) => a + b.remote_bytes, 0),
    partition_stats,
  };
};

const SKUS = ["tee-black-m", "hoodie-sand-l", "cap-navy", "mug-white", "socks-3pk", "tote-natural"];
const STATUSES = ["created", "paid", "paid", "shipped", "created", "delivered"];

/** The tail's opening burst, oldest first (the page puts each new one on top). */
function tail() {
  const out = [];
  for (let i = 0; i < 22; i++) {
    const n = 48_211 + i;
    const at = Date.now() - (22 - i) * 340;
    const record = {
      partition: (i * 5) % 6,
      offset: 1_279_000 + i * 7,
      timestamp: at,
      key: `ord_${n.toString(36).toUpperCase()}`,
      value: { status: STATUSES[i % STATUSES.length], sku: SKUS[(i * 7) % SKUS.length], total: +(19 + ((i * 13) % 70) + 0.99).toFixed(2) },
    };
    out.push(`id: ${record.partition}:${record.offset}\nevent: record\ndata: ${JSON.stringify(record)}\n\n`);
  }
  return out.join("");
}

const bucket = {
  name: BUCKET,
  endpoint: "https://s3.roll.sh",
  url: "https://assets.s3.roll.sh",
  public: true,
  bytes: 18_600_000_000,
  objects: 214_380,
  region: "eu-central",
  versioning: "Enabled",
  recover_days: 7,
  created_at: ago(55 * DAY),
  branches: [],
  parent: null,
  cors: [],
  lifecycle: [],
  tags: {},
  website: null,
  notifications: null,
  replication: null,
  policy: null,
  policy_public: true,
  public_access_block: null,
  file_size_limit: null,
  trash_bytes: 120_000_000,
  versions: 221_004,
  version_bytes: 19_100_000_000,
  object_lock: null,
};

const searchCluster = {
  id: SEARCH,
  name: "catalog",
  region: "eu-central",
  parent_id: null,
  parent_name: null,
  parent_at: null,
  created_at: ago(20 * DAY),
  password_rotated_at: null,
  flavor: "elasticsearch",
  idle_timeout_s: 900,
  max_in_flight: 64,
  max_body_mb: 100,
  destructive_requires_name: true,
  protected: false,
  indices: 4,
  docs: 1_284_000,
  stored_bytes: 2_300_000_000,
  own_bytes: 2_300_000_000,
  owner: "",
  ip_allowlist: [],
  cors: null,
  browser_per_minute: 600,
  url: "https://catalog.search.roll.sh",
  host: "catalog.search.roll.sh",
  port: 443,
  username: "elastic",
  get status() {
    return searchLoaded ? { state: "loaded", in_flight: 2 } : { state: "asleep" };
  },
  branches: [],
};

/** roll apps counts in unix ms. */
const msAgo = (s) => (NOW - s) * 1000;

const env = (name, url, revision, at) => ({
  name,
  state: "running",
  url,
  active: { id: `dep_${revision.slice(0, 5)}`, revision, created_at: at },
  latest: { id: `dep_${revision.slice(0, 5)}`, status: "active" },
  needs_deploy: false,
  ephemeral: false,
});

const service = (id, name, port, url, revision, links, cpu, mem) => ({
  id,
  project: SHOP,
  name,
  source: { kind: "repo", repo: `acme/shop`, branch: "main", path: name === "web" ? "apps/web" : "apps/api" },
  build: { builder: "auto" },
  run: { type: "web", port, min_instances: 0, max_instances: 4, concurrency: 80, cpu, memory_mb: mem, health_timeout_s: 30 },
  registry: null,
  git_login: null,
  github: null,
  links,
  created_at: msAgo(60 * DAY),
  environments: [env("production", url, revision, msAgo(2 * HOUR))],
});

const services = [
  service(
    WEB,
    "web",
    3000,
    "https://shop.acme.com",
    "a41c9e2",
    [
      { kind: "service", ref: API },
      { kind: "bucket", ref: BUCKET },
    ],
    0.5,
    512,
  ),
  service(
    API,
    "api",
    8080,
    "https://api-shop.roll.app",
    "a41c9e2",
    [
      { kind: "postgres", ref: PG },
      { kind: "redis", ref: REDIS },
      { kind: "stream", ref: STREAM },
      { kind: "search", ref: SEARCH },
    ],
    1,
    1024,
  ),
];

const COMMITS = [
  ["a41c9e2", "checkout: one-tap Apple Pay on mobile", "maya", 2 * HOUR, 41_200],
  ["7d03b18", "catalog: facets for size and colour", "jonas", 9 * HOUR, 38_900],
  ["e5f6a90", "fix: cart badge after sign-in", "maya", 26 * HOUR, 36_100],
  ["0c8b4d1", "orders: stream status changes to the api", "lee", 2 * DAY + 3 * HOUR, 52_400],
  ["93ab27f", "deps: next 16.1", "renovate", 3 * DAY, 47_800],
  ["4f1e6c3", "home: new hero, lighter images", "jonas", 4 * DAY + 5 * HOUR, 39_700],
  ["b2d7e05", "search: typo tolerance", "lee", 6 * DAY, 35_200],
];

function environmentOf(id) {
  const s = services.find((x) => x.id === id);
  if (!s) return null;
  const e = s.environments[0];
  return {
    ...e,
    service: id,
    branch: "main",
    instances: [
      { id: "ins_1", deployment: e.active.id, state: "running", started_at: msAgo(2 * HOUR), in_flight: 7 },
      { id: "ins_2", deployment: e.active.id, state: "running", started_at: msAgo(40 * 60), in_flight: 4 },
    ],
    domains:
      id === WEB
        ? [
            { hostname: "shop.acme.com", kind: "custom", record: "cname", status: "active", target: "edge.roll.app", token: "", error: null, tls: "issued", tls_error: null, created_at: msAgo(58 * DAY) },
            { hostname: "web-shop.roll.app", kind: "roll", record: "none", status: "active", target: "", token: "", error: null, tls: "issued", tls_error: null, created_at: msAgo(60 * DAY) },
          ]
        : [{ hostname: "api-shop.roll.app", kind: "roll", record: "none", status: "active", target: "", token: "", error: null, tls: "issued", tls_error: null, created_at: msAgo(60 * DAY) }],
    port: null,
    private: false,
    next_run_at: null,
    crash_loop: null,
    last_cold_start: { at: msAgo(3 * HOUR), ms: 412, runtime_ms: 180, app_ms: 232 },
    created_at: msAgo(60 * DAY),
  };
}

function deploysOf(id) {
  const s = services.find((x) => x.id === id);
  if (!s) return null;
  return COMMITS.map(([rev, message, actor, back, ready], i) => ({
    id: `dep_${rev.slice(0, 5)}`,
    service: id,
    environment: "production",
    status: i === 0 ? "active" : "inactive",
    trigger: actor === "renovate" ? "push" : i === 3 ? "manual" : "push",
    actor,
    revision: rev,
    message,
    image: `registry.roll.sh/acme/shop-${s.name}@sha256:${rev}`,
    plan: null,
    rollback_of: null,
    error: null,
    created_at: msAgo(back),
    started_at: msAgo(back) + 1_000,
    finished_at: msAgo(back) + ready,
    ready_ms: ready,
    image_bytes: 148_000_000 + i * 1_300_000,
    url: `https://${rev}--production-${s.name}-shop.roll.app`,
  }));
}

function deployOf(id, depId) {
  const d = deploysOf(id)?.find((x) => x.id === depId);
  if (!d) return null;
  const t = d.created_at;
  return {
    ...d,
    steps: [
      { step: "build", at: t + 1_000 },
      { step: "rollout", at: t + d.ready_ms - 6_000 },
      { step: "live", at: t + d.ready_ms },
    ],
    build_steps: [
      { name: "load build context", ms: 420, cached: false },
      { name: "FROM node:22-alpine", ms: 0, cached: true },
      { name: "pnpm install --frozen-lockfile", ms: 0, cached: true },
      { name: "COPY . .", ms: 610, cached: false },
      { name: "pnpm build", ms: 27_400, cached: false },
      { name: "export image", ms: 3_900, cached: false },
    ],
  };
}

/** A Next.js build, as BuildKit and roll print it. */
function buildLog(id, depId) {
  const d = deploysOf(id)?.find((x) => x.id === depId);
  if (!d) return null;
  const lines = [
    ["system", `Building ${d.revision} (${d.message}) for production`],
    ["system", "Detected Node.js 22 (pnpm 9.15, Next.js 16.1) in apps/web"],
    ["build", "#1 [internal] load build definition from Dockerfile"],
    ["build", "#1 DONE 0.0s"],
    ["build", "#2 [internal] load metadata for docker.io/library/node:22-alpine"],
    ["build", "#2 DONE 0.4s"],
    ["build", "#3 [deps 1/3] FROM docker.io/library/node:22-alpine"],
    ["build", "#3 CACHED"],
    ["build", "#4 [deps 3/3] RUN pnpm install --frozen-lockfile"],
    ["build", "#4 CACHED"],
    ["build", "#5 [build 2/3] COPY . ."],
    ["build", "#5 DONE 0.6s"],
    ["build", "#6 [build 3/3] RUN pnpm build"],
    ["build", "#6 1.204    ▲ Next.js 16.1.0"],
    ["build", "#6 1.311    Creating an optimized production build ..."],
    ["build", "#6 18.92    ✓ Compiled successfully in 17.4s"],
    ["build", "#6 19.40    ✓ Linting and checking validity of types"],
    ["build", "#6 24.77    ✓ Collecting page data"],
    ["build", "#6 26.10    ✓ Generating static pages (38/38)"],
    ["build", "#6 26.93    Route (app)                        Size  First Load JS"],
    ["build", "#6 26.93    ┌ ○ /                            4.1 kB         118 kB"],
    ["build", "#6 26.93    ├ ƒ /cart                        2.8 kB         116 kB"],
    ["build", "#6 26.93    ├ ƒ /checkout                    6.3 kB         121 kB"],
    ["build", "#6 26.93    └ ● /products/[slug]             3.4 kB         117 kB"],
    ["build", "#6 DONE 27.4s"],
    ["build", "#7 exporting to image"],
    ["build", "#7 pushing layers 3.1s done"],
    ["build", "#7 DONE 3.9s"],
    ["system", `Image ${(d.image_bytes / 1e6).toFixed(0)} MB, pushed to registry.roll.sh/acme/shop-web`],
    ["system", "Rolling out to production: 2 instances"],
    ["system", "ins_1 healthy on :3000 after 0.9s"],
    ["system", "ins_2 healthy on :3000 after 1.1s"],
    ["system", `Live at https://shop.acme.com in ${(d.ready_ms / 1000).toFixed(1)}s`],
  ];
  return lines.map(([stream, text], i) => ({ t: d.created_at + 1_000 + i * (d.ready_ms / lines.length), stream, instance: null, deployment: d.id, text, level: "info" }));
}

const secretsEnv = (slug, name, position, secrets, prot) => ({
  id: `env_${slug}`,
  slug,
  name,
  position,
  kind: "standard",
  base: null,
  labels: {},
  expires_at: null,
  protected: prot,
  approvals: prot ? 1 : 0,
  approvers: [],
  created_at: msAgo(60 * DAY),
  secrets,
  max_age_days: 90,
  notify: null,
  trusted: false,
});

const secretsOverview = {
  project: SHOP,
  environments: [secretsEnv("development", "Development", 0, 16, false), secretsEnv("staging", "Staging", 1, 17, false), secretsEnv("production", "Production", 2, 18, true)],
  previews: 2,
  history_days: 30,
  validity_checks: "on",
  leaks: { rotate: "managed" },
  breakglass: [],
  deletion: { window_days: 7, holds: null },
};

const ENVS = ["development", "staging", "production"];

/**
 * The secrets grid: each key's cell per environment. `link` keys follow a canvas resource;
 * `[1, 1, 2]`-style versions with the same fingerprint read "same", different ones "different".
 */
function secretsGrid() {
  const cell = (fp, version, back, extra = {}) => ({ fp, version, updated_at: msAgo(back), link: null, reference: false, source: "own", comment: false, personal: false, ...extra });
  const linked = (kind, ref, field) => (env) => cell(`l_${kind}_${env}`, 1, 40 * DAY, { link: { kind, ref, field, mode: "live" } });
  const row = (key, cells, status) => ({ key, cells, status });
  const per = (f) => Object.fromEntries(ENVS.map((e, i) => [e, f(e, i)]));
  return {
    path: "/",
    environments: ENVS,
    previews: 2,
    folders: ["/payments", "/email"],
    rows: [
      row("DATABASE_URL", per(linked("postgres", PG, "connection_uri")), "different"),
      row("REDIS_URL", per(linked("redis", REDIS, "redis_url")), "different"),
      row("KAFKA_BOOTSTRAP", per(linked("stream", STREAM, "bootstrap")), "same"),
      row("S3_BUCKET", per(linked("bucket", BUCKET, "name")), "same"),
      row("SEARCH_URL", per(linked("search", SEARCH, "url")), "same"),
      row("STRIPE_SECRET_KEY", per((e, i) => cell(`stripe_${e === "production" ? "live" : "test"}`, i + 2, (9 - i * 3) * DAY)), "different"),
      row("STRIPE_WEBHOOK_SECRET", per((e, i) => cell(`whsec_${e}`, 1, 30 * DAY)), "different"),
      row("SESSION_SECRET", per((e, i) => cell(`sess_${e}`, 3, 21 * DAY)), "different"),
      row("SENTRY_DSN", per(() => cell("sentry", 1, 44 * DAY)), "same"),
      row("SITE_URL", per((e) => cell(`site_${e}`, 1, 58 * DAY)), "different"),
      row("CDN_URL", per(() => cell("cdn", 1, 55 * DAY)), "same"),
      row("SMTP_URL", per((e) => cell(e === "development" ? "smtp_dev" : "smtp", 2, 19 * DAY)), "different"),
      row("SHIPPO_API_KEY", per((e) => cell(e === "production" ? "shippo_live" : "shippo_test", 1, 33 * DAY)), "different"),
      row("TAX_API_KEY", { development: null, staging: null, production: cell("tax", 1, 6 * DAY) }, "missing"),
      row("JWT_PUBLIC_KEY", per(() => cell("jwt", 2, 15 * DAY)), "same"),
      row("OPENAI_API_KEY", { development: null, staging: cell("oai_stg", 1, 5 * DAY), production: cell("oai_prd", 2, 2 * DAY) }, "missing"),
      row("FEATURE_NEW_CHECKOUT", { development: cell("ff_on", 4, 3 * HOUR), staging: cell("ff_on", 2, 26 * HOUR), production: cell("ff_off", 1, 12 * DAY) }, "different"),
      row("LOG_LEVEL", { development: cell("debug", 1, 50 * DAY), staging: cell("info", 1, 50 * DAY), production: cell("info", 1, 50 * DAY) }, "different"),
    ],
  };
}

/** The storefront's search log, newest first: what shoppers typed into catalog. */
const QUERIES = [
  ["trail running shoes", 214], ["black hoodie", 88], ["gift card", 3], ["linen shirt", 41], ["wool socks", 57],
  ["rain jacket women", 132], ["tote bag", 19], ["running shorts", 76], ["beanie", 24], ["sneakers white", 163],
  ["yoga mat", 0], ["denim jacket", 49], ["cap navy", 12], ["merino base layer", 31], ["kids raincoat", 27],
];
const COUNTRIES = ["DE", "US", "GB", "FR", "NL", "SE", "US", "DE", "ES", "US"];
const DEVICES = ["mobile", "desktop", "mobile", "mobile", "tablet"];

function searchLog(count) {
  return Array.from({ length: count }, (_, i) => {
    const [query, results] = QUERIES[(i * 7) % QUERIES.length];
    return {
      "@timestamp": new Date(Math.floor(Date.now() / 1000) * 1000 - i * 41_000 - ((i * 13) % 9) * 1_000).toISOString(),
      query,
      results,
      latency_ms: 4 + ((i * 17) % 23),
      country: COUNTRIES[(i * 3) % COUNTRIES.length],
      device: DEVICES[i % DEVICES.length],
    };
  });
}

/** Searches per half hour over the last day: quiet at night, busy in the evening (Berlin time). */
function overTime() {
  const step = 30 * 60_000;
  const end = Math.floor(Date.now() / step) * step;
  return Array.from({ length: 48 }, (_, i) => {
    const key = end - (47 - i) * step;
    const hour = (new Date(key).getUTCHours() + 1) % 24;
    const day = Math.max(0, Math.sin(((hour - 6) / 24) * Math.PI * 2 - 0.4)) ** 1.3;
    return { key_as_string: new Date(key).toISOString(), key, doc_count: Math.round(180 + day * 2_400 + ((i * 37) % 13) * 25) };
  });
}

/** Just enough Elasticsearch for Discover: the value it answers, or null. */
function elastic({ method, path, body }) {
  const json = (value) => value;
  if (method === "GET" && path.startsWith("/_resolve/index/"))
    return json({ indices: [{ name: "products", attributes: ["open"] }, { name: ".ds-search-log-2026.11.28-000031", attributes: ["open"], data_stream: "search-log" }], aliases: [], data_streams: [{ name: "search-log", backing_indices: [".ds-search-log-2026.11.28-000031"], timestamp_field: "@timestamp" }] });
  if (method === "GET" && path.includes("/_field_caps"))
    return json({
      indices: [".ds-search-log-2026.11.28-000031"],
      fields: {
        "@timestamp": { date: { type: "date", searchable: true, aggregatable: true } },
        query: { text: { type: "text", searchable: true, aggregatable: false } },
        "query.keyword": { keyword: { type: "keyword", searchable: true, aggregatable: true } },
        results: { long: { type: "long", searchable: true, aggregatable: true } },
        latency_ms: { long: { type: "long", searchable: true, aggregatable: true } },
        country: { keyword: { type: "keyword", searchable: true, aggregatable: true } },
        device: { keyword: { type: "keyword", searchable: true, aggregatable: true } },
      },
    });
  if (method === "POST" && path.endsWith("/_search")) {
    const buckets = overTime();
    const total = buckets.reduce((a, b) => a + b.doc_count, 0);
    if (body?.aggs?.sample) {
      const top = [["US", 0.31], ["DE", 0.24], ["GB", 0.14], ["FR", 0.09], ["NL", 0.07]];
      return json({ took: 4, hits: { total: { value: total, relation: "eq" }, hits: [] }, aggregations: { sample: { doc_count: 1000, top: { buckets: top.map(([key, f]) => ({ key, doc_count: Math.round(f * 1000) })) } } } });
    }
    const size = body?.size ?? 50;
    const hits = searchLog(size).map((d, i) => ({ _index: ".ds-search-log-2026.11.28-000031", _id: `q${(9_000_000 - i).toString(36)}`, _score: null, _source: d, sort: [Date.parse(d["@timestamp"])] }));
    return json({ took: 7, timed_out: false, hits: { total: { value: total, relation: "eq" }, max_score: null, hits }, aggregations: { over_time: { buckets, interval: "30m" } } });
  }
  return null;
}

/** `POST …/request`: the console's envelope around what the cluster said. */
function consoleRequest(req) {
  const value = elastic(req);
  const text = JSON.stringify(value ?? { error: { type: "index_not_found_exception", reason: "no such index" }, status: 404 });
  return { status: value ? 200 : 404, headers: [["content-type", "application/json"]], body: text, json: true, took_ms: 6, truncated: false };
}

const repo = {
  id: "repo_shop",
  owner: "acme",
  name: "shop",
  full_name: "acme/shop",
  description: "The storefront, its API and the fulfilment worker.",
  visibility: "private",
  source: "native",
  default_branch: "main",
  head: "cs_a41c9e2",
  head_git: "a41c9e2b7f0c3d18e6a5f4b2c9d07e1a3b5c8f60",
  generation: 4_812,
  created_at: ago(120 * DAY),
  updated_at: ago(2 * HOUR),
  land_policy: "queue",
  required_checks: ["test", "typecheck"],
  clone_url: "https://roll.sh/acme/shop.git",
  web_url: "https://roll.sh/acme/shop",
  github: null,
  import: null,
  languages: { TypeScript: 4_120_000, Rust: 1_310_000, CSS: 402_000, SQL: 61_000, Dockerfile: 4_000 },
  readme: "README.md",
  stats: {
    merges_today: 23,
    merges_per_s: 0,
    queue_depth: 3,
    changesets: 4_812,
    open_changes: 7,
    open_work: 12,
    contributors: 9,
    get merges_daily() {
      return curve(30, 38, 1).map((n) => Math.round(n));
    },
    queue_batch: 2,
  },
};

/** The merge queue: a batch of two under test, one waiting, and the last hour's landings. */
function queue() {
  const entry = (n, title, author, state, back, extra = {}) => ({
    id: `qe_${n}`,
    change: `ch_${n}`,
    number: n,
    title,
    author,
    position: null,
    lane: "normal",
    state,
    error: null,
    batch: null,
    enqueued_at: ago(back),
    enqueued_by: author,
    finished_at: null,
    ...extra,
  });
  const check = (name, status, started, finished = null) => ({ name, status, source: "runner", url: null, summary: null, started_at: ago(started), finished_at: finished === null ? null : ago(finished) });
  const batch = {
    id: "qb_311",
    changes: ["ch_1243", "ch_1244"],
    tip: "cs_f81d2a0",
    started_at: ago(74),
    state: "testing",
    testing_prefix: 2,
    checks: [check("typecheck", "success", 74, 41), check("test", "running", 74), check("e2e", "running", 70)],
  };
  return {
    repo: "acme/shop",
    policy: "queue",
    state: "testing",
    paused: false,
    batch,
    entries: [
      entry(1243, "checkout: one-tap Apple Pay on mobile", "maya", "testing", 6 * 60, { position: 1, batch: "qb_311" }),
      entry(1244, "orders: idempotent refunds", "agent:fixer", "testing", 4 * 60, { position: 2, batch: "qb_311" }),
      entry(1246, "catalog: size facet counts from search", "jonas", "waiting", 50, { position: 3 }),
    ],
    recent: [
      entry(1241, "fix: cart badge after sign-in", "maya", "landed", 31 * 60, { finished_at: ago(22 * 60) }),
      entry(1240, "deps: next 16.1", "renovate", "landed", 48 * 60, { finished_at: ago(37 * 60) }),
      entry(1239, "search: typo tolerance for sku codes", "lee", "ejected", 55 * 60, { finished_at: ago(44 * 60), error: "test: 1 failed (search › sku typo › exact match ranks first)" }),
      entry(1238, "api: paginate /orders by cursor", "agent:fixer", "landed", 70 * 60, { finished_at: ago(52 * 60) }),
      entry(1237, "web: lighter hero images", "jonas", "landed", 80 * 60, { finished_at: ago(52 * 60) }),
    ],
    stats: { depth: 3, landed_1h: 4, avg_wait_s: 412, batch_size: 2, max_batch_size: 8 },
    settings: { checks: ["typecheck", "test", "e2e"], change_checks: ["typecheck", "test"], batch_size: 8, timeout_s: 1800, require_change_checks: true, auto_revert: true, error: null },
    last_green: "cs_a41c9e2",
  };
}

/** A gentle daily curve over `n` days: weekdays busier than weekends, a little noise. */
const curve = (n, base, swing) =>
  Array.from({ length: n }, (_, i) => {
    const d = new Date((NOW - (n - 1 - i) * DAY) * 1000).getUTCDay();
    const week = d === 0 || d === 6 ? 0.55 : 1;
    return +(base * week * (1 + swing * Math.sin(i * 1.7) * 0.3) * (0.9 + ((i * 37) % 11) / 50)).toFixed(3);
  });
const unixDay = (back) => Math.floor((NOW - back * DAY) / DAY);
const isoDay = (back) => new Date((NOW - back * DAY) * 1000).toISOString().slice(0, 10);

function pgUsage(days) {
  const cu = curve(days, 1.6, 1);
  const cuHours = cu.reduce((a, b) => a + b, 0);
  return {
    period: { start: NOW - days * DAY, end: NOW, days },
    compute: { cu_hours: +cuHours.toFixed(1), usd: +(cuHours * 0.09).toFixed(2), active_hours: +(cuHours * 1.9).toFixed(1) },
    storage: { current_bytes: 2_967_000_000, gib_months: 2.6, usd: 0.91 },
    total_usd: +(cuHours * 0.09 + 0.91).toFixed(2),
    projected_month_usd: 6.4,
    always_on_month_usd: 94.5,
    pricing: { usd_per_cu_hour: 0.09, usd_per_gb_month: 0.35 },
    daily: cu.map((c, i) => ({ day: (unixDay(days - 1 - i)) * DAY, cu_hours: c })),
  };
}

const redisUsage = (days) => ({
  db_id: REDIS,
  days,
  active_seconds: 41 * HOUR,
  gib_hours: 1.9,
  commands: 2_840_000,
  net_in_bytes: 610_000_000,
  net_out_bytes: 2_300_000_000,
  peak_memory_bytes: 41_000_000,
  storage_bytes: 18_400_000,
  cost: { memory_usd: 0.31, compute_usd: 0.22, storage_usd: 0.01, egress_usd: 0.08, total_usd: 0.62, elasticache_serverless_usd: 91.4 },
});

const storageUsage = (days) => {
  const daily = curve(days, 0.014, 1).map((usd, i) => ({
    day: isoDay(days - 1 - i),
    class_a: Math.round(usd * 90_000),
    class_b: Math.round(usd * 1_400_000),
    egress_bytes: Math.round(usd * 9e10),
    egress_cached_bytes: Math.round(usd * 3e11),
    storage_bytes: 18_600_000_000 - (days - i) * 40_000_000,
    usd,
  }));
  return {
    bucket: BUCKET,
    days,
    class_a: 38_400,
    class_b: 612_000,
    bytes_in: 2_100_000_000,
    egress_internet_bytes: 31_000_000_000,
    egress_internal_bytes: 4_000_000_000,
    egress_cached_bytes: 120_000_000_000,
    egress_cdn_origin_bytes: 6_000_000_000,
    gb_months: 17.1,
    storage_bytes: 18_600_000_000,
    objects: 214_380,
    daily,
    cost: { storage_usd: 0.26, class_a_usd: 0.05, class_b_usd: 0.03, egress_usd: 0.07, egress_cached_usd: 0, total_usd: 0.41, projected_month_usd: 0.44 },
  };
};

const searchUsage = (days) => ({
  cluster_id: SEARCH,
  days,
  hours: days * 24,
  loaded_seconds: 19 * HOUR,
  loaded_gib_hours: 7.4,
  requests: 182_000,
  peak_loaded_bytes: 1_100_000_000,
  stored_gib_months: 2.1,
  stored_bytes: 2_300_000_000,
  inference_cpu_seconds: 0,
  input_tokens: 0,
  output_tokens: 0,
  cost: {
    memory_usd: 0.08,
    requests_usd: 0.04,
    inference_usd: 0,
    storage_usd: 0.06,
    total_usd: 0.18,
    elastic_cloud_usd: 99,
    opensearch_service_usd: 175,
    opensearch_serverless_usd: 350,
  },
});

const streamUsage = (hours) => ({
  hours,
  estimate: {
    monthly_usd: { s3_storage: 1.12, local_storage: 0.34, requests: 0.88, transfer: 0.56, total: 2.9 },
    msk_monthly_usd: { brokers: 368, storage: 44, total: 412 },
    projected: { bytes_in: 41_200_000_000, bytes_out: 118_000_000_000, remote_gb: 48, local_gb: 3.2 },
  },
});

const appsUsage = (days, scale) => {
  const usd = curve(days, 0.06 * scale, 1);
  return {
    days: usd.map((u, i) => ({
      day: unixDay(days - 1 - i),
      cpu_seconds: Math.round(u * 40_000),
      gb_seconds: Math.round(u * 60_000),
      requests: Math.round(u * 1_900_000),
      egress_bytes: Math.round(u * 4e9),
      build_seconds: i % 4 ? 0 : 74,
      stored_gb_hours: 12,
      usd: u,
    })),
    total_usd: +usd.reduce((a, b) => a + b, 0).toFixed(2),
  };
};

const days = (url, fallback = 30) => Number(url.searchParams.get("days") ?? fallback);

export const ORDERS_SQL = `select status, count(*) as orders, sum(total)::money as revenue
from orders
where created_at > now() - interval '24 hours'
group by 1 order by 2 desc;`;

const ordersByStatus = {
  columns: ["status", "orders", "revenue"],
  rows: [
    ["delivered", "1284", "$96,412.80"],
    ["shipped", "412", "$31,907.15"],
    ["paid", "187", "$14,220.40"],
    ["pending", "41", "$2,866.00"],
    ["refunded", "9", "$611.25"],
  ],
  affected: 5,
};

/** Exact `METHOD path` answers; `null` body means 404. */
const exact = {
  "GET /v1/info": info,
  "GET /v1/workspace/projects": projects,
  "GET /v1/health": { ok: true, version: info.version, pg_versions: [16, 17, 18], default_pg_version: 18, host_cpus: 32, proxy: "db.roll.sh:5432" },
  "GET /v1/code/owners": [
    { name: "acme", kind: "org", display_name: "Acme", bio: null, url: null, created_at: ago(120 * DAY), repos_count: 1 },
  ],
  "GET /v1/accounts/session": null,
  "GET /v1/access/approvals": [],
  "GET /v1/secrets/findings": [],
  "GET /v1/secrets/deleted": [],
  "GET /v1/projects": [pgProject],
  "GET /v1/redis/databases": [redisDb],
  "GET /v1/search": {
    version: "9.5.5",
    opensearch_version: "3.9.0",
    lucene_version: "10.3.1",
    region: "eu-central",
    http: "search.roll.sh:443",
    tls: true,
    domain: "search.roll.sh",
    history_retention_s: 7 * DAY,
    deleted_retention_s: 7 * DAY,
    defaults: { flavor: "elasticsearch", idle_timeout_s: 900, max_in_flight: 64, max_body_mb: 100, destructive_requires_name: true },
    pricing: {
      usd_per_gib_hour: 0.006,
      usd_per_million_requests: 0.2,
      usd_per_vcpu_hour: 0.04,
      usd_per_gb_month_s3: 0.023,
      elastic_cloud_usd_per_month: 99,
      elastic_cloud_gb_per_block: 120,
      opensearch_instance_usd_per_hour: 0.118,
      opensearch_gp3_usd_per_gb_month: 0.122,
      opensearch_min_storage_gb: 10,
      opensearch_serverless_usd_per_ocu_hour: 0.24,
      opensearch_serverless_min_ocu: 2,
      opensearch_serverless_usd_per_gb_month: 0.024,
    },
  },
  "GET /v1/kafka": { bootstrap: "orders.stream.roll.sh:9092", security_protocol: "SASL_SSL", default_stream: null, storage: "s3", streams: 1 },
  "GET /v1/storage/buckets": [bucket],
  "GET /v1/nosql/databases": [],
  "GET /v1/search/clusters": [searchCluster],
  "GET /v1/queues/vhosts": [],
  "GET /_roll/tenants": { tenants: [] },
  "GET /v1/apps/services": services,
  "GET /v1/code/repos": [repo],
  "GET /v1/code": {
    storage: "s3",
    git_url: "https://roll.sh",
    repos: 1,
    stats: { merges_today: 23, merges_per_s: 0, queue_depth: 3 },
    github: { configured: false },
    auth: { required: true, operator: true, caller: { kind: "admin", owner: null } },
  },
  // No roll.toml to compare the canvas with.
  "POST /v1/apps/import": null,
};

/** Answers computed on each call. */
const live = {
  "GET /v1/streams": () => [streamInfo()],
  "GET /v1/apps": () => ({
    services,
    runtime: "fargate",
    domain: "roll.app",
    prices: { cpu_second: 0.000_024, gb_second: 0.000_002_5, egress_gb: 0.05, build_minute: 0.005, stored_gb_month: 0.03 },
  }),
};

/** Pattern answers, tried in order: [method, regex, (match, url) => body]. */
const patterns = [
  ["GET", /^\/v1\/workspace\/projects\/([^/]+)$/, (m) => projects.find((p) => p.id === m[1]) ?? null],
  ["POST", /^\/v1\/search\/clusters\/[^/]+\/request$/, (_, url, headers, body) => consoleRequest(body)],
  ["POST", /^\/v1\/search\/clusters\/[^/]+\/indices\/([^/]+)\/search$/, (m, url, headers, body) => elastic({ method: "POST", path: `/${m[1]}/_search`, body })],
  ["GET", /^\/v1\/code\/repos\/acme\/shop$/, () => repo],
  ["GET", /^\/v1\/code\/repos\/acme\/shop\/queue$/, () => queue()],
  ["GET", /^\/v1\/code\/repos\/acme\/shop\/events$/, () => ({ sse: "" })],
  ["GET", /^\/v1\/search\/clusters\/([^/]+)$/, (m) => (m[1] === SEARCH ? searchCluster : null)],
  ["GET", /^\/v1\/secrets\/projects\/[^/]+\/health$/, () => []],
  ["GET", /^\/v1\/secrets\/projects\/([^/]+)$/, (m) => (m[1] === SHOP ? secretsOverview : null)],
  ["GET", /^\/v1\/secrets\/projects\/[^/]+\/grid$/, () => secretsGrid()],
  ["GET", /^\/v1\/secrets\/projects\/[^/]+\/suggestions$/, () => []],
  ["GET", /^\/v1\/secrets\/projects\/[^/]+\/required$/, () => [
    { key: "DATABASE_URL", path: "/", comment: "Postgres main", type: "url" },
    { key: "STRIPE_SECRET_KEY", path: "/", comment: "", pattern: "^sk_(live|test)_" },
  ]],
  ["GET", /^\/v1\/apps\/services\/([^/]+)$/, (m) => services.find((x) => x.id === m[1]) ?? null],
  ["GET", /^\/v1\/apps\/services\/([^/]+)\/environments\/[^/]+$/, (m) => environmentOf(m[1])],
  ["GET", /^\/v1\/apps\/services\/([^/]+)\/environments\/[^/]+\/deploys$/, (m) => deploysOf(m[1])],
  ["GET", /^\/v1\/apps\/services\/([^/]+)\/environments\/[^/]+\/deploys\/([^/]+)$/, (m) => deployOf(m[1], m[2])],
  ["GET", /^\/v1\/apps\/services\/([^/]+)\/environments\/[^/]+\/deploys\/([^/]+)\/logs$/, (m) => buildLog(m[1], m[2])],
  ["GET", /^\/v1\/apps\/services\/[^/]+\/environments\/[^/]+\/deploys\/[^/]+\/commits$/, () => ({
    since: { id: "dep_7d03b", revision: "7d03b18" },
    commits: [
      { sha: "a41c9e2", summary: "checkout: one-tap Apple Pay on mobile", author: "maya" },
      { sha: "1f9d3c7", summary: "checkout: wallet button states", author: "maya" },
      { sha: "c07e115", summary: "tests: payment sheet e2e", author: "lee" },
    ],
    complete: true,
  })],
  ["GET", /^\/v1\/apps\/projects\/[^/]+\/limits$/, () => ({ alert_usd: 25, hard_usd: 50, month_usd: 3.33, capped: false })],
  ["GET", /^\/v1\/apps\/services\/[^/]+\/changes$/, () => ({ sse: "" })],
  ["GET", /^\/v1\/streams\/([^/]+)$/, (m) => (m[1] === STREAM ? streamDetail() : null)],
  ["GET", /^\/v1\/streams\/[^/]+\/topics\/([^/]+)$/, (m) => topicDetail(decodeURIComponent(m[1]))],
  ["GET", /^\/v1\/streams\/[^/]+\/topics\/[^/]+\/tail$/, (_, url, headers) =>
    headers["last-event-id"] ? { sse: "" } : { sse: tail() }],
  ["POST", /^\/v1\/endpoints\/[^/]+\/query$/, () => ({ results: [ordersByStatus], took_ms: 3 })],
  ["GET", /^\/v1\/projects\/[^/]+\/usage$/, (_, url) => pgUsage(days(url))],
  ["GET", /^\/v1\/redis\/databases\/[^/]+\/usage$/, (_, url) => redisUsage(days(url))],
  ["GET", /^\/v1\/storage\/buckets\/[^/]+\/usage$/, (_, url) => storageUsage(days(url))],
  ["GET", /^\/v1\/search\/clusters\/[^/]+\/usage$/, (_, url) => searchUsage(days(url))],
  ["GET", /^\/v1\/streams\/[^/]+\/usage$/, (_, url) => streamUsage(Number(url.searchParams.get("hours") ?? 168))],
  ["GET", /^\/v1\/apps\/services\/([^/]+)\/environments\/[^/]+\/usage$/, (m, url) => appsUsage(days(url), m[1] === WEB ? 0.7 : 1.1)],
];

/**
 * Answers one request: `{ status, body }` (JSON) or `{ status, sse }` (an event stream's text), or
 * null when nothing here knows it.
 */
export function answer(method, url, headers = {}, body = null) {
  const key = `${method} ${url.pathname}`;
  const wrap = (body) => (body === null ? { status: 404, body: { error: "not_found" } } : body?.sse !== undefined ? { status: 200, ...body } : { status: 200, body });
  if (key in exact) return wrap(exact[key]);
  if (key in live) return wrap(live[key]());
  for (const [m, re, fn] of patterns) {
    if (m !== method) continue;
    const match = url.pathname.match(re);
    if (match) return wrap(fn(match, url, headers, body));
  }
  return null;
}
