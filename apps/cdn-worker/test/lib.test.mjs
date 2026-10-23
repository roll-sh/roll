// Unit tests of the Worker's pure helpers (dist/lib.js, built by `pnpm build`).
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  DEFAULT_ROUTES,
  cacheKey,
  cacheTags,
  canonicalQuery,
  credentialOf,
  matchRoute,
  parsePath,
  parseRange,
  parseRoutes,
  presignSignature,
  privateKey,
  sha256Hex,
  verifyJwt,
  verifyPresigned,
} from "../dist/lib.js";

const b64url = (b) => Buffer.from(b).toString("base64url");
const enc = new TextEncoder();

async function hs256(claims, secret, header = { alg: "HS256", typ: "JWT" }) {
  const head = `${b64url(JSON.stringify(header))}.${b64url(JSON.stringify(claims))}`;
  const k = await crypto.subtle.importKey("raw", enc.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  return `${head}.${b64url(new Uint8Array(await crypto.subtle.sign("HMAC", k, enc.encode(head))))}`;
}

const NOW = 1_800_000_000;
const SECRET = "super-secret-jwt-key-for-tests";

test("HS256 tokens: valid, expired, not yet valid, wrong object, bad signature", async () => {
  const keys = { hs256: SECRET };
  const ok = await hs256({ url: "media/a b/c.png", exp: NOW + 60 }, SECRET);
  assert.deepEqual(await verifyJwt(ok, keys, "media/a b/c.png", NOW), { ok: true });
  assert.equal((await verifyJwt(ok, keys, "media/a b/other.png", NOW)).ok, false);
  assert.equal((await verifyJwt(ok, keys, "media/a b/c.png", NOW + 60)).ok, false, "exp is exclusive");
  const nbf = await hs256({ url: "m/k", exp: NOW + 60, nbf: NOW + 10 }, SECRET);
  assert.equal((await verifyJwt(nbf, keys, "m/k", NOW)).ok, false);
  assert.equal((await verifyJwt(nbf, keys, "m/k", NOW + 10)).ok, true);
  const other = await hs256({ url: "m/k", exp: NOW + 60 }, "another-secret");
  assert.equal((await verifyJwt(other, keys, "m/k", NOW)).reason, "invalid signature");
  const noExp = await hs256({ url: "m/k" }, SECRET);
  assert.equal((await verifyJwt(noExp, keys, "m/k", NOW)).ok, false);
  // A tampered payload under the original signature.
  const [h, , s] = ok.split(".");
  const forged = `${h}.${b64url(JSON.stringify({ url: "media/secret", exp: NOW + 60 }))}.${s}`;
  assert.equal((await verifyJwt(forged, keys, "media/secret", NOW)).ok, false);
  assert.equal((await verifyJwt("not.a.jwt!", keys, "m/k", NOW)).ok, false);
  assert.equal((await verifyJwt("onlyone", keys, "m/k", NOW)).ok, false);
});

test("alg none and unconfigured algorithms are refused", async () => {
  const none = `${b64url(JSON.stringify({ alg: "none" }))}.${b64url(JSON.stringify({ url: "m/k", exp: NOW + 60 }))}.`;
  assert.equal((await verifyJwt(none, { hs256: SECRET }, "m/k", NOW)).ok, false);
  const ok = await hs256({ url: "m/k", exp: NOW + 60 }, SECRET);
  assert.equal((await verifyJwt(ok, {}, "m/k", NOW)).ok, false, "no HS256 secret configured");
  const hs384 = await hs256({ url: "m/k", exp: NOW + 60 }, SECRET, { alg: "HS384" });
  assert.equal((await verifyJwt(hs384, { hs256: SECRET }, "m/k", NOW)).ok, false);
});

test("ES256 tokens verify against the published JWK", async () => {
  const pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
  const jwk = JSON.stringify(await crypto.subtle.exportKey("jwk", pair.publicKey));
  const head = `${b64url(JSON.stringify({ alg: "ES256", typ: "JWT" }))}.${b64url(JSON.stringify({ url: "b/k.txt", exp: NOW + 5 }))}`;
  const sig = new Uint8Array(await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, pair.privateKey, enc.encode(head)));
  const token = `${head}.${b64url(sig)}`;
  assert.deepEqual(await verifyJwt(token, { es256Jwk: jwk }, "b/k.txt", NOW), { ok: true });
  assert.equal((await verifyJwt(token, { es256Jwk: jwk }, "b/k.txt", NOW + 5)).ok, false);
  assert.equal((await verifyJwt(token, { hs256: SECRET }, "b/k.txt", NOW)).ok, false, "ES256 needs the JWK");
  const other = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
  assert.equal((await verifyJwt(token, { es256Jwk: JSON.stringify(await crypto.subtle.exportKey("jwk", other.publicKey)) }, "b/k.txt", NOW)).ok, false);
});

// AWS's documented presigned URL example (SigV4 query-string auth).
const AWS = {
  host: "examplebucket.s3.amazonaws.com",
  path: "/test.txt",
  query:
    "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404",
  keyId: "AKIAIOSFODNN7EXAMPLE",
  secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
  t: Date.UTC(2013, 4, 24) / 1000,
};

test("SigV4 presigned URLs: AWS's example vector", async () => {
  const sig = await presignSignature("GET", AWS.host, AWS.path, AWS.query, AWS.secret, "20130524", "us-east-1", "20130524T000000Z", "20130524/us-east-1/s3/aws4_request");
  assert.equal(sig, "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404");
  const check = (over = {}) => verifyPresigned({ method: "GET", host: AWS.host, path: AWS.path, rawQuery: AWS.query, keyId: AWS.keyId, secret: AWS.secret, nowS: AWS.t + 3600, ...over });
  assert.deepEqual(await check(), { ok: true });
  assert.equal((await check({ nowS: AWS.t + 86400 })).ok, true, "valid through its last second");
  assert.equal((await check({ nowS: AWS.t + 86401 })).reason, "request has expired");
  assert.equal((await check({ nowS: AWS.t - 901 })).ok, false, "too far in the future");
  assert.equal((await check({ nowS: AWS.t - 900 })).ok, true, "15 minutes of clock skew");
  assert.equal((await check({ path: "/other.txt" })).ok, false, "another object");
  assert.equal((await check({ host: "evil.example" })).ok, false, "another host");
  assert.equal((await check({ method: "HEAD" })).ok, false, "signed for GET");
  assert.equal((await check({ rawQuery: AWS.query.replace("X-Amz-Expires=86400", "X-Amz-Expires=86401") })).ok, false, "tampered expiry");
  assert.equal((await check({ rawQuery: `${AWS.query}&versionId=x` })).ok, false, "added parameter");
  assert.equal((await check({ rawQuery: AWS.query.replace(/aeee/, "aeef") })).ok, false, "tampered signature");
  assert.equal((await check({ keyId: "RKOTHERKEY0000000000" })).reason, "unknown access key");
  assert.equal((await check({ secret: "wrong" })).ok, false);
  assert.equal((await check({ keyId: "", secret: "" })).ok, false, "no presigning key configured");
  const v4a = AWS.query.replace("AWS4-HMAC-SHA256", "AWS4-ECDSA-P256-SHA256");
  assert.equal((await check({ rawQuery: v4a })).ok, false);
});

test("canonical queries re-encode and sort like the origin", () => {
  assert.equal(canonicalQuery("b=2&a=x%20y&X-Amz-Signature=zz&a=%7e&c"), "a=x%20y&a=~&b=2&c=");
});

test("credentials in a request", () => {
  assert.deepEqual(credentialOf("", false), { kind: "none" });
  assert.deepEqual(credentialOf("?width=10", false), { kind: "none" });
  assert.deepEqual(credentialOf("?token=abc.def.ghi", false), { kind: "token", token: "abc.def.ghi" });
  assert.deepEqual(credentialOf("?X-Amz-Signature=1", false), { kind: "presigned" });
  assert.deepEqual(credentialOf("?x-amz-date=1&token=t", false), { kind: "presigned" });
  assert.equal(credentialOf("?AWSAccessKeyId=a&Signature=s&Expires=1", false).kind, "unsupported");
  assert.equal(credentialOf("", true).kind, "unsupported");
});

test("routes: parsing, longest prefix, paths", () => {
  assert.equal(parseRoutes(undefined), DEFAULT_ROUTES);
  const routes = parseRoutes(
    JSON.stringify([
      { prefix: "/", kind: "s3" },
      { prefix: "/v/", kind: "immutable" },
      { prefix: "/storage/v1/object/public/", kind: "bucket-key", auth: "public" },
      { prefix: "/storage/v1/object/sign/", kind: "bucket-key", auth: "token", forward_token: true, origin: "http://api:59001/" },
    ]),
  );
  assert.throws(() => parseRoutes('[{"prefix":"x"}]'));
  assert.throws(() => parseRoutes('[{"prefix":"/","kind":"ftp"}]'));
  assert.throws(() => parseRoutes("{}"));
  const sign = matchRoute(routes, "/storage/v1/object/sign/b/k");
  assert.equal(sign.kind, "bucket-key");
  assert.equal(sign.origin, "http://api:59001");
  assert.equal(sign.forward_token, true);
  assert.equal(matchRoute(routes, "/v/abc/b/k").kind, "immutable");
  assert.equal(matchRoute(routes, "/b/k").kind, "s3");
  assert.equal(matchRoute(parseRoutes('[{"prefix":"/x/"}]'), "/y"), undefined);

  assert.deepEqual(parsePath(routes[0], "/media/a%20b/c%2Bd.png"), { bucket: "media", key: "a b/c+d.png" });
  assert.deepEqual(parsePath(routes[0], "/media"), { bucket: "media", key: "" });
  assert.deepEqual(parsePath(routes[0], "/media/"), { bucket: "media", key: "" });
  assert.equal(parsePath(routes[0], "/"), null);
  assert.deepEqual(parsePath(routes[1], "/v/0123abc/media/x/y.js"), { bucket: "media", key: "x/y.js", version: "0123abc" });
  assert.equal(parsePath(routes[1], "/v/0123abc/media"), null);
  assert.equal(parsePath(routes[1], "/v/media"), null);
  assert.deepEqual(parsePath(routes[2], "/storage/v1/object/public/avatars/u/1.png"), { bucket: "avatars", key: "u/1.png" });

  // A bucket's website: paths are its keys, credentials mean nothing.
  const [site] = parseRoutes('[{"prefix":"/","kind":"website","bucket":"site","auth":"token"}]');
  assert.equal(site.auth, "public");
  assert.deepEqual(parsePath(site, "/docs/a%20b.html"), { bucket: "site", key: "docs/a b.html" });
  assert.deepEqual(parsePath(site, "/"), { bucket: "site", key: "" });
  assert.throws(() => parseRoutes('[{"prefix":"/","kind":"website"}]'));
  assert.throws(() => parseRoutes('[{"prefix":"/","kind":"s3","bucket":"x"}]'));
});

test("cache keys strip credentials and keep namespaces apart", () => {
  const s3 = { prefix: "/", kind: "s3", auth: "any" };
  const bk = { prefix: "/render/", kind: "bucket-key", auth: "any" };
  assert.equal(cacheKey(s3, "cdn.example:8443", "/b/k%20x", "?X-Amz-Signature=1&token=t"), "https://cdn.example:8443/b/k%20x");
  // However the client encoded the path, the key is in S3's encoding (what roll purges).
  assert.equal(cacheKey(s3, "cdn", "/b/a+b%2bc%7E!", ""), "https://cdn/b/a%2Bb%2Bc~%21");
  assert.equal(cacheKey(bk, "cdn", "/render/b/k", "?width=10&token=t&X-AMZ-Date=1&roll-cdn-ns=private&height=5"), "https://cdn/render/b/k?height=5&width=10");
  assert.equal(cacheKey(bk, "cdn", "/render/b/k", "?roll-cdn-ns=private"), "https://cdn/render/b/k");
  assert.equal(cacheKey(bk, "cdn", "/render/b/k", "?a=%7e&a=1"), "https://cdn/render/b/k?a=1&a=~");
  assert.equal(privateKey("https://cdn/b/k"), "https://cdn/b/k?roll-cdn-ns=private");
  assert.equal(privateKey("https://cdn/render/b/k?w=1"), "https://cdn/render/b/k?w=1&roll-cdn-ns=private");
});

test("cache tags", async () => {
  const tags = await cacheTags("media", "a b/c.png");
  assert.equal(tags, `rk-${(await sha256Hex("media/a b/c.png")).slice(0, 32)},rb-${(await sha256Hex("media")).slice(0, 32)}`);
  assert.match(tags, /^rk-[0-9a-f]{32},rb-[0-9a-f]{32}$/);
  // Known value: SHA-256("abc").
  assert.equal(await sha256Hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
});

test("ranges", () => {
  assert.deepEqual(parseRange("bytes=0-9", 100), { start: 0, end: 9 });
  assert.deepEqual(parseRange("bytes=90-", 100), { start: 90, end: 99 });
  assert.deepEqual(parseRange("bytes=-10", 100), { start: 90, end: 99 });
  assert.deepEqual(parseRange("bytes=50-1000", 100), { start: 50, end: 99 });
  assert.equal(parseRange("bytes=100-", 100), "unsatisfiable");
  assert.equal(parseRange("bytes=-0", 100), "unsatisfiable");
  assert.equal(parseRange("bytes=0-1,5-6", 100), null, "multiple ranges: the whole object");
  assert.equal(parseRange(null, 100), null);
});
