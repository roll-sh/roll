// roll CDN Worker: Cloudflare in front of roll storage's public read endpoints.
//
// - Public objects are cached by path; signed URLs (`?token=` JWTs and SigV4 presigned URLs)
//   are verified here, at the edge, so a cached private object is only served to a valid one.
// - Cache keys never contain credentials. Credentialed responses live in their own namespace
//   and are never served to anonymous requests.
// - The origin is reached with `X-Roll-CDN-Auth` (a shared secret) and told the credential.
// - Bytes served are reported to roll per bucket, in batches with ids (idempotent retries).

import {
  type Route,
  type Target,
  cacheKey,
  cacheTags,
  credentialOf,
  joinPairs,
  matchRoute,
  nonAuthPairs,
  parsePath,
  parseRange,
  parseRoutes,
  privateKey,
  safeEqual,
  verifyJwt,
  verifyPresigned,
} from "./lib";

export interface Env {
  ROLL_ORIGIN: string;
  ROLL_CDN_ORIGIN_SECRET: string;
  ROLL_ROUTES?: string;
  ROLL_CDN_JWT_SECRET?: string;
  ROLL_CDN_JWT_ES256_JWK?: string;
  ROLL_CDN_PRESIGN_KEY_ID?: string;
  ROLL_CDN_PRESIGN_SECRET?: string;
  ROLL_CDN_MAX_CACHE_BYTES?: string;
  ROLL_CDN_DEFAULT_TTL?: string;
  ROLL_CDN_USAGE_URL?: string;
  ROLL_CDN_USAGE_FLUSH_MS?: string;
  ROLL_TEST_HOOKS?: string;
}

type CacheStatus = "HIT" | "MISS" | "PASS";

const IMMUTABLE = "public, max-age=31536000, immutable";
const CLIENT_CC = "x-roll-client-cache-control";

// ---- Routes (parsed once per configuration) ----

let routesFor: { json: string | undefined; routes: Route[] } | undefined;

function routes(env: Env): Route[] {
  if (!routesFor || routesFor.json !== env.ROLL_ROUTES) routesFor = { json: env.ROLL_ROUTES, routes: parseRoutes(env.ROLL_ROUTES) };
  return routesFor.routes;
}

function origin(env: Env, route: Route): string {
  return (route.origin ?? env.ROLL_ORIGIN ?? "").replace(/\/+$/, "");
}

function num(v: string | undefined, dflt: number): number {
  const n = Number(v);
  return v !== undefined && v !== "" && Number.isFinite(n) ? n : dflt;
}

// ---- Usage reporting ----

interface Counts {
  bytes: number;
  requests: number;
  hits: number;
}

const usage = new Map<string, Counts>();
const pending: { batch: string; entries: ({ bucket: string } & Counts)[] }[] = [];
const MAX_PENDING = 1000;
let lastFlush = 0;
let flushing: Promise<void> | undefined;

function record(bucket: string, bytes: number, hit: boolean): void {
  const c = usage.get(bucket) ?? { bytes: 0, requests: 0, hits: 0 };
  c.bytes += bytes;
  c.requests += 1;
  if (hit) c.hits += 1;
  usage.set(bucket, c);
}

async function sendPending(env: Env): Promise<void> {
  if (usage.size) {
    pending.push({ batch: crypto.randomUUID(), entries: [...usage].map(([bucket, c]) => ({ bucket, ...c })) });
    usage.clear();
  }
  // An origin that refuses every batch mustn't grow the isolate without bound.
  if (pending.length > MAX_PENDING) {
    const dropped = pending.splice(0, pending.length - MAX_PENDING);
    console.error(`roll cdn: dropped ${dropped.length} usage batches roll didn't accept`);
  }
  const url = env.ROLL_CDN_USAGE_URL || `${(env.ROLL_ORIGIN ?? "").replace(/\/+$/, "")}/_roll/cdn/usage`;
  for (const b of [...pending]) {
    try {
      const r = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json", "x-roll-cdn-auth": env.ROLL_CDN_ORIGIN_SECRET ?? "" },
        body: JSON.stringify(b),
      });
      await r.body?.cancel();
      // Kept (same batch id) and retried at the next flush unless roll took it.
      if (r.ok) pending.splice(pending.indexOf(b), 1);
    } catch {
      // Retried at the next flush.
    }
  }
}

/** Send what's counted (and every batch that failed before). One flush at a time. */
async function flush(env: Env): Promise<void> {
  while (flushing) await flushing;
  lastFlush = Date.now();
  flushing = sendPending(env).finally(() => {
    flushing = undefined;
  });
  await flushing;
}

function maybeFlush(env: Env, ctx: ExecutionContext): void {
  const now = Date.now();
  if (lastFlush === 0) lastFlush = now;
  if (now - lastFlush >= num(env.ROLL_CDN_USAGE_FLUSH_MS, 10_000) && (usage.size || pending.length)) ctx.waitUntil(flush(env));
}

/** Count a GET's bytes for its bucket (by Content-Length, or as they stream when it's unknown). */
function metered(r: Response, target: Target | null, method: string, hit: boolean, env: Env, ctx: ExecutionContext): Response {
  if (!target || method !== "GET" || (r.status !== 200 && r.status !== 206)) return r;
  const len = r.headers.get("content-length");
  if (len !== null || !r.body) {
    record(target.bucket, Number(len ?? 0) || 0, hit);
    maybeFlush(env, ctx);
    return r;
  }
  let n = 0;
  const count = new TransformStream<Uint8Array, Uint8Array>({
    transform(chunk, c) {
      n += chunk.byteLength;
      c.enqueue(chunk);
    },
    flush() {
      record(target.bucket, n, hit);
      maybeFlush(env, ctx);
    },
  });
  return new Response(r.body.pipeThrough(count), r);
}

// ---- Responses ----

/** A response for the client: internal headers out, the cache status in. */
function forClient(r: Response, status: CacheStatus, opts: { head?: boolean; restoreCC?: boolean } = {}): Response {
  const h = new Headers(r.headers);
  if (opts.restoreCC) {
    const cc = h.get(CLIENT_CC);
    if (cc !== null) h.set("cache-control", cc);
    else h.delete("cache-control");
  }
  for (const name of [...h.keys()]) {
    if (name.startsWith("x-roll-") || name === "cdn-cache-control" || name === "cache-tag" || name === "cf-cache-status") h.delete(name);
  }
  h.set("x-roll-cdn-cache", status);
  return new Response(opts.head ? null : r.body, { status: r.status, statusText: r.statusText, headers: h });
}

function denied(route: Route | undefined, status: number, reason: string): Response {
  const h = { "x-roll-cdn-cache": "PASS" };
  if (route?.kind === "bucket-key") {
    return Response.json({ statusCode: String(status), error: status === 403 ? "Unauthorized" : "Error", message: reason }, { status, headers: h });
  }
  const code = status === 403 ? "AccessDenied" : status === 404 ? "NoSuchKey" : status === 405 ? "MethodNotAllowed" : "InvalidRequest";
  const esc = reason.replace(/[<>&]/g, (c) => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;" })[c] ?? c);
  const body = `<?xml version="1.0" encoding="UTF-8"?>\n<Error><Code>${code}</Code><Message>${esc}</Message></Error>`;
  return new Response(body, { status, headers: { ...h, "content-type": "application/xml" } });
}

function etagMatches(ifNoneMatch: string | null, etag: string | null): boolean {
  if (!ifNoneMatch || !etag) return false;
  const bare = (t: string) => t.trim().replace(/^W\//, "");
  return ifNoneMatch.split(",").some((t) => t.trim() === "*" || bare(t) === bare(etag));
}

function notModified(r: Response, status: CacheStatus, restoreCC: boolean): Response {
  const c = forClient(r, status, { head: true, restoreCC });
  const h = new Headers();
  for (const name of ["etag", "last-modified", "cache-control", "expires", "x-roll-cdn-cache"]) {
    const v = c.headers.get(name);
    if (v !== null) h.set(name, v);
  }
  return new Response(null, { status: 304, headers: h });
}

/** Bytes `start..=end` of a stream. */
function slice(body: ReadableStream<Uint8Array>, start: number, end: number): ReadableStream<Uint8Array> {
  const reader = body.getReader();
  let pos = 0;
  return new ReadableStream<Uint8Array>({
    async pull(c) {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) return c.close();
        const from = pos;
        pos += value.byteLength;
        if (pos <= start) continue;
        const a = Math.max(0, start - from);
        const b = Math.min(value.byteLength, end + 1 - from);
        if (b > a) c.enqueue(value.subarray(a, b));
        if (pos > end) {
          await reader.cancel();
          return c.close();
        }
        return;
      }
    },
    cancel(reason) {
      return reader.cancel(reason);
    },
  });
}

/** A cached 200 answered for a Range request. */
function ranged(r: Response, range: string): Response {
  const size = Number(r.headers.get("content-length") ?? NaN);
  if (!Number.isFinite(size) || !r.body) return r;
  const spec = parseRange(range, size);
  if (spec === null) return r;
  if (spec === "unsatisfiable") {
    r.body.cancel();
    const h = new Headers(r.headers);
    h.set("content-range", `bytes */${size}`);
    h.delete("content-length");
    return new Response(null, { status: 416, headers: h });
  }
  const h = new Headers(r.headers);
  h.set("content-range", `bytes ${spec.start}-${spec.end}/${size}`);
  h.set("content-length", String(spec.end - spec.start + 1));
  return new Response(slice(r.body, spec.start, spec.end), { status: 206, headers: h });
}

// ---- The edge cache ----

/** The stored copy of an origin 200: the edge's Cache-Control, the client's kept aside, tags. */
async function storedCopy(r: Response, body: ReadableStream | null, route: Route, target: Target, env: Env): Promise<Response> {
  const h = new Headers(r.headers);
  const client = h.get("cache-control");
  const edge = h.get("cdn-cache-control") ?? (route.kind === "immutable" ? IMMUTABLE : `public, s-maxage=${num(env.ROLL_CDN_DEFAULT_TTL, 60)}`);
  h.delete("cdn-cache-control");
  h.delete(CLIENT_CC);
  if (client !== null) h.set(CLIENT_CC, client);
  h.set("cache-control", edge);
  h.set("cache-tag", await cacheTags(target.bucket, target.key));
  h.delete("set-cookie");
  return new Response(body, { status: 200, headers: h });
}

function storable(r: Response, route: Route, env: Env): boolean {
  if (route.cache === "none" || r.status !== 200 || !r.body) return false;
  const len = r.headers.get("content-length");
  if (len === null || !(Number(len) <= num(env.ROLL_CDN_MAX_CACHE_BYTES, 536_870_912))) return false;
  const edge = (r.headers.get("cdn-cache-control") ?? "").toLowerCase();
  return !/\b(no-store|private)\b/.test(edge);
}

async function put(cache: Cache, key: string, copy: Response): Promise<void> {
  try {
    await cache.put(key, copy);
  } catch (e) {
    console.warn(`roll cdn: caching ${key} failed: ${e}`);
  }
}

// ---- Origin ----

interface Asked {
  route: Route;
  target: Target | null;
  url: URL;
  host: string;
  credential: { kind: "none" } | { kind: "token"; token: string } | { kind: "presigned" };
}

function originRequest(env: Env, a: Asked, method: string, query: string, extra: Record<string, string> = {}): Request {
  const h = new Headers(extra);
  h.set("x-roll-cdn-auth", env.ROLL_CDN_ORIGIN_SECRET ?? "");
  h.set("x-roll-cdn-host", a.host);
  if (a.credential.kind === "token") h.set("x-roll-cdn-token", a.credential.token);
  if (a.credential.kind === "presigned") h.set("x-roll-cdn-presigned", a.url.search.slice(1));
  const url = `${origin(env, a.route)}${a.url.pathname}${query ? `?${query}` : ""}`;
  return new Request(url, { method, headers: h, redirect: "manual", cf: { cacheTtl: -1 } } as RequestInit);
}

/** The query sent to the origin when filling the cache. */
function fillQuery(a: Asked): string {
  if (a.route.kind !== "bucket-key") return "";
  if (a.route.forward_token) return a.url.search.slice(1);
  return joinPairs(nonAuthPairs(a.url.search));
}

function passHeaders(req: Request): Record<string, string> {
  const out: Record<string, string> = {};
  for (const name of ["range", "if-none-match", "if-modified-since", "if-match", "if-unmodified-since", "origin", "access-control-request-method", "access-control-request-headers"]) {
    const v = req.headers.get(name);
    if (v !== null) out[name] = v;
  }
  return out;
}

// ---- Test hooks (ROLL_TEST_HOOKS=1 only) ----

async function testHook(req: Request, env: Env, path: string): Promise<Response | null> {
  if (env.ROLL_TEST_HOOKS !== "1" || !path.startsWith("/__roll_test/")) return null;
  if (req.method !== "POST" || !safeEqual(req.headers.get("x-roll-cdn-auth") ?? "", env.ROLL_CDN_ORIGIN_SECRET ?? "")) return new Response("forbidden", { status: 403 });
  if (path === "/__roll_test/purge") {
    const body = (await req.json()) as { files?: (string | { url: string })[] };
    let deleted = 0;
    for (const f of body.files ?? []) {
      if (await caches.default.delete(typeof f === "string" ? f : f.url)) deleted++;
    }
    return Response.json({ deleted });
  }
  if (path === "/__roll_test/flush") {
    await flush(env);
    return Response.json({ flushed: true, pending: pending.length });
  }
  return new Response("not found", { status: 404 });
}

// ---- The handler ----

export default {
  async fetch(req: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    const url = new URL(req.url);
    const hook = await testHook(req, env, url.pathname);
    if (hook) return hook;

    const route = matchRoute(routes(env), url.pathname);
    if (!route) return denied(undefined, 404, "no route for this path");
    const head = req.method === "HEAD";
    if (req.method === "OPTIONS") {
      // CORS preflights go to the origin as they are.
      const a: Asked = { route, target: null, url, host: url.host, credential: { kind: "none" } };
      return forClient(await fetch(originRequest(env, a, "OPTIONS", joinPairs(nonAuthPairs(url.search)), passHeaders(req))), "PASS");
    }
    if (req.method !== "GET" && !head) {
      const r = denied(route, 405, "only GET and HEAD go through the CDN");
      r.headers.set("allow", "GET, HEAD");
      return r;
    }

    let target: Target | null;
    try {
      target = parsePath(route, url.pathname);
    } catch {
      return denied(route, 400, "malformed path");
    }
    if (!target && route.kind === "immutable") return denied(route, 404, "immutable URLs are /v/<version>/<bucket>/<key>");

    // Credentials, checked here: an invalid one never reaches the origin.
    let credential: Asked["credential"] = { kind: "none" };
    if (route.auth !== "public") {
      const c = credentialOf(url.search, req.headers.has("authorization"));
      if (c.kind === "unsupported") return denied(route, 403, c.reason);
      if (c.kind !== "none" && (!target || !target.key)) return denied(route, 403, "credentials only open objects");
      const nowS = Math.floor(Date.now() / 1000);
      if (c.kind === "token") {
        const v = await verifyJwt(c.token, { hs256: env.ROLL_CDN_JWT_SECRET, es256Jwk: env.ROLL_CDN_JWT_ES256_JWK }, `${target!.bucket}/${target!.key}`, nowS);
        if (!v.ok) return denied(route, 403, v.reason);
        credential = { kind: "token", token: c.token };
      } else if (c.kind === "presigned") {
        const v = await verifyPresigned({
          method: req.method,
          host: req.headers.get("host") ?? url.host,
          path: url.pathname,
          rawQuery: url.search,
          keyId: env.ROLL_CDN_PRESIGN_KEY_ID ?? "",
          secret: env.ROLL_CDN_PRESIGN_SECRET ?? "",
          nowS,
        });
        if (!v.ok) return denied(route, 403, v.reason);
        credential = { kind: "presigned" };
      }
      if (route.auth === "token" && credential.kind === "none") return denied(route, 403, "this URL needs a signed token");
    }
    const a: Asked = { route, target, url, host: req.headers.get("host") ?? url.host, credential };

    // Never cached: bucket-level paths, and S3 requests with other parameters (versionId,
    // response-* overrides, subresources).
    const extra = nonAuthPairs(url.search);
    const pass = !target || (!target.key && route.kind !== "website") || (route.kind !== "bucket-key" && extra.length > 0);
    if (pass) {
      const query = route.kind === "bucket-key" && route.forward_token ? url.search.slice(1) : joinPairs(extra);
      const r = await fetch(originRequest(env, a, req.method, query, passHeaders(req)));
      return metered(forClient(r, "PASS", { head }), target, req.method, false, env, ctx);
    }

    const cache = caches.default;
    const publicKey = cacheKey(route, a.host, url.pathname, url.search);
    const key = credential.kind === "none" ? publicKey : privateKey(publicKey);
    const range = req.headers.get("range");
    const inm = req.headers.get("if-none-match");

    // Look up: credentialed requests may use their own namespace or the public one.
    for (const k of key === publicKey ? [publicKey] : [key, publicKey]) {
      const hit = await cache.match(new Request(k, { headers: range && !head ? { range } : {} }));
      if (!hit) continue;
      if (etagMatches(inm, hit.headers.get("etag"))) {
        await hit.body?.cancel();
        return notModified(hit, "HIT", true);
      }
      if (head) {
        await hit.body?.cancel();
        const h = forClient(hit, "HIT", { head: true, restoreCC: true });
        if (hit.status === 206) {
          // HEAD with a Range: the whole object's headers.
          h.headers.delete("content-range");
        }
        return h;
      }
      const r = hit.status === 200 && range ? ranged(hit, range) : hit;
      return metered(forClient(r, "HIT", { restoreCC: true }), target, "GET", true, env, ctx);
    }

    // A miss. HEADs and ranges go to the origin as they are; a range also fills the cache.
    const fill = async (): Promise<Response> => {
      const r = await fetch(originRequest(env, a, "GET", fillQuery(a)));
      if (!storable(r, route, env)) return r;
      const [mine, theirs] = r.body!.tee();
      ctx.waitUntil(storedCopy(r, theirs, route, target!, env).then((copy) => put(cache, key, copy)));
      return new Response(mine, r);
    };
    if (head || range) {
      const r = await fetch(originRequest(env, a, req.method, fillQuery(a), range ? { range } : {}));
      if (range && !head) {
        const background = async () => {
          const f = await fetch(originRequest(env, a, "GET", fillQuery(a)));
          if (storable(f, route, env)) await put(cache, key, await storedCopy(f, f.body, route, target!, env));
          else await f.body?.cancel();
        };
        ctx.waitUntil(background().catch(() => {}));
      }
      return metered(forClient(r, range ? "PASS" : "MISS", { head }), target, req.method, false, env, ctx);
    }
    const r = await fill();
    if (r.status === 200 && etagMatches(inm, r.headers.get("etag"))) {
      await r.body?.cancel();
      return notModified(r, "MISS", false);
    }
    return metered(forClient(r, "MISS"), target, "GET", false, env, ctx);
  },
} satisfies ExportedHandler<Env>;
