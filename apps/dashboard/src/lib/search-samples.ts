/**
 * Sample data for a new search cluster (Kibana's sample data, Meilisearch's movies, Algolia's demo
 * indices): two datasets roll generates itself from a seeded PRNG at load time, so nothing
 * third-party ships in the bundle and nothing needs a licence. Each comes with its setup requests,
 * its `_bulk` pages, the one request that removes it, and three ready queries.
 */

import type { Flavor } from "./search-api";
import { BulkBatcher } from "./search-import";

export type SampleId = "sample-products" | "sample-logs";

/** One REST request as the console sends it. */
export interface SampleRequest {
  method: "GET" | "POST" | "PUT" | "DELETE";
  path: string;
  body?: unknown;
}

export interface SampleQuery {
  id: string;
  title: string;
  /** One plain line on what it shows. */
  about: string;
  request: SampleRequest;
}

export interface SampleDataset {
  id: SampleId;
  title: string;
  about: string;
  docs: number;
  /** `index` or `data stream`: what the remove button deletes. */
  kind: "index" | "data stream";
  /** Run in order before the first page (an index or a template). */
  setup: SampleRequest[];
  /** Deletes everything the dataset made (404 when it's already gone). */
  remove: SampleRequest[];
  /** The documents, generated for a load at `now` (ms since the epoch). */
  documents: (now: number) => { id: string; source: Record<string, unknown> }[];
  queries: (flavor: Flavor) => SampleQuery[];
}

/** mulberry32: small, fast and the same on every engine. */
export function prng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const pick = <T>(r: () => number, xs: readonly T[]): T => xs[Math.floor(r() * xs.length)];
const pad = (n: number, w: number) => String(n).padStart(w, "0");

// --- Products ------------------------------------------------------------------------------------

const BRANDS = ["Alder", "Brightline", "Cobalt", "Fernway", "Hollow Peak", "Kestrel", "Lumen", "Marlow", "Quill", "Tidewater", "Vantor", "Wrenfield"] as const;

/** Category → nouns, the features a description mentions, and a price range in dollars. */
const CATEGORIES: Record<string, { nouns: string[]; features: string[]; price: [number, number] }> = {
  Headphones: { nouns: ["wireless headphones", "noise cancelling headphones", "earbuds", "studio headphones"], features: ["a 40-hour battery", "active noise cancelling", "USB-C charging", "a folding frame", "multipoint Bluetooth"], price: [29, 349] },
  "Running shoes": { nouns: ["trail running shoes", "road running shoes", "racing flats", "walking shoes"], features: ["a cushioned midsole", "a grippy outsole", "a breathable mesh upper", "reflective details", "a wide toe box"], price: [59, 219] },
  Backpacks: { nouns: ["travel backpack", "laptop backpack", "hiking daypack", "rolltop backpack"], features: ["a padded 16-inch laptop sleeve", "water-resistant fabric", "a luggage pass-through", "hidden pockets", "a 30 litre main compartment"], price: [39, 189] },
  "Coffee makers": { nouns: ["pour-over coffee maker", "espresso machine", "cold brew maker", "French press"], features: ["a borosilicate glass carafe", "a 15-bar pump", "a built-in grinder", "a keep-warm plate", "a reusable steel filter"], price: [24, 599] },
  Lamps: { nouns: ["desk lamp", "floor lamp", "clip-on reading light", "bedside lamp"], features: ["dimmable warm light", "a wireless charging base", "a flexible arm", "a touch switch", "a memory setting"], price: [19, 159] },
  Keyboards: { nouns: ["mechanical keyboard", "wireless keyboard", "compact keyboard", "ergonomic keyboard"], features: ["hot-swappable switches", "a per-key backlight", "a split layout", "PBT keycaps", "three Bluetooth profiles"], price: [35, 229] },
  Jackets: { nouns: ["rain jacket", "down jacket", "softshell jacket", "windbreaker"], features: ["taped seams", "a packable hood", "recycled insulation", "pit zips", "a two-way front zip"], price: [49, 399] },
  Tents: { nouns: ["two-person tent", "ultralight tent", "family tent", "bivy shelter"], features: ["a full rain fly", "aluminium poles", "two vestibules", "a footprint", "a three-minute pitch"], price: [89, 649] },
  Watches: { nouns: ["GPS watch", "field watch", "smartwatch", "dive watch"], features: ["sapphire glass", "a 10-day battery", "heart-rate tracking", "100 m water resistance", "an always-on display"], price: [49, 799] },
  "Water bottles": { nouns: ["insulated water bottle", "filter bottle", "collapsible bottle", "glass water bottle"], features: ["double-wall insulation", "a leak-proof lid", "a carry loop", "a wide mouth", "dishwasher-safe parts"], price: [12, 59] },
};
const CATEGORY_NAMES = Object.keys(CATEGORIES);
const ADJECTIVES = ["Classic", "Pro", "Lite", "Everyday", "Summit", "Urban", "Studio", "Trail", "Compact", "Max"] as const;
const COLORS = ["black", "white", "slate", "forest green", "sand", "navy", "red", "olive"] as const;
const PRODUCTS = 1000;
/** Mid-2026: products' `created_at` are fixed, so they don't move between loads. */
const PRODUCTS_EPOCH = Date.UTC(2026, 5, 1);

function products(): { id: string; source: Record<string, unknown> }[] {
  const r = prng(0x5eed_0001);
  const out: { id: string; source: Record<string, unknown> }[] = [];
  for (let i = 1; i <= PRODUCTS; i++) {
    const category = CATEGORY_NAMES[(i - 1) % CATEGORY_NAMES.length];
    const c = CATEGORIES[category];
    const brand = pick(r, BRANDS);
    const noun = pick(r, c.nouns);
    const adjective = pick(r, ADJECTIVES);
    const color = pick(r, COLORS);
    const title = `${brand} ${adjective} ${noun.replace(/(^|\s)\S/g, (s) => s.toUpperCase())}`;
    const f1 = pick(r, c.features);
    let f2 = pick(r, c.features);
    if (f2 === f1) f2 = c.features[(c.features.indexOf(f1) + 1) % c.features.length];
    const [lo, hi] = c.price;
    const price = Math.round((lo + (hi - lo) * r() ** 1.6) * 100 - 1) / 100;
    const rating = Math.round((2.6 + 2.4 * Math.sqrt(r())) * 10) / 10;
    const stock = r() < 0.12 ? 0 : Math.floor(r() * 240);
    out.push({
      id: `p${pad(i, 4)}`,
      source: {
        title,
        description: `${brand} ${noun} in ${color}, with ${f1} and ${f2}.`,
        brand,
        category,
        color,
        price,
        rating,
        stock,
        in_stock: stock > 0,
        created_at: new Date(PRODUCTS_EPOCH - Math.floor(r() * 365) * 86_400_000).toISOString().slice(0, 10),
        suggest: { input: [title, `${brand} ${noun}`, noun], weight: Math.round(rating * 10) },
      },
    });
  }
  return out;
}

const productSource = ["title", "brand", "category", "price", "rating", "stock"];

const PRODUCTS_SET: SampleDataset = {
  id: "sample-products",
  title: "Products",
  about: "1,000 products of a made-up shop: titles typed as you search, brands and categories as facets, prices, ratings and stock.",
  docs: PRODUCTS,
  kind: "index",
  setup: [
    {
      method: "PUT",
      path: "/sample-products",
      body: {
        settings: { number_of_shards: 1 },
        mappings: {
          properties: {
            title: { type: "search_as_you_type" },
            description: { type: "text" },
            brand: { type: "keyword" },
            category: { type: "keyword" },
            color: { type: "keyword" },
            price: { type: "scaled_float", scaling_factor: 100 },
            rating: { type: "float" },
            stock: { type: "integer" },
            in_stock: { type: "boolean" },
            created_at: { type: "date" },
            suggest: { type: "completion" },
          },
        },
      },
    },
  ],
  remove: [{ method: "DELETE", path: "/sample-products" }],
  documents: () => products(),
  queries: () => [
    {
      id: "typeahead",
      title: "Search as you type",
      about: "A half-typed, misspelled query still finds the products.",
      request: {
        method: "POST",
        path: "/sample-products/_search",
        body: {
          size: 5,
          query: { multi_match: { query: "wirless headph", type: "bool_prefix", fields: ["title", "title._2gram", "title._3gram"], fuzziness: "AUTO" } },
          _source: productSource,
        },
      },
    },
    {
      id: "facets",
      title: "Facets",
      about: "Counts by category and brand, and price bands, for a filter sidebar.",
      request: {
        method: "POST",
        path: "/sample-products/_search",
        body: {
          size: 0,
          aggs: {
            categories: { terms: { field: "category", size: 10 } },
            brands: { terms: { field: "brand", size: 5 } },
            prices: { range: { field: "price", ranges: [{ key: "under $50", to: 50 }, { key: "$50–150", from: 50, to: 150 }, { key: "over $150", from: 150 }] } },
          },
        },
      },
    },
    {
      id: "filtered",
      title: "Filter and sort",
      about: "Headphones rated 4 or more, in stock, cheapest first.",
      request: {
        method: "POST",
        path: "/sample-products/_search",
        body: {
          size: 5,
          query: { bool: { filter: [{ term: { category: "Headphones" } }, { range: { rating: { gte: 4 } } }, { term: { in_stock: true } }] } },
          sort: [{ price: "asc" }],
          _source: productSource,
        },
      },
    },
  ],
};

// --- Logs ----------------------------------------------------------------------------------------

const LOGS = 2000;
const DAY_MS = 86_400_000;
/** Paths with their weight, method and typical response size. */
const PATHS: { path: string; w: number; method: string; bytes: number }[] = [
  { path: "/", w: 18, method: "GET", bytes: 14_200 },
  { path: "/products", w: 14, method: "GET", bytes: 32_400 },
  { path: "/products/:id", w: 16, method: "GET", bytes: 9_800 },
  { path: "/search", w: 12, method: "GET", bytes: 21_000 },
  { path: "/api/cart", w: 9, method: "POST", bytes: 620 },
  { path: "/api/checkout", w: 4, method: "POST", bytes: 940 },
  { path: "/api/session", w: 7, method: "POST", bytes: 310 },
  { path: "/static/app.js", w: 11, method: "GET", bytes: 188_000 },
  { path: "/static/app.css", w: 6, method: "GET", bytes: 41_000 },
  { path: "/healthz", w: 3, method: "GET", bytes: 2 },
];
const PATH_WEIGHT = PATHS.reduce((s, p) => s + p.w, 0);
const AGENTS = [
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Safari/605.1.15",
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Safari/537.36",
  "Mozilla/5.0 (X11; Linux x86_64; rv:144.0) Gecko/20100101 Firefox/144.0",
  "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1",
  "curl/8.11.0",
] as const;
const COUNTRIES = ["US", "DE", "GB", "FR", "JP", "BR", "IN", "CA", "NL", "AU"] as const;
const HOSTS = ["web-1", "web-2", "web-3"] as const;
const REASONS: Record<number, string> = { 200: "OK", 201: "Created", 304: "Not Modified", 400: "Bad Request", 401: "Unauthorized", 404: "Not Found", 429: "Too Many Requests", 500: "Internal Server Error", 502: "Bad Gateway", 503: "Service Unavailable" };

function pickPath(r: () => number) {
  let x = r() * PATH_WEIGHT;
  for (const p of PATHS) if ((x -= p.w) < 0) return p;
  return PATHS[0];
}

function statusFor(r: () => number, path: string, recent: boolean): number {
  // The last hour has an incident on the cart and checkout, so "errors in the last hour" finds some.
  if (recent && (path === "/api/checkout" || path === "/api/cart") && r() < 0.8) return pick(r, [500, 502, 503]);
  const x = r();
  if (x < 0.015) return pick(r, [500, 502, 503]);
  if (x < 0.07) return pick(r, [400, 401, 404, 404, 404, 429]);
  if (x < 0.12 && !path.startsWith("/api")) return 304;
  return path === "/api/cart" && r() < 0.5 ? 201 : 200;
}

function logs(now: number): { id: string; source: Record<string, unknown> }[] {
  const r = prng(0x5eed_0002);
  const start = now - DAY_MS;
  // Sorted times spread over the day, busier in the (UTC) afternoon.
  const times: number[] = [];
  while (times.length < LOGS) {
    const t = r();
    const hour = new Date(start + t * DAY_MS).getUTCHours();
    if (r() < 0.45 + 0.55 * Math.sin((Math.PI * hour) / 24)) times.push(t);
  }
  times.sort((a, b) => a - b);
  return times.map((t, i) => {
    const at = Math.min(now - 1000, Math.floor(start + t * DAY_MS));
    const p = pickPath(r);
    const path = p.path === "/products/:id" ? `/products/p${pad(1 + Math.floor(r() * PRODUCTS), 4)}` : p.path;
    const status = statusFor(r, p.path, now - at < 3_600_000);
    const bytes = status === 304 ? 0 : status >= 400 ? 180 + Math.floor(r() * 200) : Math.floor(p.bytes * (0.6 + r() * 0.8));
    const ms = Math.round((status >= 500 ? 900 + r() * 2_500 : p.path.startsWith("/static") ? 2 + r() * 12 : 15 + r() ** 3 * 600) * 10) / 10;
    const ip = `${pick(r, [81, 93, 151, 185, 203])}.${Math.floor(r() * 256)}.${Math.floor(r() * 256)}.${1 + Math.floor(r() * 254)}`;
    const level = status >= 500 ? "error" : status >= 400 ? "warn" : "info";
    return {
      id: `l${pad(i + 1, 5)}`,
      source: {
        "@timestamp": new Date(at).toISOString(),
        message: `${p.method} ${path} ${status} ${REASONS[status]} ${ms}ms`,
        log: { level },
        event: { dataset: "sample.web", duration: Math.round(ms * 1e6) },
        http: { request: { method: p.method }, response: { status_code: status, body: { bytes } } },
        url: { path },
        client: { ip },
        source: { geo: { country_iso_code: pick(r, COUNTRIES) } },
        user_agent: { original: pick(r, AGENTS) },
        host: { name: pick(r, HOSTS) },
        service: { name: "shop-web" },
      },
    };
  });
}

const kw = { type: "keyword" };
const LOGS_SET: SampleDataset = {
  id: "sample-logs",
  title: "Web logs",
  about: "2,000 ECS web log lines from the last 24 hours in a data stream, with an incident on checkout in the last hour.",
  docs: LOGS,
  kind: "data stream",
  setup: [
    {
      method: "PUT",
      path: "/_index_template/sample-logs",
      body: {
        index_patterns: ["sample-logs*"],
        data_stream: {},
        priority: 200,
        template: {
          settings: { number_of_shards: 1 },
          mappings: {
            properties: {
              "@timestamp": { type: "date" },
              message: { type: "text" },
              log: { properties: { level: kw } },
              event: { properties: { dataset: kw, duration: { type: "long" } } },
              http: { properties: { request: { properties: { method: kw } }, response: { properties: { status_code: { type: "integer" }, body: { properties: { bytes: { type: "long" } } } } } } },
              url: { properties: { path: kw } },
              client: { properties: { ip: { type: "ip" } } },
              source: { properties: { geo: { properties: { country_iso_code: kw } } } },
              user_agent: { properties: { original: kw } },
              host: { properties: { name: kw } },
              service: { properties: { name: kw } },
            },
          },
        },
      },
    },
  ],
  remove: [
    { method: "DELETE", path: "/_data_stream/sample-logs" },
    { method: "DELETE", path: "/_index_template/sample-logs" },
  ],
  documents: logs,
  queries: (flavor) => [
    {
      id: "errors",
      title: "Errors in the last hour",
      about: "Server errors since an hour ago, newest first.",
      request: {
        method: "POST",
        path: "/sample-logs/_search",
        body: {
          size: 5,
          query: { bool: { filter: [{ range: { "@timestamp": { gte: "now-1h" } } }, { range: { "http.response.status_code": { gte: 500 } } }] } },
          sort: [{ "@timestamp": "desc" }],
          _source: ["@timestamp", "http.response.status_code", "url.path", "message"],
        },
      },
    },
    {
      id: "paths",
      title: "Top paths",
      about: "The busiest paths today, with how many of their requests failed.",
      request: {
        method: "POST",
        path: "/sample-logs/_search",
        body: {
          size: 0,
          aggs: { paths: { terms: { field: "url.path", size: 6 }, aggs: { errors: { filter: { range: { "http.response.status_code": { gte: 500 } } } } } } },
        },
      },
    },
    ...(flavor === "opensearch"
      ? []
      : [
          {
            id: "esql",
            title: "ES|QL stats",
            about: "Requests and median latency per status code, in ES|QL.",
            request: {
              method: "POST",
              path: "/_query",
              body: { query: "FROM sample-logs\n| STATS requests = COUNT(*), median_ms = MEDIAN(event.duration) / 1000000 BY status = http.response.status_code\n| SORT requests DESC\n| LIMIT 10" },
            },
          } satisfies SampleQuery,
        ]),
  ],
};

export const SAMPLES: readonly SampleDataset[] = [PRODUCTS_SET, LOGS_SET];

export const sample = (id: SampleId) => SAMPLES.find((s) => s.id === id) as SampleDataset;

/** The bytes a `_bulk` page may hold: half the cluster's body limit, at most 1 MiB, so progress moves. */
export const pageLimit = (maxBodyMb: number) => Math.min(1024 * 1024, Math.floor((maxBodyMb * 1024 * 1024) / 2));

/**
 * The dataset's documents as `_bulk` NDJSON bodies of at most `maxBytes` each, for `POST /_bulk`
 * (the target is in every action line). Data streams take only `create`; ids are stable, so a
 * page sent twice overwrites (an index) or is refused as a duplicate (a data stream), never doubled.
 */
export function bulkPages(set: SampleDataset, now: number, maxBytes: number): { body: string; docs: number }[] {
  const op = set.kind === "data stream" ? "create" : "index";
  const batcher = new BulkBatcher(maxBytes);
  const pages: { body: string; docs: number }[] = [];
  for (const d of set.documents(now)) {
    const full = batcher.add(`${JSON.stringify({ [op]: { _index: set.id, _id: d.id } })}\n${JSON.stringify(d.source)}\n`);
    if (full) pages.push(full);
  }
  const last = batcher.take();
  if (last) pages.push(last);
  return pages;
}

/** Which datasets an index listing shows (a data stream's backing indices are `.ds-<name>-…`). */
export function presentSamples(indices: string[]): Set<SampleId> {
  const out = new Set<SampleId>();
  for (const i of indices) {
    if (i === "sample-products") out.add("sample-products");
    if (i.startsWith(".ds-sample-logs-")) out.add("sample-logs");
  }
  return out;
}

// --- Copy as code --------------------------------------------------------------------------------

export type SnippetLanguage = "curl" | "javascript" | "python";
export const SNIPPET_LANGUAGES: SnippetLanguage[] = ["curl", "javascript", "python"];

/** ES|QL as code (Elasticsearch-flavor only: OpenSearch has no ES|QL). Search requests go
 * through the playground's `copyAs`. */
export function esqlSnippet(lang: SnippetLanguage, query: string, url: string, keyEnv: string): string {
  const base = url.replace(/\/+$/, "");
  switch (lang) {
    case "curl":
      return `curl -X POST '${base}/_query?format=txt' \\\n  -H "Authorization: ApiKey $${keyEnv}" \\\n  -H 'content-type: application/json' \\\n  -d '${JSON.stringify({ query }).replace(/'/g, "'\\''")}'`;
    case "javascript":
      return `// npm i @elastic/elasticsearch\nimport { Client } from "@elastic/elasticsearch";\n\nconst client = new Client({\n  node: ${JSON.stringify(base)},\n  auth: { apiKey: process.env.${keyEnv} },\n});\nconst resp = await client.esql.query({\n  query: ${JSON.stringify(query)},\n});\nconsole.table(resp.values);\n`;
    case "python":
      return `# pip install elasticsearch\nimport os\nfrom elasticsearch import Elasticsearch\n\nclient = Elasticsearch(${JSON.stringify(base)}, api_key=os.environ["${keyEnv}"])\nresp = client.esql.query(query=${JSON.stringify(query)})\nfor row in resp["values"]:\n    print(row)\n`;
  }
}
