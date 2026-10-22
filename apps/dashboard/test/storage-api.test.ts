// The storage control-plane client (src/lib/storage-api.ts) against the contract in
// docs/storage/architecture.md, with `fetch` and `XMLHttpRequest` stubbed.

import assert from "node:assert/strict";
import { afterEach, beforeEach, describe, it } from "node:test";
import { ROLL_API } from "../src/lib/roll-api";
import {
  PERMISSIONS,
  RECOVER_OPTIONS,
  agoMs,
  baseName,
  bucketNameError,
  bucketTagsError,
  formatDate,
  inMs,
  isLocked,
  lifecycleExtras,
  lifecycleHasAction,
  normalizeLifecycleRule,
  retentionLabel,
  storageApi,
  uploadObject,
  usd,
  versioningLabel,
  type Bucket,
  type DeletedObject,
  type LifecycleRule,
  type ObjectInfo,
  type TrashEntry,
  type VersionList,
} from "../src/lib/storage-api";

interface Call {
  method: string;
  /** The URL exactly as requested. */
  raw: string;
  url: URL;
  headers: Record<string, string>;
  /** The parsed JSON body, or `undefined` when there was none. */
  body: unknown;
  init: RequestInit;
}

type Reply = Response | (() => Response | Promise<Response>);

const json = (v: unknown, status = 200, statusText = "") =>
  new Response(JSON.stringify(v), { status, statusText, headers: { "content-type": "application/json" } });

const realFetch = globalThis.fetch;
let calls: Call[] = [];
let reply: (c: Call) => Reply = () => json({});

beforeEach(() => {
  calls = [];
  reply = () => json({});
  globalThis.fetch = (async (input: string | URL | Request, init: RequestInit = {}) => {
    const raw = String(input);
    const c: Call = {
      method: init.method ?? "GET",
      raw,
      url: new URL(raw),
      headers: { ...((init.headers as Record<string, string> | undefined) ?? {}) },
      body: typeof init.body === "string" ? JSON.parse(init.body) : init.body == null ? undefined : init.body,
      init,
    };
    calls.push(c);
    const r = reply(c);
    return typeof r === "function" ? r() : r;
  }) as typeof fetch;
});

afterEach(() => {
  globalThis.fetch = realFetch;
});

/** Run `fn`, check it made exactly one request, and return it. */
async function one<T>(fn: () => Promise<T>, respond?: unknown): Promise<{ call: Call; result: T }> {
  if (respond !== undefined) reply = () => json(respond);
  const result = await fn();
  assert.equal(calls.length, 1, `expected one request, got ${calls.map((c) => `${c.method} ${c.raw}`).join(", ")}`);
  return { call: calls[0], result };
}

/** Method, path (still percent-encoded), raw query string and JSON body of one request. */
async function expectRequest(
  fn: () => Promise<unknown>,
  method: string,
  path: string,
  query: string,
  body?: unknown,
): Promise<Call> {
  const { call } = await one(fn);
  assert.equal(call.method, method);
  assert.equal(call.url.origin, new URL(ROLL_API).origin);
  assert.ok(call.raw.startsWith(`${ROLL_API}/v1/storage`), call.raw);
  assert.equal(call.url.pathname, path);
  assert.equal(call.url.search, query);
  assert.deepEqual(call.body, body);
  if (body === undefined) {
    assert.equal(call.init.body, undefined);
    assert.equal(call.headers["content-type"], undefined);
  } else {
    assert.equal(call.headers["content-type"], "application/json");
  }
  // Always fresh data, never Next's fetch cache.
  assert.equal(call.init.cache, "no-store");
  return call;
}

/** Keys that need encoding in a query string, and how a form decoder (axum's `Query`) reads them back. */
const ODD_KEYS = [
  "plain.txt",
  "dir/sub/file.txt",
  "with space.txt",
  "a+b.txt",
  "100% real?.txt",
  "x&y=z#frag",
  "ünïcødé/日本語/😀.png",
  " leading and trailing ",
  "already%20encoded",
];

describe("ROLL_API", () => {
  it("defaults to the local rolld", () => {
    assert.equal(ROLL_API, process.env.NEXT_PUBLIC_ROLL_API ?? "http://127.0.0.1:7070");
  });
});

describe("storageApi requests", () => {
  it("info", async () => {
    const info = { endpoint: "http://127.0.0.1:59000", region: "us-east-1" };
    reply = () => json(info);
    await expectRequest(() => storageApi.info(), "GET", "/v1/storage", "");
    assert.deepEqual(await storageApi.info(), info);
  });

  it("buckets and bucket", async () => {
    await expectRequest(() => storageApi.buckets(), "GET", "/v1/storage/buckets", "");
    calls = [];
    await expectRequest(() => storageApi.bucket("my.bucket-1"), "GET", "/v1/storage/buckets/my.bucket-1", "");
    calls = [];
    // Names are path segments: anything odd is encoded, never a new segment.
    await expectRequest(() => storageApi.bucket("a/b c"), "GET", "/v1/storage/buckets/a%2Fb%20c", "");
  });

  it("createBucket", async () => {
    await expectRequest(() => storageApi.createBucket("photos"), "POST", "/v1/storage/buckets", "", { name: "photos" });
    calls = [];
    await expectRequest(
      () => storageApi.createBucket("assets", { public: true, recover_days: 0 }),
      "POST",
      "/v1/storage/buckets",
      "",
      { name: "assets", public: true, recover_days: 0 },
    );
    calls = [];
    await expectRequest(
      () => storageApi.createBucket("logs", { recover_days: 30 }),
      "POST",
      "/v1/storage/buckets",
      "",
      { name: "logs", recover_days: 30 },
    );
    calls = [];
    await expectRequest(
      () => storageApi.createBucket("vault", { object_lock: true }),
      "POST",
      "/v1/storage/buckets",
      "",
      { name: "vault", object_lock: true },
    );
  });

  it("updateBucket sends exactly the patch", async () => {
    const cors = [
      {
        allowed_origins: ["https://app.example"],
        allowed_methods: ["GET", "PUT"],
        allowed_headers: ["*"],
        expose_headers: ["ETag"],
        max_age_s: 600,
      },
    ];
    const lifecycle = [{ id: "tmp", prefix: "tmp/", enabled: true, expire_days: 3, abort_multipart_days: null }];
    await expectRequest(
      () => storageApi.updateBucket("site", { public: false, recover_days: 14, cors, lifecycle }),
      "PATCH",
      "/v1/storage/buckets/site",
      "",
      { public: false, recover_days: 14, cors, lifecycle },
    );
    calls = [];
    // An empty list removes a setting, so it must be sent; omitted settings stay omitted.
    await expectRequest(() => storageApi.updateBucket("site", { cors: [] }), "PATCH", "/v1/storage/buckets/site", "", { cors: [] });
    calls = [];
    await expectRequest(() => storageApi.updateBucket("site", { versioning: "Enabled" }), "PATCH", "/v1/storage/buckets/site", "", {
      versioning: "Enabled",
    });
    calls = [];
    await expectRequest(
      () => storageApi.updateBucket("site", { versioning: "Suspended", tags: { team: "web" } }),
      "PATCH",
      "/v1/storage/buckets/site",
      "",
      { versioning: "Suspended", tags: { team: "web" } },
    );
    calls = [];
    // `{}` removes every tag: it must be sent as is.
    await expectRequest(() => storageApi.updateBucket("site", { tags: {} }), "PATCH", "/v1/storage/buckets/site", "", { tags: {} });
  });

  it("updateBucket sends the full lifecycle model", async () => {
    const lifecycle: LifecycleRule[] = [
      { id: "old", prefix: "", enabled: true, expire_days: 30, abort_multipart_days: 7 },
      {
        id: "everything",
        prefix: "logs/",
        enabled: false,
        expire_days: null,
        abort_multipart_days: null,
        tags: { tier: "cold" },
        size_greater_than: 1024,
        size_less_than: null,
        expire_at: 1_798_761_600_000,
        expire_delete_markers: true,
        noncurrent_days: 10,
        newer_noncurrent_versions: 2,
        transitions: [{ days: 30, date: null, storage_class: "GLACIER" }],
        noncurrent_transitions: [{ noncurrent_days: 5, newer_noncurrent_versions: null, storage_class: "STANDARD_IA" }],
        legacy_prefix: false,
      },
    ];
    await expectRequest(() => storageApi.updateBucket("logs", { lifecycle }), "PATCH", "/v1/storage/buckets/logs", "", { lifecycle });
  });

  it("versions: only the given parameters", async () => {
    const page: VersionList = {
      versions: [
        { key: "k", version_id: "7ffffe6b2c3d4e5fabcdefghijklmnop", is_latest: true, delete_marker: true, size: 0, etag: "", last_modified: 2 },
        { key: "k", version_id: "null", is_latest: false, delete_marker: false, size: 3, etag: "abc", last_modified: 1 },
      ],
      next_cursor: "WyJrIiwibnVsbCJd",
    };
    const { result } = await one(() => storageApi.versions("ver"), page);
    assert.deepEqual(result, page);
    assert.equal(calls[0].url.pathname, "/v1/storage/buckets/ver/versions");
    assert.equal(calls[0].url.search, "");
    calls = [];
    await expectRequest(() => storageApi.versions("ver", { prefix: "", cursor: null }), "GET", "/v1/storage/buckets/ver/versions", "");
    calls = [];
    const c = await expectRequest(
      () => storageApi.versions("ver", { prefix: "dir/a b", cursor: "WyJrIiwibnVsbCJd", limit: 200 }),
      "GET",
      "/v1/storage/buckets/ver/versions",
      "?prefix=dir%2Fa+b&cursor=WyJrIiwibnVsbCJd&limit=200",
    );
    assert.deepEqual(Object.fromEntries(c.url.searchParams), { prefix: "dir/a b", cursor: "WyJrIiwibnVsbCJd", limit: "200" });
  });

  it("object, deleteObject and downloadUrl take a version id", async () => {
    const vid = "7ffffe6b2c3d4e5fabcdefghijklmnop";
    const info = await expectRequest(() => storageApi.object("ver", "a b+c", vid), "GET", "/v1/storage/buckets/ver/object", `?key=a+b%2Bc&version_id=${vid}`);
    assert.deepEqual(Object.fromEntries(info.url.searchParams), { key: "a b+c", version_id: vid });
    calls = [];
    const del = await expectRequest(() => storageApi.deleteObject("ver", "k", "null"), "DELETE", "/v1/storage/buckets/ver/object", "?key=k&version_id=null");
    assert.equal(del.url.searchParams.get("version_id"), "null");
    calls = [];
    // No version id (or null): the current version, and no `version_id` parameter at all.
    for (const v of [undefined, null, ""]) {
      await expectRequest(() => storageApi.object("ver", "k", v), "GET", "/v1/storage/buckets/ver/object", "?key=k");
      calls = [];
      await expectRequest(() => storageApi.deleteObject("ver", "k", v), "DELETE", "/v1/storage/buckets/ver/object", "?key=k");
      calls = [];
      assert.equal(storageApi.downloadUrl("ver", "k", v), `${ROLL_API}/v1/storage/buckets/ver/download?key=k`);
    }
    const url = new URL(storageApi.downloadUrl("ver", "dir/x #1", vid));
    assert.equal(`${url.origin}${url.pathname}`, `${ROLL_API}/v1/storage/buckets/ver/download`);
    assert.deepEqual(Object.fromEntries(url.searchParams), { key: "dir/x #1", version_id: vid });
    assert.equal(url.hash, "");
  });

  it("deleteObject returns what was deleted", async () => {
    const marker: DeletedObject = { deleted: "k", version_id: "7ffffe6b2c3d4e5fabcdefghijklmnop", delete_marker: true };
    assert.deepEqual((await one(() => storageApi.deleteObject("ver", "k"), marker)).result, marker);
    calls = [];
    const plain: DeletedObject = { deleted: "k" };
    assert.deepEqual((await one(() => storageApi.deleteObject("plain", "k"), plain)).result, plain);
  });

  it("deleteBucket", async () => {
    await expectRequest(() => storageApi.deleteBucket("old"), "DELETE", "/v1/storage/buckets/old", "");
    calls = [];
    await expectRequest(() => storageApi.deleteBucket("old", false), "DELETE", "/v1/storage/buckets/old", "");
    calls = [];
    await expectRequest(() => storageApi.deleteBucket("old", true), "DELETE", "/v1/storage/buckets/old", "?force=true");
  });

  it("branch", async () => {
    await expectRequest(() => storageApi.branch("main", "preview-1"), "POST", "/v1/storage/buckets/main/branches", "", {
      name: "preview-1",
    });
  });

  it("objects: only the given parameters", async () => {
    await expectRequest(() => storageApi.objects("files"), "GET", "/v1/storage/buckets/files/objects", "");
    calls = [];
    // Empty and null values are left out (the server's defaults).
    await expectRequest(
      () => storageApi.objects("files", { prefix: "", cursor: null }),
      "GET",
      "/v1/storage/buckets/files/objects",
      "",
    );
    calls = [];
    const c = await expectRequest(
      () => storageApi.objects("files", { prefix: "dir/a b+c/", delimiter: "/", cursor: "Y3Vyc29y==", limit: 200 }),
      "GET",
      "/v1/storage/buckets/files/objects",
      "?prefix=dir%2Fa+b%2Bc%2F&delimiter=%2F&cursor=Y3Vyc29y%3D%3D&limit=200",
    );
    assert.deepEqual(Object.fromEntries(c.url.searchParams), {
      prefix: "dir/a b+c/",
      delimiter: "/",
      cursor: "Y3Vyc29y==",
      limit: "200",
    });
    calls = [];
    // limit 0 is a value, not "absent".
    const z = await expectRequest(() => storageApi.objects("files", { limit: 0 }), "GET", "/v1/storage/buckets/files/objects", "?limit=0");
    assert.equal(z.url.searchParams.get("limit"), "0");
  });

  for (const key of ODD_KEYS) {
    it(`object, deleteObject and downloadUrl encode the key ${JSON.stringify(key)}`, async () => {
      const info = await expectRequest(() => storageApi.object("b-1", key), "GET", "/v1/storage/buckets/b-1/object", keyQuery(key));
      assert.equal(info.url.searchParams.get("key"), key);
      assert.deepEqual([...info.url.searchParams.keys()], ["key"]);
      calls = [];
      const del = await expectRequest(() => storageApi.deleteObject("b-1", key), "DELETE", "/v1/storage/buckets/b-1/object", keyQuery(key));
      assert.equal(del.url.searchParams.get("key"), key);

      const url = new URL(storageApi.downloadUrl("b-1", key));
      assert.equal(`${url.origin}${url.pathname}`, `${ROLL_API}/v1/storage/buckets/b-1/download`);
      assert.equal(url.searchParams.get("key"), key);
      assert.equal(url.hash, "", "a # in the key must not become a fragment");
    });
  }

  it("encodes keys the way a form decoder reads them", async () => {
    await one(() => storageApi.object("b-1", "dir/a b+c ü.txt"));
    // Space as +, a literal + as %2B, / and non-ASCII percent-encoded.
    assert.equal(calls[0].url.search, "?key=dir%2Fa+b%2Bc+%C3%BC.txt");
    assert.equal(
      storageApi.downloadUrl("b-1", "x/1+1 = 2.txt"),
      `${ROLL_API}/v1/storage/buckets/b-1/download?key=x%2F1%2B1+%3D+2.txt`,
    );
  });

  it("deleteMany", async () => {
    await expectRequest(() => storageApi.deleteMany("bin", { keys: ["a", "b c", "d/e"] }), "POST", "/v1/storage/buckets/bin/delete", "", {
      keys: ["a", "b c", "d/e"],
    });
    calls = [];
    // `{ prefix: "" }` empties the bucket: the empty prefix must be sent, not dropped.
    await expectRequest(() => storageApi.deleteMany("bin", { prefix: "" }), "POST", "/v1/storage/buckets/bin/delete", "", { prefix: "" });
    calls = [];
    await expectRequest(() => storageApi.deleteMany("bin", { prefix: "logs/" }), "POST", "/v1/storage/buckets/bin/delete", "", {
      prefix: "logs/",
    });
  });

  it("presign", async () => {
    await expectRequest(() => storageApi.presign("share", "dir/a b.txt"), "POST", "/v1/storage/buckets/share/presign", "", {
      key: "dir/a b.txt",
      method: "GET",
      expires_s: 3600,
    });
    calls = [];
    await expectRequest(() => storageApi.presign("share", "up.json", "PUT", 60), "POST", "/v1/storage/buckets/share/presign", "", {
      key: "up.json",
      method: "PUT",
      expires_s: 60,
    });
  });

  it("trash and restore", async () => {
    await expectRequest(() => storageApi.trash("bin"), "GET", "/v1/storage/buckets/bin/trash", "");
    calls = [];
    const c = await expectRequest(
      () => storageApi.trash("bin", { prefix: "logs/ü", cursor: "bG9ncy9h/1", limit: 50 }),
      "GET",
      "/v1/storage/buckets/bin/trash",
      "?prefix=logs%2F%C3%BC&cursor=bG9ncy9h%2F1&limit=50",
    );
    assert.equal(c.url.searchParams.get("prefix"), "logs/ü");
    calls = [];
    await expectRequest(() => storageApi.trash("bin", { prefix: "", cursor: null }), "GET", "/v1/storage/buckets/bin/trash", "");
    calls = [];
    await expectRequest(
      () => storageApi.restore("bin", "logs/a b", 1_760_968_980_123),
      "POST",
      "/v1/storage/buckets/bin/trash/restore",
      "",
      { key: "logs/a b", deleted_at: 1_760_968_980_123 },
    );
  });

  it("usage and usageAll", async () => {
    await expectRequest(() => storageApi.usage("metered"), "GET", "/v1/storage/buckets/metered/usage", "?days=30");
    calls = [];
    await expectRequest(() => storageApi.usage("metered", 7), "GET", "/v1/storage/buckets/metered/usage", "?days=7");
    calls = [];
    await expectRequest(() => storageApi.usageAll(), "GET", "/v1/storage/usage", "?days=30");
    calls = [];
    await expectRequest(() => storageApi.usageAll(90), "GET", "/v1/storage/usage", "?days=90");
  });

  it("keys, createKey and deleteKey", async () => {
    await expectRequest(() => storageApi.keys(), "GET", "/v1/storage/keys", "");
    calls = [];
    await expectRequest(() => storageApi.createKey("ci", "read", ["team-a", "team-b"]), "POST", "/v1/storage/keys", "", {
      name: "ci",
      permission: "read",
      buckets: ["team-a", "team-b"],
    });
    calls = [];
    // null = every bucket: `buckets` is left out, not sent as null or [].
    const c = await expectRequest(() => storageApi.createKey("root", "admin", null), "POST", "/v1/storage/keys", "", {
      name: "root",
      permission: "admin",
    });
    assert.ok(!("buckets" in (c.body as object)));
    calls = [];
    await expectRequest(() => storageApi.deleteKey("RKABCDEFGHIJKLMNOPQR"), "DELETE", "/v1/storage/keys/RKABCDEFGHIJKLMNOPQR", "");
    calls = [];
    await expectRequest(() => storageApi.deleteKey("odd/id?x"), "DELETE", "/v1/storage/keys/odd%2Fid%3Fx", "");
  });

  it("returns the response body", async () => {
    const created = { id: "RKX", name: "ci", permission: "write", buckets: null, created_at: 1, last_used_at: null, secret_access_key: "s" };
    const { result } = await one(() => storageApi.createKey("ci", "write", null), created);
    assert.deepEqual(result, created);
  });
});

/** The query string `URLSearchParams` produces for `?key=`. */
function keyQuery(key: string) {
  return `?${new URLSearchParams({ key })}`;
}

describe("errors", () => {
  it("a non-2xx {error} body becomes an Error with that message", async () => {
    for (const status of [400, 404, 409, 500, 503]) {
      reply = () => json({ error: `no bucket named nope (${status})` }, status);
      await assert.rejects(storageApi.bucket("nope"), (e: unknown) => {
        assert.ok(e instanceof Error);
        assert.equal(e.message, `no bucket named nope (${status})`);
        return true;
      });
    }
  });

  it("every function rejects on an API error", async () => {
    reply = () => json({ error: "storage unavailable: fenced" }, 503);
    const all: (() => Promise<unknown>)[] = [
      () => storageApi.info(),
      () => storageApi.buckets(),
      () => storageApi.bucket("b-1"),
      () => storageApi.createBucket("b-1"),
      () => storageApi.updateBucket("b-1", { public: true }),
      () => storageApi.deleteBucket("b-1", true),
      () => storageApi.branch("b-1", "b-2"),
      () => storageApi.objects("b-1"),
      () => storageApi.versions("b-1"),
      () => storageApi.object("b-1", "k"),
      () => storageApi.object("b-1", "k", "null"),
      () => storageApi.deleteObject("b-1", "k"),
      () => storageApi.deleteObject("b-1", "k", "null"),
      () => storageApi.deleteMany("b-1", { prefix: "" }),
      () => storageApi.presign("b-1", "k"),
      () => storageApi.trash("b-1"),
      () => storageApi.restore("b-1", "k", 1),
      () => storageApi.usage("b-1"),
      () => storageApi.usageAll(),
      () => storageApi.keys(),
      () => storageApi.createKey("k", "read", null),
      () => storageApi.deleteKey("RK"),
    ];
    for (const f of all) await assert.rejects(f(), { message: "storage unavailable: fenced" });
    assert.equal(calls.length, all.length);
  });

  it("without an {error} body, the status", async () => {
    reply = () => new Response("", { status: 500, statusText: "Internal Server Error" });
    await assert.rejects(storageApi.buckets(), { message: "500 Internal Server Error" });
    reply = () => json({ message: "not ours" }, 404, "Not Found");
    await assert.rejects(storageApi.buckets(), { message: "404 Not Found" });
    // A proxy's HTML error page: the status, not a JSON syntax error.
    reply = () => new Response("<html><h1>502 Bad Gateway</h1></html>", { status: 502, statusText: "Bad Gateway" });
    await assert.rejects(storageApi.buckets(), { message: "502 Bad Gateway" });
    reply = () => new Response("oops", { status: 500 });
    await assert.rejects(storageApi.buckets(), { message: "500" });
  });

  it("a 2xx that isn't JSON is an error, an empty one is null", async () => {
    reply = () => new Response("<html>dev server</html>", { status: 200 });
    await assert.rejects(storageApi.buckets(), /GET \/v1\/storage\/buckets: the response isn't JSON/);
    reply = () => new Response("", { status: 200 });
    assert.equal(await storageApi.buckets(), null);
  });

  it("a network failure rejects", async () => {
    reply = () => {
      throw new TypeError("fetch failed");
    };
    await assert.rejects(storageApi.info(), { name: "TypeError", message: "fetch failed" });
  });
});

// ── uploadObject ──

class FakeXHR {
  static all: FakeXHR[] = [];
  method = "";
  url = "";
  headers: Record<string, string> = {};
  sent: unknown = undefined;
  aborts = 0;
  status = 0;
  statusText = "";
  responseText = "";
  upload: { onprogress: ((e: { loaded: number; total: number; lengthComputable: boolean }) => void) | null } = { onprogress: null };
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onabort: (() => void) | null = null;

  constructor() {
    FakeXHR.all.push(this);
  }
  open(method: string, url: string) {
    this.method = method;
    this.url = url;
  }
  setRequestHeader(k: string, v: string) {
    this.headers[k.toLowerCase()] = v;
  }
  send(body: unknown) {
    this.sent = body;
  }
  abort() {
    this.aborts++;
    this.onabort?.();
  }

  progress(loaded: number, total: number, lengthComputable = true) {
    this.upload.onprogress?.({ loaded, total, lengthComputable });
  }
  respond(status: number, body: string, statusText = "") {
    this.status = status;
    this.statusText = statusText;
    this.responseText = body;
    this.onload?.();
  }
}

describe("uploadObject", () => {
  const g = globalThis as { XMLHttpRequest?: unknown };
  const real = g.XMLHttpRequest;
  beforeEach(() => {
    FakeXHR.all = [];
    g.XMLHttpRequest = FakeXHR;
  });
  afterEach(() => {
    g.XMLHttpRequest = real;
  });
  const last = () => {
    assert.equal(FakeXHR.all.length, 1);
    return FakeXHR.all[0];
  };

  it("PUTs the file to /upload?key= with its type, reporting progress", async () => {
    const file = new Blob(["hello world"], { type: "text/plain" });
    const progress: [number, number][] = [];
    const p = uploadObject("b-1", "dir/a b+c ü.txt", file, (l, t) => progress.push([l, t]));
    const x = last();
    assert.equal(x.method, "PUT");
    assert.equal(x.url, `${ROLL_API}/v1/storage/buckets/b-1/upload?key=dir%2Fa+b%2Bc+%C3%BC.txt`);
    assert.equal(new URL(x.url).searchParams.get("key"), "dir/a b+c ü.txt");
    assert.deepEqual(x.headers, { "content-type": "text/plain" });
    assert.equal(x.sent, file);

    x.progress(4, 11);
    x.progress(11, 11);
    // Without a computable length, the file's size is the total.
    x.progress(5, 0, false);
    const info = { key: "dir/a b+c ü.txt", size: 11, etag: "5eb63bbbe01eeed093cb22bb8f5acdc3" };
    x.respond(200, JSON.stringify(info));
    assert.deepEqual(await p, info);
    assert.deepEqual(progress, [
      [4, 11],
      [11, 11],
      [5, 11],
    ]);
  });

  it("sends no content-type for an untyped file", async () => {
    const p = uploadObject("b-1", "raw", new Blob([new Uint8Array([0, 1, 2])]), () => {});
    const x = last();
    assert.deepEqual(x.headers, {});
    x.respond(200, "{}");
    await p;
  });

  it("rejects with the API's {error}", async () => {
    const p = uploadObject("b-1", "k", new Blob(["x"]), () => {});
    last().respond(400, JSON.stringify({ error: "Content-Length is required" }), "Bad Request");
    await assert.rejects(p, { message: "Content-Length is required" });
  });

  it("rejects with the status when the body isn't {error}", async () => {
    const p = uploadObject("b-1", "k", new Blob(["x"]), () => {});
    last().respond(413, "<html>too large</html>", "Payload Too Large");
    await assert.rejects(p, { message: "413 Payload Too Large" });
    FakeXHR.all = [];
    const q = uploadObject("b-1", "k", new Blob(["x"]), () => {});
    last().respond(500, "", "Internal Server Error");
    await assert.rejects(q, { message: "500 Internal Server Error" });
  });

  it("rejects on a network error", async () => {
    const p = uploadObject("b-1", "k", new Blob(["x"]), () => {});
    last().onerror?.();
    await assert.rejects(p, { message: "upload failed: rolld unreachable" });
  });

  it("aborts through the signal", async () => {
    const ac = new AbortController();
    const p = uploadObject("b-1", "k", new Blob(["x"]), () => {}, ac.signal);
    const x = last();
    ac.abort();
    assert.equal(x.aborts, 1);
    await assert.rejects(p, { message: "upload cancelled" });
  });

  it("doesn't start with an already-aborted signal", async () => {
    const ac = new AbortController();
    ac.abort();
    await assert.rejects(uploadObject("b-1", "k", new Blob(["x"]), () => {}, ac.signal), { message: "upload cancelled" });
    assert.equal(FakeXHR.all.length, 0);
  });
});

// ── pure helpers ──

describe("bucketNameError agrees with crates/s3-store/src/validate.rs", () => {
  // Every rule of `bucket_name`, with names on both sides of it.
  const ok = [
    "abc",
    "a".repeat(63),
    "my-bucket",
    "a.b.c",
    "123",
    "a-.b", // only ".." is rejected
    "a--b",
    "1.2.3", // not four octets
    "1.2.3.4.5",
    "256.1.1.1", // not an IPv4 address: an octet is over 255
    "999.999.999.999",
    "01.2.3.4", // leading zeros don't parse as IPv4 either
    "1.2.3.04",
    "xn-abc",
    "axn--b",
    "sthree",
    "asthree-x",
    "amzn-s3-demo",
    "s3alias",
    "abc-s3",
    "a-s3aliasx",
    "mrap",
    "abc-mrap",
    "ol-s3",
    "x-s3",
    "table-s3",
  ];
  const bad = [
    "",
    "ab",
    "a".repeat(64),
    "ABC",
    "abC",
    "a_b",
    "a b",
    "a/b",
    "été",
    "-abc",
    "abc-",
    ".abc",
    "abc.",
    "a..b",
    "192.168.1.1",
    "0.0.0.0",
    "255.255.255.255",
    "xn--abc",
    "sthree-abc",
    "amzn-s3-demo-bucket",
    "abc-s3alias",
    "abc--ol-s3",
    "abc.mrap",
    "abc--x-s3",
    "abc--table-s3",
  ];
  for (const name of ok) it(`accepts ${JSON.stringify(name)}`, () => assert.equal(bucketNameError(name), null));
  for (const name of bad)
    it(`rejects ${JSON.stringify(name)}`, () => {
      const e = bucketNameError(name);
      assert.equal(typeof e, "string");
      assert.ok(e && e.length > 0);
    });

  it("explains which rule", () => {
    assert.match(bucketNameError("ab")!, /3–63/);
    assert.match(bucketNameError("Ab_c")!, /lowercase/);
    assert.match(bucketNameError("-abc")!, /Start and end/);
    assert.match(bucketNameError("a..b")!, /adjacent periods/);
    assert.match(bucketNameError("10.0.0.1")!, /IP address/);
    assert.match(bucketNameError("sthree-x")!, /sthree-/);
    assert.match(bucketNameError("x--table-s3")!, /--table-s3/);
  });
});

describe("helpers", () => {
  it("usd keeps fractions of a cent visible", () => {
    assert.equal(usd(0), "$0");
    assert.equal(usd(0.00001), "<$0.0001");
    assert.equal(usd(0.0001), "$0.0001");
    assert.equal(usd(0.0042), "$0.0042");
    assert.equal(usd(0.01), "$0.01");
    assert.equal(usd(0.5), "$0.50");
    assert.equal(usd(1234.5), "$1234.50");
  });

  it("agoMs and inMs", () => {
    const realNow = Date.now;
    const now = 1_760_968_980_000;
    Date.now = () => now;
    try {
      assert.equal(agoMs(now), "0s ago");
      assert.equal(agoMs(now + 5_000), "0s ago"); // clock skew: never negative
      assert.equal(agoMs(now - 59_999), "59s ago");
      assert.equal(agoMs(now - 60_000), "1m ago");
      assert.equal(agoMs(now - 3_599_000), "59m ago");
      assert.equal(agoMs(now - 3_600_000), "1h ago");
      assert.equal(agoMs(now - 86_399_000), "23h ago");
      assert.equal(agoMs(now - 86_400_000), "1d ago");
      assert.equal(agoMs(now - 30 * 86_400_000), "30d ago");

      assert.equal(inMs(now - 1_000), "1m"); // already past: at least 1m
      assert.equal(inMs(now + 30_000), "1m");
      assert.equal(inMs(now + 59 * 60_000), "59m");
      assert.equal(inMs(now + 3_600_000), "1h");
      assert.equal(inMs(now + 86_399_000), "23h");
      assert.equal(inMs(now + 7 * 86_400_000), "7d");
    } finally {
      Date.now = realNow;
    }
  });

  it("formatDate is a 24-hour date and time", () => {
    const d = new Date(2025, 9, 20, 14, 3); // local time
    assert.equal(formatDate(d.getTime()), "Oct 20, 2025, 14:03");
    assert.equal(formatDate(new Date(2026, 0, 2, 0, 5).getTime()), "Jan 2, 2026, 00:05");
  });

  it("baseName", () => {
    assert.equal(baseName("dir/a.txt", "dir/"), "a.txt");
    assert.equal(baseName("dir/sub/", "dir/"), "sub/");
    assert.equal(baseName("a.txt", ""), "a.txt");
    assert.equal(baseName("other/a.txt", "dir/"), "other/a.txt");
  });

  it("recover options and permissions match the server", () => {
    // recover_days is 0-30 (validate.rs MAX_RECOVER_DAYS); 7 is the default.
    assert.ok(RECOVER_OPTIONS.every((d) => Number.isInteger(d) && d >= 0 && d <= 30));
    assert.deepEqual([...RECOVER_OPTIONS].sort((a, b) => a - b), RECOVER_OPTIONS);
    assert.ok(RECOVER_OPTIONS.includes(7) && RECOVER_OPTIONS.includes(0) && RECOVER_OPTIONS.includes(30));
    assert.deepEqual(
      PERMISSIONS.map((p) => p.value),
      ["read", "write", "admin"],
    );
  });
});

describe("contract shapes (docs/storage/architecture.md)", () => {
  it("a versioned bucket, an object version and a trash entry", () => {
    // Shapes as the server sends them; these only need to compile and round-trip.
    const b: Bucket = {
      id: "bkt_000000000000",
      name: "ver",
      region: "us-east-1",
      public: false,
      recover_days: 7,
      created_at: 1,
      parent: null,
      branches: [],
      objects: 1,
      bytes: 4,
      trash_bytes: 0,
      versioning: "Enabled",
      object_lock: { mode: "GOVERNANCE", days: 30, years: null },
      versions: 2,
      version_bytes: 3,
      cors: [],
      lifecycle: [],
      endpoint: "http://127.0.0.1:59000",
      url: "http://127.0.0.1:59000/ver",
      virtual_host_url: null,
      tags: { team: "web" },
      website: {
        index_document: "index.html",
        error_document: null,
        redirect_all: null,
        routing_rules: [
          {
            condition: { key_prefix_equals: "docs/", http_error_code_returned_equals: null },
            redirect: { host_name: null, protocol: null, http_redirect_code: null, replace_key_prefix_with: "documents/", replace_key_with: null },
          },
        ],
      },
    };
    assert.equal(JSON.parse(JSON.stringify(b)).version_bytes, 3);
    const o: ObjectInfo = {
      key: "k",
      size: 4,
      etag: "e",
      content_type: null,
      last_modified: 1,
      metadata: {},
      tags: {},
      cache_control: null,
      content_disposition: null,
      content_encoding: null,
      content_language: null,
      expires: null,
      checksum: null,
      parts: 0,
      version_id: "null",
      lock: { mode: "COMPLIANCE", retain_until: 2, legal_hold: null },
    };
    assert.equal(o.lock?.mode, "COMPLIANCE");
    const t: TrashEntry = { key: "k", size: 1, etag: "e", deleted_at: 1, expires_at: 2, version_id: "null" };
    assert.equal(t.version_id, "null");
  });
});

describe("versioning helpers", () => {
  it("versioningLabel", () => {
    assert.equal(versioningLabel("Enabled"), "enabled");
    assert.equal(versioningLabel("Suspended"), "suspended");
    assert.equal(versioningLabel(null), "off");
  });

  it("retentionLabel", () => {
    assert.equal(retentionLabel(null), null);
    assert.equal(retentionLabel({ mode: null, days: null, years: null }), null);
    assert.equal(retentionLabel({ mode: "GOVERNANCE", days: 1, years: null }), "1 day, governance");
    assert.equal(retentionLabel({ mode: "COMPLIANCE", days: null, years: 7 }), "7 years, compliance");
    assert.equal(retentionLabel({ mode: "GOVERNANCE", days: null, years: null }), null);
  });

  it("isLocked", () => {
    const now = 1_000_000;
    assert.equal(isLocked(null, now), false);
    assert.equal(isLocked({ mode: null, retain_until: null, legal_hold: null }, now), false);
    assert.equal(isLocked({ mode: null, retain_until: null, legal_hold: true }, now), true);
    assert.equal(isLocked({ mode: null, retain_until: null, legal_hold: false }, now), false);
    assert.equal(isLocked({ mode: "GOVERNANCE", retain_until: now + 1, legal_hold: null }, now), true);
    assert.equal(isLocked({ mode: "COMPLIANCE", retain_until: now, legal_hold: false }, now), false, "a retention that ran out");
  });

  it("lifecycle rules: defaults, actions and what isn't edited", () => {
    const basic: LifecycleRule = { id: "a", prefix: "", enabled: true, expire_days: 3, abort_multipart_days: null };
    // A rule written without the optional fields equals the server's echo of it.
    const echoed = {
      ...basic,
      tags: {},
      size_greater_than: null,
      size_less_than: null,
      expire_at: null,
      expire_delete_markers: false,
      noncurrent_days: null,
      newer_noncurrent_versions: null,
      transitions: [],
      noncurrent_transitions: [],
    };
    assert.deepEqual(normalizeLifecycleRule(basic), normalizeLifecycleRule(echoed));
    assert.equal(normalizeLifecycleRule(echoed).legacy_prefix, false);
    assert.deepEqual(lifecycleExtras(basic), []);

    const none: LifecycleRule = { ...basic, expire_days: null };
    assert.equal(lifecycleHasAction(basic), true);
    assert.equal(lifecycleHasAction(none), false);
    for (const patch of [
      { noncurrent_days: 5 },
      { expire_delete_markers: true },
      { expire_at: 1_798_761_600_000 },
      { abort_multipart_days: 7 },
      { transitions: [{ days: 1, date: null, storage_class: "GLACIER" }] },
      { noncurrent_transitions: [{ noncurrent_days: 1, newer_noncurrent_versions: null, storage_class: "GLACIER" }] },
    ]) {
      assert.equal(lifecycleHasAction({ ...none, ...patch }), true, JSON.stringify(patch));
    }
    assert.deepEqual(
      lifecycleExtras({
        ...none,
        tags: { tier: "cold" },
        size_greater_than: 10,
        size_less_than: 20,
        expire_at: 1_798_761_600_000,
        newer_noncurrent_versions: 2,
        expire_delete_markers: true,
        transitions: [
          { days: 30, date: null, storage_class: "GLACIER" },
          { days: null, date: 1_798_761_600_000, storage_class: "DEEP_ARCHIVE" },
        ],
        noncurrent_transitions: [{ noncurrent_days: 5, newer_noncurrent_versions: null, storage_class: "STANDARD_IA" }],
      }),
      [
        "tag tier=cold",
        "size > 10 B",
        "size < 20 B",
        "expire on 2027-01-01",
        "keep newest 2 noncurrent",
        "remove lone delete markers",
        "to GLACIER after 30d",
        "to DEEP_ARCHIVE on 2027-01-01",
        "noncurrent to STANDARD_IA after 5d",
      ],
    );
  });

  it("bucketTagsError agrees with validate_bucket_tags", () => {
    assert.equal(bucketTagsError([]), null);
    assert.equal(bucketTagsError([["team", "web"], ["env", ""]]), null, "empty values are fine");
    assert.equal(bucketTagsError([["k".repeat(128), "v".repeat(256)]]), null);
    assert.equal(bucketTagsError([["ü".repeat(128), "😀".repeat(256)]]), null, "characters, not bytes or UTF-16 units");
    assert.equal(bucketTagsError(Array.from({ length: 50 }, (_, i): [string, string] => [`k${i}`, "v"])), null);
    assert.match(bucketTagsError(Array.from({ length: 51 }, (_, i): [string, string] => [`k${i}`, "v"])) ?? "", /50/);
    assert.match(bucketTagsError([["", "v"]]) ?? "", /key/);
    assert.match(bucketTagsError([["k".repeat(129), "v"]]) ?? "", /128/);
    assert.match(bucketTagsError([["k", "v".repeat(257)]]) ?? "", /256/);
    assert.match(bucketTagsError([["aws:x", "v"]]) ?? "", /aws:/);
    assert.match(bucketTagsError([["AWS:x", "v"]]) ?? "", /aws:/);
    assert.match(bucketTagsError([["a", "1"], ["a", "2"]]) ?? "", /Duplicate/);
  });
});
