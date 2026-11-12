// Request shapes of every `/v1/nosql` route (crates/nosql-control/src/api.rs) and the pure
// helpers of lib/nosql-api.ts. Run with `pnpm --filter dashboard test`.

import assert from "node:assert/strict";
import { afterEach, beforeEach, describe, test } from "node:test";
import {
  branchTree,
  cqlUrl,
  descendants,
  DRIVERS,
  formatCountdown,
  formatSpan,
  hostPort,
  isCqlError,
  importSourceKind,
  maskCqlUrl,
  nosqlApi,
  parseObject,
  settingsPatch,
  snippet,
  validLabel,
  validName,
  type CqlResult,
  type NosqlDb,
} from "../src/lib/nosql-api";
import { ROLL_API } from "../src/lib/roll-api";

interface Seen {
  method: string;
  url: string;
  body: unknown;
  contentType: string | null;
}

let seen: Seen[] = [];
let reply: { status: number; body: unknown } = { status: 200, body: {} };
const realFetch = globalThis.fetch;

beforeEach(() => {
  seen = [];
  reply = { status: 200, body: {} };
  globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
    const headers = new Headers(init?.headers);
    seen.push({
      method: init?.method ?? "GET",
      url: String(input),
      body: typeof init?.body === "string" ? JSON.parse(init.body) : undefined,
      contentType: headers.get("content-type"),
    });
    assert.equal(init?.cache, "no-store", "control-plane calls are never cached");
    return new Response(reply.body === undefined ? "" : JSON.stringify(reply.body), { status: reply.status });
  }) as typeof fetch;
});

afterEach(() => {
  globalThis.fetch = realFetch;
});

const last = () => {
  assert.equal(seen.length, 1, "exactly one request");
  const s = seen[0];
  return { ...s, path: s.url.slice(ROLL_API.length) };
};

describe("routes", () => {
  const cases: [string, () => Promise<unknown>, string, string, unknown][] = [
    ["info", () => nosqlApi.info(), "GET", "/v1/nosql", undefined],
    ["list", () => nosqlApi.list(), "GET", "/v1/nosql/databases", undefined],
    ["get", () => nosqlApi.get("nk3v9q2m7xa1"), "GET", "/v1/nosql/databases/nk3v9q2m7xa1", undefined],
    [
      "create",
      () => nosqlApi.create({ name: "app-prod", dialect: "cassandra", datacenter: "us-east-1", rack: "r1", cluster_name: "Prod" }),
      "POST",
      "/v1/nosql/databases",
      { name: "app-prod", dialect: "cassandra", datacenter: "us-east-1", rack: "r1", cluster_name: "Prod" },
    ],
    [
      "update",
      () => nosqlApi.update("n1", { durability: "sync", settings: { tombstone_warn_threshold: null }, expires_at: null }),
      "PATCH",
      "/v1/nosql/databases/n1",
      { durability: "sync", settings: { tombstone_warn_threshold: null }, expires_at: null },
    ],
    ["remove", () => nosqlApi.remove("n1"), "DELETE", "/v1/nosql/databases/n1", undefined],
    [
      "branch",
      () => nosqlApi.branch("n1", { name: "pr-482", at: 1_800_000_000, schema_only: true, expires_at: 1_900_000_000, read_only: true }),
      "POST",
      "/v1/nosql/databases/n1/branches",
      { name: "pr-482", at: 1_800_000_000, schema_only: true, expires_at: 1_900_000_000, read_only: true },
    ],
    ["reset latest", () => nosqlApi.reset("n1"), "POST", "/v1/nosql/databases/n1/reset", {}],
    ["reset at", () => nosqlApi.reset("n1", 1_800_000_000), "POST", "/v1/nosql/databases/n1/reset", { at: 1_800_000_000 }],
    ["restore", () => nosqlApi.restore("n1", 1_800_000_000), "POST", "/v1/nosql/databases/n1/restore", { at: 1_800_000_000 }],
    ["diff", () => nosqlApi.diff("n1"), "GET", "/v1/nosql/databases/n1/diff", undefined],
    ["imports", () => nosqlApi.imports("n1"), "GET", "/v1/nosql/databases/n1/imports", undefined],
    [
      "start import",
      () => nosqlApi.startImport("n1", { from: "cql://reader:pw@prod:9042", keyspaces: ["app", "events"], every: 86_400 }),
      "POST",
      "/v1/nosql/databases/n1/imports",
      { from: "cql://reader:pw@prod:9042", keyspaces: ["app", "events"], every: 86_400 },
    ],
    ["rotate", () => nosqlApi.rotatePassword("n1"), "POST", "/v1/nosql/databases/n1/password", {}],
    [
      "cql",
      () => nosqlApi.cql("n1", { query: "SELECT * FROM app.t", keyspace: "app", page_state: "AAE=", page_size: 50, trace: true }),
      "POST",
      "/v1/nosql/databases/n1/cql",
      { query: "SELECT * FROM app.t", keyspace: "app", page_state: "AAE=", page_size: 50, trace: true },
    ],
    ["schema", () => nosqlApi.schema("n1"), "GET", "/v1/nosql/databases/n1/schema", undefined],
    ["usage default", () => nosqlApi.usage("n1"), "GET", "/v1/nosql/databases/n1/usage?days=30", undefined],
    ["usage 7d", () => nosqlApi.usage("n1", 7), "GET", "/v1/nosql/databases/n1/usage?days=7", undefined],
    ["activity", () => nosqlApi.activity("n1"), "GET", "/v1/nosql/databases/n1/activity", undefined],
    ["flush", () => nosqlApi.flush("n1"), "POST", "/v1/nosql/databases/n1/flush", {}],
    ["compact", () => nosqlApi.compact("n1"), "POST", "/v1/nosql/databases/n1/compact", {}],
    ["unload", () => nosqlApi.unload("n1"), "POST", "/v1/nosql/databases/n1/unload", {}],
  ];

  for (const [name, run, method, path, body] of cases) {
    test(name, async () => {
      await run();
      const r = last();
      assert.equal(r.method, method);
      assert.equal(r.path, path);
      assert.deepEqual(r.body, body);
      assert.equal(r.contentType, body === undefined ? null : "application/json");
    });
  }

  test("ids are path-encoded", async () => {
    await nosqlApi.get("a/b c");
    assert.equal(last().path, "/v1/nosql/databases/a%2Fb%20c");
  });

  test("API errors throw their message", async () => {
    reply = { status: 400, body: { error: "unsupported backup URL \"s4://x\" (s3://bucket/path or file:///path)" } };
    await assert.rejects(nosqlApi.startImport("n1", { from: "s4://x" }), /unsupported backup URL/);
    seen = [];
    reply = { status: 409, body: { error: "root is not a branch" } };
    await assert.rejects(nosqlApi.diff("root"), /root is not a branch/);
    seen = [];
    reply = { status: 502, body: undefined };
    await assert.rejects(nosqlApi.list(), /502/);
  });

  test("import sources", () => {
    assert.equal(importSourceKind("cql://u:p@h:9042"), "live");
    assert.equal(importSourceKind(" s3://bucket/backups?tag=nightly "), "backup");
    assert.equal(importSourceKind("file:///var/backups"), "backup");
    assert.equal(importSourceKind("s3://"), null);
    assert.equal(importSourceKind("http://x"), null);
  });

  test("CQL errors are results, not exceptions", async () => {
    reply = { status: 200, body: { error: "line 1:0 no viable alternative at input 'SELEC'", code: 8192, code_name: "SYNTAX_ERROR", warnings: [] } };
    const r = (await nosqlApi.cql("n1", { query: "SELEC 1" })) as CqlResult;
    assert.ok(isCqlError(r));
    assert.equal(r.code_name, "SYNTAX_ERROR");
    assert.ok(!isCqlError({ kind: "void", warnings: [] }));
  });
});

// ---------------------------------------------------------------------------------------------

function sample(over: Partial<NosqlDb> = {}): NosqlDb {
  return {
    id: "nabc",
    name: "app-prod",
    dialect: "scylla",
    datacenter: "us-east-1",
    rack: "rack1",
    cluster_name: "app-prod",
    settings: {},
    durability: "async",
    idle_timeout: 300,
    history_retention: 604_800,
    read_only: false,
    masking: {},
    expires_at: null,
    parent_id: null,
    parent_at: null,
    schema_only: false,
    created_at: 1_700_000_000,
    state: "idle",
    endpoints: { cql: "127.0.0.1:59042", cql_shard_aware: "127.0.0.1:59043", alternator: null, rest: "http://127.0.0.1:59100" },
    tls: false,
    server_name: null,
    credentials: { username: "cassandra@nabc", password: "p@ss/word", role: "cassandra" },
    http_token: "nabc_pw",
    storage_bytes: null,
    connections: 0,
    num_tokens: 256,
    host_id: null,
    status: {
      loaded: false,
      users: null,
      idle_ms: null,
      metrics: { connections: 0, connections_total: 0, connection_ms: 0, requests: 0, errors: 0, bytes_in: 0, bytes_out: 0, http_requests: 0, loads: 0 },
    },
    ...over,
  };
}

describe("connection details", () => {
  test("host and port", () => {
    assert.deepEqual(hostPort(sample()), ["127.0.0.1", 59042]);
    assert.deepEqual(hostPort(sample({ endpoints: { ...sample().endpoints, cql: "[::1]:9042" } })), ["::1", 9042]);
    assert.deepEqual(hostPort(sample({ endpoints: { ...sample().endpoints, cql: "nabc.nosql.example.com" } })), ["nabc.nosql.example.com", 9042]);
  });

  test("cql:// URL encodes credentials and marks TLS", () => {
    assert.equal(cqlUrl(sample()), "cql://cassandra%40nabc:p%40ss%2Fword@127.0.0.1:59042");
    assert.equal(cqlUrl(sample({ tls: true })), "cql://cassandra%40nabc:p%40ss%2Fword@127.0.0.1:59042?ssl=true");
    assert.equal(cqlUrl(sample({ endpoints: { ...sample().endpoints, cql: "[::1]:9042" } })), "cql://cassandra%40nabc:p%40ss%2Fword@[::1]:9042");
    assert.equal(maskCqlUrl("cql://reader:secret@prod:9042"), "cql://reader:••••••••@prod:9042");
    assert.equal(maskCqlUrl("cql://prod:9042"), "cql://prod:9042");
  });

  test("snippets carry the datacenter, credentials and TLS/SNI", () => {
    const d = sample();
    for (const drv of DRIVERS) {
      const s = snippet(d, drv);
      assert.ok(s.includes("127.0.0.1"), drv);
      assert.ok(s.includes("cassandra@nabc"), drv);
      assert.ok(s.includes("p@ss/word"), drv);
      if (drv !== "cqlsh") assert.ok(s.includes("us-east-1"), drv);
    }
    // Same text as `roll nosql connect --driver node` (crates/roll-cli/src/nosql/mod.rs).
    assert.equal(
      snippet(d, "node"),
      "const client = new cassandra.Client({\n  contactPoints: ['127.0.0.1:59042'],\n  localDataCenter: 'us-east-1',\n  credentials: { username: 'cassandra@nabc', password: 'p@ss/word' },\n});\n",
    );
    const hosted = sample({ tls: true, server_name: "nabc.nosql.example.com" });
    assert.ok(snippet(hosted, "python").includes(`"server_hostname": "nabc.nosql.example.com"`));
    assert.ok(snippet(hosted, "go").includes(`ServerName: "nabc.nosql.example.com"`));
    assert.ok(snippet(hosted, "node").includes("servername: 'nabc.nosql.example.com'"));
    assert.ok(snippet(hosted, "java").includes("withSslContext"));
    assert.ok(snippet(hosted, "cqlsh").includes("--ssl"));
    assert.ok(!snippet(d, "cqlsh").includes("--ssl"));
    assert.ok(snippet(d, "cqlsh").includes("roll nosql cqlsh app-prod"));
    // Masked secrets for display.
    assert.ok(!snippet(d, "rust", "••••").includes("p@ss/word"));
  });
});

describe("branch tree", () => {
  const root = sample({ id: "r", name: "prod" });
  const a = sample({ id: "a", name: "pr-2", parent_id: "r" });
  const b = sample({ id: "b", name: "pr-1", parent_id: "r" });
  const aa = sample({ id: "aa", name: "pr-2-fix", parent_id: "a" });
  const orphan = sample({ id: "o", name: "old", parent_id: "gone" });
  const other = sample({ id: "x", name: "analytics" });

  test("branches nest under parents, sorted by name; orphans are roots", () => {
    const rows = branchTree([aa, a, orphan, root, b, other]);
    assert.deepEqual(
      rows.map((r) => [r.db.id, r.depth, r.orphanOf]),
      [
        ["x", 0, null],
        ["o", 0, "gone"],
        ["r", 0, null],
        ["b", 1, null],
        ["a", 1, null],
        ["aa", 2, null],
      ],
    );
  });

  test("descendants include branches of branches", () => {
    assert.deepEqual(
      descendants([root, a, b, aa, other], "r").map((d) => d.id),
      ["a", "b", "aa"],
    );
    assert.deepEqual(descendants([root], "r"), []);
  });
});

describe("helpers", () => {
  test("names and labels", () => {
    assert.ok(validName("pr-482"));
    assert.ok(!validName("PR"));
    assert.ok(!validName(""));
    assert.ok(!validName("a".repeat(41)));
    assert.ok(!validName("a_b"));
    assert.ok(validLabel("us-east-1"));
    assert.ok(validLabel("Prod Cluster"));
    assert.ok(!validLabel("a'b"));
    assert.ok(!validLabel('a"b'));
    assert.ok(!validLabel(""));
    assert.ok(validLabel("", true));
    assert.ok(!validLabel("x".repeat(65)));
  });

  test("countdowns and spans", () => {
    assert.equal(formatCountdown(100, 100), null);
    assert.equal(formatCountdown(145, 100), "45s");
    assert.equal(formatCountdown(100 + 125, 100), "2m 5s");
    assert.equal(formatCountdown(100 + 3 * 3600 + 12 * 60, 100), "3h 12m");
    assert.equal(formatCountdown(100 + 2 * 86_400 + 4 * 3600 + 5, 100), "2d 4h");
    assert.equal(formatSpan(300), "5m");
    assert.equal(formatSpan(604_800), "7d");
    assert.equal(formatSpan(0), "0s");
    assert.equal(formatSpan(90), "90s");
  });

  test("settings patch: changes, additions and removals (null)", () => {
    assert.deepEqual(settingsPatch({ a: 1, b: 2, c: "x" }, { a: 1, b: 3, d: true }), { b: 3, d: true, c: null });
    assert.deepEqual(settingsPatch({ a: { x: 1 } }, { a: { x: 1 } }), {});
  });

  test("parseObject", () => {
    assert.deepEqual(parseObject(""), { value: {} });
    assert.deepEqual(parseObject('{"tombstone_warn_threshold": 50}'), { value: { tombstone_warn_threshold: 50 } });
    assert.ok("error" in parseObject("[1]"));
    assert.ok("error" in parseObject("null"));
    assert.ok("error" in parseObject("{nope"));
  });
});
