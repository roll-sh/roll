// Pure helpers of the roll CDN Worker: routes, paths, cache keys, edge credential checks (JWT
// and SigV4 presigned URLs) and cache tags. WebCrypto only, so they run in workerd and Node.

/** How a path prefix is served. */
export type RouteKind = "s3" | "immutable" | "bucket-key" | "website";
/** `any`: anonymous or credentialed; `public`: credentials ignored; `token`: a credential is required. */
export type RouteAuth = "any" | "public" | "token";

export interface Route {
  prefix: string;
  kind: RouteKind;
  auth: RouteAuth;
  /** Overrides `ROLL_ORIGIN` for this prefix. */
  origin?: string;
  /** `none`: never stored at the edge. */
  cache?: string;
  /** bucket-key routes: send the original query (with `token`) to the origin too. */
  forward_token?: boolean;
  /** website routes: the bucket whose website this is (paths are its keys). */
  bucket?: string;
}

export const DEFAULT_ROUTES: Route[] = [
  { prefix: "/v/", kind: "immutable", auth: "any" },
  { prefix: "/", kind: "s3", auth: "any" },
];

/** Parse `ROLL_ROUTES` (a JSON array); throws on anything malformed. */
export function parseRoutes(json: string | undefined): Route[] {
  if (!json || !json.trim()) return DEFAULT_ROUTES;
  const raw: unknown = JSON.parse(json);
  if (!Array.isArray(raw)) throw new Error("ROLL_ROUTES must be a JSON array");
  return raw.map((r, i) => {
    const o = r as Record<string, unknown>;
    const prefix = o.prefix;
    if (typeof prefix !== "string" || !prefix.startsWith("/")) throw new Error(`ROLL_ROUTES[${i}].prefix must start with /`);
    const kind = o.kind ?? "s3";
    if (kind !== "s3" && kind !== "immutable" && kind !== "bucket-key" && kind !== "website") throw new Error(`ROLL_ROUTES[${i}].kind is invalid`);
    if ((kind === "website") !== (typeof o.bucket === "string" && o.bucket !== "")) throw new Error(`ROLL_ROUTES[${i}]: website routes (and only they) name their bucket`);
    const auth = o.auth ?? "any";
    if (auth !== "any" && auth !== "public" && auth !== "token") throw new Error(`ROLL_ROUTES[${i}].auth is invalid`);
    const route: Route = { prefix, kind, auth };
    if (typeof o.origin === "string" && o.origin) route.origin = o.origin.replace(/\/+$/, "");
    if (typeof o.cache === "string") route.cache = o.cache;
    if (o.forward_token === true) route.forward_token = true;
    if (kind === "website") {
      route.bucket = o.bucket as string;
      // A website is anonymous: credentials mean nothing there.
      route.auth = "public";
    }
    return route;
  });
}

/** The route with the longest prefix matching `pathname`. */
export function matchRoute(routes: Route[], pathname: string): Route | undefined {
  let best: Route | undefined;
  for (const r of routes) {
    if (pathname.startsWith(r.prefix) && (!best || r.prefix.length > best.prefix.length)) best = r;
  }
  return best;
}

// ---- Percent-encoding (S3's rules, bytewise) ----

const enc = new TextEncoder();
const dec = new TextDecoder();

function hexVal(c: number): number {
  if (c >= 48 && c <= 57) return c - 48;
  if (c >= 65 && c <= 70) return c - 55;
  if (c >= 97 && c <= 102) return c - 87;
  return -1;
}

/** Percent-decode to bytes; a malformed escape stays as it is (as the origin reads it). */
export function percentDecodeBytes(s: string): Uint8Array {
  const b = enc.encode(s);
  const out: number[] = [];
  for (let i = 0; i < b.length; i++) {
    if (b[i] === 37 && i + 2 < b.length) {
      const h = hexVal(b[i + 1]);
      const l = hexVal(b[i + 2]);
      if (h >= 0 && l >= 0) {
        out.push(h * 16 + l);
        i += 2;
        continue;
      }
    }
    out.push(b[i]);
  }
  return Uint8Array.from(out);
}

export function percentDecode(s: string): string {
  return dec.decode(percentDecodeBytes(s));
}

const HEX = "0123456789ABCDEF";

/** RFC 3986 encoding of bytes (unreserved kept; `/` kept unless `encodeSlash`). */
export function uriEncodeBytes(b: Uint8Array, encodeSlash: boolean): string {
  let out = "";
  for (const c of b) {
    const unreserved = (c >= 48 && c <= 57) || (c >= 65 && c <= 90) || (c >= 97 && c <= 122) || c === 45 || c === 95 || c === 46 || c === 126;
    if (unreserved || (c === 47 && !encodeSlash)) out += String.fromCharCode(c);
    else out += "%" + HEX[c >> 4] + HEX[c & 15];
  }
  return out;
}

export function uriEncode(s: string, encodeSlash: boolean): string {
  return uriEncodeBytes(enc.encode(s), encodeSlash);
}

/** Raw query pairs, still encoded (split on `&`, then the first `=`). */
export function rawPairs(rawQuery: string): [string, string][] {
  const q = rawQuery.startsWith("?") ? rawQuery.slice(1) : rawQuery;
  return q
    .split("&")
    .filter((p) => p.length > 0)
    .map((p) => {
      const i = p.indexOf("=");
      return i < 0 ? [p, ""] : [p.slice(0, i), p.slice(i + 1)];
    });
}

// ---- Paths ----

export interface Target {
  bucket: string;
  /** Decoded; empty for bucket-level paths (listings). */
  key: string;
  /** Immutable URLs: the version or blob token. */
  version?: string;
}

/** Bucket and key of a path under `route` (`null`: the path doesn't name an object). */
export function parsePath(route: Route, pathname: string): Target | null {
  let rest = pathname.slice(route.prefix.length);
  if (route.prefix.endsWith("/") === false && rest.startsWith("/")) rest = rest.slice(1);
  if (route.kind === "website") return { bucket: route.bucket ?? "", key: percentDecode(rest) };
  let version: string | undefined;
  if (route.kind === "immutable") {
    const i = rest.indexOf("/");
    if (i <= 0) return null;
    version = percentDecode(rest.slice(0, i));
    rest = rest.slice(i + 1);
  }
  const i = rest.indexOf("/");
  const bucket = percentDecode(i < 0 ? rest : rest.slice(0, i));
  const key = i < 0 ? "" : percentDecode(rest.slice(i + 1));
  if (!bucket) return null;
  if (route.kind === "immutable" && !key) return null;
  return version === undefined ? { bucket, key } : { bucket, key, version };
}

// ---- Query parameters and cache keys ----

/** The internal cache namespace parameter; never accepted from clients. */
export const NS_PARAM = "roll-cdn-ns";

/** Parameters that carry credentials (left out of cache keys). */
export function isAuthParam(rawName: string): boolean {
  const n = percentDecode(rawName).toLowerCase();
  return n === "token" || n.startsWith("x-amz-") || n === NS_PARAM;
}

/** The query without credentials and the internal namespace (raw pairs, in order). */
export function nonAuthPairs(rawQuery: string): [string, string][] {
  return rawPairs(rawQuery).filter(([k]) => !isAuthParam(k));
}

export function joinPairs(pairs: [string, string][]): string {
  return pairs.map(([k, v]) => (v === "" ? k : `${k}=${v}`)).join("&");
}

/**
 * The public cache key of a request: `https://<host><path>`, plus the sorted non-credential
 * query for bucket-key routes.
 */
export function cacheKey(route: Route, host: string, pathname: string, rawQuery: string): string {
  // The path in S3's encoding, however the client encoded it: what roll purges.
  const base = `https://${host}${canonicalUri(pathname)}`;
  if (route.kind !== "bucket-key") return base;
  const pairs = nonAuthPairs(rawQuery).map(([k, v]): [string, string] => [uriEncode(percentDecode(k), true), uriEncode(percentDecode(v), true)]);
  pairs.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : a[1] < b[1] ? -1 : a[1] > b[1] ? 1 : 0));
  return pairs.length ? `${base}?${pairs.map(([k, v]) => `${k}=${v}`).join("&")}` : base;
}

/** The key credentialed requests are stored under (never served to anonymous ones). */
export function privateKey(publicKey: string): string {
  return `${publicKey}${publicKey.includes("?") ? "&" : "?"}${NS_PARAM}=private`;
}

// ---- Bytes ----

export function hex(b: ArrayBuffer | Uint8Array): string {
  const u = b instanceof Uint8Array ? b : new Uint8Array(b);
  let s = "";
  for (const x of u) s += x.toString(16).padStart(2, "0");
  return s;
}

export async function sha256Hex(s: string): Promise<string> {
  return hex(await crypto.subtle.digest("SHA-256", enc.encode(s)));
}

async function hmac(key: Uint8Array | string, data: string): Promise<Uint8Array> {
  const k = await crypto.subtle.importKey("raw", typeof key === "string" ? enc.encode(key) : key, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  return new Uint8Array(await crypto.subtle.sign("HMAC", k, enc.encode(data)));
}

/** Constant-time string comparison. */
export function safeEqual(a: string, b: string): boolean {
  const x = enc.encode(a);
  const y = enc.encode(b);
  let diff = x.length ^ y.length;
  for (let i = 0; i < Math.max(x.length, y.length); i++) diff |= (x[i] ?? 0) ^ (y[i] ?? 0);
  return diff === 0;
}

// ---- Cache tags ----

/** `rk-<object>,rb-<bucket>`: the first 32 hex digits of SHA-256 of `<bucket>/<key>` and `<bucket>`. */
export async function cacheTags(bucket: string, key: string): Promise<string> {
  const rk = (await sha256Hex(`${bucket}/${key}`)).slice(0, 32);
  const rb = (await sha256Hex(bucket)).slice(0, 32);
  return `rk-${rk},rb-${rb}`;
}

// ---- JWT (`?token=`) ----

export type Verdict = { ok: true } | { ok: false; reason: string };

const MAX_TOKEN = 8192;

function b64urlBytes(s: string): Uint8Array {
  if (!/^[A-Za-z0-9_-]*$/.test(s)) throw new Error("not base64url");
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((s.length + 3) % 4);
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export interface JwtKeys {
  /** HS256 secret (raw text). */
  hs256?: string;
  /** ES256 public key, a JWK as JSON. */
  es256Jwk?: string;
}

/** Verify a signed-URL token for `<bucket>/<key>`: HS256 or ES256, `url`, `exp`, optional `nbf`. */
export async function verifyJwt(token: string, keys: JwtKeys, url: string, nowS: number): Promise<Verdict> {
  if (token.length > MAX_TOKEN) return { ok: false, reason: "token too long" };
  const parts = token.split(".");
  if (parts.length !== 3) return { ok: false, reason: "malformed token" };
  let header: { alg?: unknown };
  let claims: { url?: unknown; exp?: unknown; nbf?: unknown };
  let sig: Uint8Array;
  try {
    header = JSON.parse(dec.decode(b64urlBytes(parts[0])));
    claims = JSON.parse(dec.decode(b64urlBytes(parts[1])));
    sig = b64urlBytes(parts[2]);
  } catch {
    return { ok: false, reason: "malformed token" };
  }
  const signed = enc.encode(`${parts[0]}.${parts[1]}`);
  let valid = false;
  try {
    if (header.alg === "HS256" && keys.hs256) {
      const k = await crypto.subtle.importKey("raw", enc.encode(keys.hs256), { name: "HMAC", hash: "SHA-256" }, false, ["verify"]);
      valid = await crypto.subtle.verify("HMAC", k, sig, signed);
    } else if (header.alg === "ES256" && keys.es256Jwk) {
      const j = JSON.parse(keys.es256Jwk) as JsonWebKey;
      const pub: JsonWebKey = { kty: j.kty, crv: j.crv, x: j.x, y: j.y };
      const k = await crypto.subtle.importKey("jwk", pub, { name: "ECDSA", namedCurve: "P-256" }, false, ["verify"]);
      valid = sig.length === 64 && (await crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, k, sig, signed));
    } else {
      return { ok: false, reason: "unsupported token algorithm" };
    }
  } catch {
    return { ok: false, reason: "invalid token" };
  }
  if (!valid) return { ok: false, reason: "invalid signature" };
  if (typeof claims.exp !== "number") return { ok: false, reason: "token has no exp" };
  if (claims.exp <= nowS) return { ok: false, reason: "token expired" };
  if (claims.nbf !== undefined && (typeof claims.nbf !== "number" || claims.nbf > nowS)) return { ok: false, reason: "token not yet valid" };
  if (claims.url !== url) return { ok: false, reason: "token is for another object" };
  return { ok: true };
}

// ---- SigV4 presigned URLs ----

export const MAX_SKEW_S = 15 * 60;
export const MAX_PRESIGN_S = 7 * 24 * 3600;

export interface PresignCheck {
  method: string;
  /** The request's Host (with its port). */
  host: string;
  /** As on the request line (percent-encoded). */
  path: string;
  rawQuery: string;
  keyId: string;
  secret: string;
  nowS: number;
}

/** `20130524T000000Z` → Unix seconds. */
function amzTime(s: string): number | null {
  const m = /^(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})Z$/.exec(s);
  if (!m) return null;
  return Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +m[6]) / 1000;
}

/** The canonical query of a presigned request (every pair re-encoded, sorted, no signature). */
export function canonicalQuery(rawQuery: string): string {
  const pairs = rawPairs(rawQuery)
    .map(([k, v]): [string, string] => [uriEncodeBytes(percentDecodeBytes(k), true), uriEncodeBytes(percentDecodeBytes(v), true)])
    .filter(([k]) => k !== "X-Amz-Signature");
  pairs.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : a[1] < b[1] ? -1 : a[1] > b[1] ? 1 : 0));
  return pairs.map(([k, v]) => `${k}=${v}`).join("&");
}

export function canonicalUri(path: string): string {
  return path ? uriEncodeBytes(percentDecodeBytes(path), false) : "/";
}

export async function signingKey(secret: string, date: string, region: string, service: string): Promise<Uint8Array> {
  const kDate = await hmac(`AWS4${secret}`, date);
  const kRegion = await hmac(kDate, region);
  const kService = await hmac(kRegion, service);
  return hmac(kService, "aws4_request");
}

/** The signature of a presigned request (host the only signed header, unsigned payload). */
export async function presignSignature(method: string, host: string, path: string, rawQuery: string, secret: string, date: string, region: string, amzDate: string, scope: string): Promise<string> {
  const creq = [method, canonicalUri(path), canonicalQuery(rawQuery), `host:${host.trim()}`, "", "host", "UNSIGNED-PAYLOAD"].join("\n");
  const sts = ["AWS4-HMAC-SHA256", amzDate, scope, await sha256Hex(creq)].join("\n");
  return hex(await hmac(await signingKey(secret, date, region, "s3"), sts));
}

/** Verify a SigV4 presigned URL made with the one key roll publishes to the edge. */
export async function verifyPresigned(c: PresignCheck): Promise<Verdict> {
  const q = new Map<string, string>();
  for (const [k, v] of rawPairs(c.rawQuery)) q.set(percentDecode(k), percentDecode(v));
  if (q.get("X-Amz-Algorithm") !== "AWS4-HMAC-SHA256") return { ok: false, reason: "unsupported presigning algorithm" };
  const cred = (q.get("X-Amz-Credential") ?? "").split("/");
  const amzDate = q.get("X-Amz-Date") ?? "";
  const expires = Number(q.get("X-Amz-Expires") ?? "NaN");
  const signature = q.get("X-Amz-Signature") ?? "";
  if (cred.length !== 5 || cred[3] !== "s3" || cred[4] !== "aws4_request") return { ok: false, reason: "malformed credential" };
  if (!c.keyId || !c.secret || !safeEqual(cred[0], c.keyId)) return { ok: false, reason: "unknown access key" };
  if ((q.get("X-Amz-SignedHeaders") ?? "") !== "host") return { ok: false, reason: "only host may be signed" };
  const t = amzTime(amzDate);
  if (t === null || cred[1] !== amzDate.slice(0, 8)) return { ok: false, reason: "malformed date" };
  if (!Number.isInteger(expires) || expires < 1 || expires > MAX_PRESIGN_S) return { ok: false, reason: "invalid expiry" };
  if (t - c.nowS > MAX_SKEW_S) return { ok: false, reason: "request is from the future" };
  if (c.nowS > t + expires) return { ok: false, reason: "request has expired" };
  if (!/^[0-9a-f]{64}$/.test(signature)) return { ok: false, reason: "malformed signature" };
  const want = await presignSignature(c.method, c.host, c.path, c.rawQuery, c.secret, cred[1], cred[2], amzDate, cred.slice(1).join("/"));
  return safeEqual(want, signature) ? { ok: true } : { ok: false, reason: "signature does not match" };
}

// ---- Credentials in a request ----

export type Credential = { kind: "none" } | { kind: "token"; token: string } | { kind: "presigned" } | { kind: "unsupported"; reason: string };

/** Which credential a query (and headers) carry. */
export function credentialOf(rawQuery: string, hasAuthorization: boolean): Credential {
  let token: string | undefined;
  let amz = false;
  let sigv2 = false;
  for (const [k, v] of rawPairs(rawQuery)) {
    const n = percentDecode(k);
    if (n === "token") token = percentDecode(v);
    else if (n.toLowerCase().startsWith("x-amz-")) amz = true;
    else if (n === "AWSAccessKeyId" || n === "Signature") sigv2 = true;
  }
  if (hasAuthorization) return { kind: "unsupported", reason: "signed requests aren't accepted here; use a presigned URL" };
  if (sigv2) return { kind: "unsupported", reason: "only SigV4 presigned URLs are accepted" };
  if (amz) return { kind: "presigned" };
  if (token !== undefined) return { kind: "token", token };
  return { kind: "none" };
}

// ---- Ranges ----

export type RangeSpec = { start: number; end: number } | "unsatisfiable" | null;

/** One `bytes=` range over `size` bytes (`null`: no range, or one this doesn't handle: send all). */
export function parseRange(header: string | null, size: number): RangeSpec {
  if (!header) return null;
  const m = /^\s*bytes\s*=\s*(\d*)\s*-\s*(\d*)\s*$/i.exec(header);
  if (!m || (m[1] === "" && m[2] === "")) return null;
  if (m[1] === "") {
    const n = Number(m[2]);
    if (n === 0 || size === 0) return "unsatisfiable";
    return { start: Math.max(0, size - n), end: size - 1 };
  }
  const start = Number(m[1]);
  if (start >= size) return "unsatisfiable";
  const end = m[2] === "" ? size - 1 : Math.min(Number(m[2]), size - 1);
  if (end < start) return null;
  return { start, end };
}
