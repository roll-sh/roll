// createClient and the roll extras against a fake fetch: headers, auth and error parsing.
import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { RollStorageClient, StorageApiError, StorageClient, StorageUnknownError, createClient, isStorageError } from "../src/index.js";

interface Call {
  url: string;
  method: string;
  headers: Headers;
  body: string | undefined;
}

function fake(respond: (c: Call) => Response = () => Response.json([])) {
  const calls: Call[] = [];
  const f: typeof fetch = async (input, init) => {
    const c = { url: String(input), method: init?.method ?? "GET", headers: new Headers(init?.headers), body: init?.body as string | undefined };
    calls.push(c);
    return respond(c);
  };
  return { calls, fetch: f };
}

const URL_ = "http://roll.test/storage/v1";

describe("createClient", () => {
  it("is a StorageClient that sends apikey and the key as the bearer token", async () => {
    const { calls, fetch } = fake();
    const client = createClient(URL_, "anon-key", { fetch });
    assert.ok(client instanceof StorageClient);
    assert.ok(client instanceof RollStorageClient);
    const { error } = await client.listBuckets();
    assert.equal(error, null);
    assert.equal(calls[0].url, `${URL_}/bucket`);
    assert.equal(calls[0].headers.get("apikey"), "anon-key");
    assert.equal(calls[0].headers.get("authorization"), "Bearer anon-key");
  });

  it("uses a static access token and extra headers", async () => {
    const { calls, fetch } = fake();
    const client = createClient(URL_, "anon-key", { fetch, accessToken: "user-jwt", headers: { "x-extra": "1" } });
    await client.from("b").list();
    assert.equal(calls[0].url, `${URL_}/object/list/b`);
    assert.equal(calls[0].headers.get("apikey"), "anon-key");
    assert.equal(calls[0].headers.get("authorization"), "Bearer user-jwt");
    assert.equal(calls[0].headers.get("x-extra"), "1");
  });

  it("resolves an async access token on every request, falling back to the key", async () => {
    const { calls, fetch } = fake(() => Response.json({ policy: null }));
    const tokens = ["first", null, "third"];
    let i = 0;
    const client = createClient(URL_, "anon-key", { fetch, accessToken: async () => tokens[i++] });
    await client.listBuckets();
    await client.from("b").list();
    await client.roll.getPolicy("b");
    assert.deepEqual(
      calls.map((c) => c.headers.get("authorization")),
      ["Bearer first", "Bearer anon-key", "Bearer third"],
    );
    assert.ok(calls.every((c) => c.headers.get("apikey") === "anon-key"));
  });

  it("keeps an explicit Authorization header", async () => {
    const { calls, fetch } = fake();
    const client = createClient(URL_, "anon-key", { fetch, headers: { Authorization: "Bearer custom" } });
    await client.listBuckets();
    assert.equal(calls[0].headers.get("authorization"), "Bearer custom");
  });
});

describe("roll extras", () => {
  it("send the documented requests", async () => {
    const { calls, fetch } = fake(() => Response.json({}));
    const { roll } = createClient(URL_, "k", { fetch });
    await roll.versions("b", { prefix: "p/", limit: 10, cursor: "c" });
    await roll.trash("b");
    await roll.restore("b", "p/x.txt", 1700000000000);
    await roll.branches("b");
    await roll.createBranch("b", "dev");
    await roll.getPolicy("b");
    await roll.setPolicy("b", { Version: "2012-10-17", Statement: [] });
    await roll.deletePolicy("b");
    await roll.applyRules("b", "user-folders");
    const seen = calls.map((c) => [c.method, c.url.slice(URL_.length), c.body ? JSON.parse(c.body) : undefined]);
    assert.deepEqual(seen, [
      ["POST", "/roll/versions/b", { prefix: "p/", limit: 10, cursor: "c" }],
      ["POST", "/roll/trash/b", { prefix: "", cursor: null }],
      ["POST", "/roll/trash/b/restore", { path: "p/x.txt", deletedAt: 1700000000000 }],
      ["GET", "/roll/branches/b", undefined],
      ["POST", "/roll/branches/b", { name: "dev" }],
      ["GET", "/roll/policy/b", undefined],
      ["PUT", "/roll/policy/b", { Version: "2012-10-17", Statement: [] }],
      ["DELETE", "/roll/policy/b", undefined],
      ["POST", "/roll/rules/b", { preset: "user-folders" }],
    ]);
    for (const c of calls) {
      assert.equal(c.headers.get("authorization"), "Bearer k");
      if (c.body) assert.equal(c.headers.get("content-type"), "application/json");
    }
  });

  it("parse errors like storage-js", async () => {
    const { fetch } = fake(() => Response.json({ statusCode: "404", error: "not_found", message: "Bucket not found" }, { status: 400 }));
    const client = createClient(URL_, "k", { fetch });
    const { data, error } = await client.roll.branches("nope");
    assert.equal(data, null);
    assert.ok(error instanceof StorageApiError);
    assert.equal(error.status, 400);
    assert.equal(error.statusCode, "404");
    assert.equal(error.message, "Bucket not found");

    // storage-js itself builds the same error from the same response.
    const sjs = await client.getBucket("nope");
    assert.ok(sjs.error instanceof StorageApiError);
    assert.deepEqual(error.toJSON(), sjs.error.toJSON());
  });

  it("handle bodies that aren't JSON and network failures", async () => {
    const plain = createClient(URL_, "k", { fetch: fake(() => new Response("boom", { status: 502, statusText: "Bad Gateway" })).fetch });
    const r1 = await plain.roll.getPolicy("b");
    assert.ok(r1.error instanceof StorageApiError);
    assert.equal(r1.error.statusCode, "502");
    assert.equal(r1.error.message, "Bad Gateway");

    const down = createClient(URL_, "k", {
      fetch: async () => {
        throw new TypeError("fetch failed");
      },
    });
    const r2 = await down.roll.getPolicy("b");
    assert.ok(r2.error instanceof StorageUnknownError);
    assert.ok(isStorageError(r2.error));
  });

  it("throw after throwOnError()", async () => {
    const { fetch } = fake(() => Response.json({ statusCode: "403", error: "Unauthorized", message: "denied" }, { status: 400 }));
    const client = createClient(URL_, "k", { fetch }).throwOnError();
    await assert.rejects(client.roll.applyRules("b", "owner-only"), (e: unknown) => e instanceof StorageApiError && e.statusCode === "403");
  });

  it("follow setHeader", async () => {
    const { calls, fetch } = fake(() => Response.json([]));
    const client = createClient(URL_, "k", { fetch }).setHeader("x-trace", "abc");
    await client.roll.branches("b");
    assert.equal(calls[0].headers.get("x-trace"), "abc");
  });
});
