// Buckets through storage-js against the real server: lifecycle, limits and a bare StorageClient.
import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { StorageClient } from "../src/index.js";
import { bucketId, fails, ok, setup, text } from "./harness.js";

const { skip, env } = setup();

describe("buckets", { skip, concurrency: true }, () => {
  it("create, get, list, update, empty and delete", async () => {
    const id = bucketId("life");
    assert.deepEqual(ok(await env.service.createBucket(id, { public: false })), { name: id });
    fails(await env.service.createBucket(id), "409", "creating it twice");

    const b = ok(await env.service.getBucket(id));
    assert.equal(b.id, id);
    assert.equal(b.name, id);
    assert.equal(b.public, false);

    const all = ok(await env.service.listBuckets());
    assert.ok(all.some((x) => x.id === id), "listBuckets includes it");

    ok(await env.service.updateBucket(id, { public: true, fileSizeLimit: "1KB", allowedMimeTypes: ["text/*"] }));
    const u = ok(await env.service.getBucket(id));
    assert.equal(u.public, true);
    assert.equal(u.file_size_limit, 1000, "KB is 1000 bytes, as in Supabase");
    assert.deepEqual(u.allowed_mime_types, ["text/*"]);

    ok(await env.service.from(id).upload("a/one.txt", "1", { contentType: "text/plain" }));
    ok(await env.service.from(id).upload("two.txt", "2", { contentType: "text/plain" }));
    fails(await env.service.deleteBucket(id), "409", "deleting a non-empty bucket");

    ok(await env.service.emptyBucket(id));
    assert.deepEqual(ok(await env.service.from(id).list()), []);
    ok(await env.service.deleteBucket(id));
    fails(await env.service.getBucket(id), "404", "getting a deleted bucket");
  });

  it("enforce fileSizeLimit and allowedMimeTypes on upload", async () => {
    const id = await env.bucket({ public: false, fileSizeLimit: "1KB", allowedMimeTypes: ["image/*", "text/plain"] });
    const files = env.service.from(id);

    fails(await files.upload("big.txt", "x".repeat(2048), { contentType: "text/plain" }), "413", "an upload over the limit");
    fails(await files.upload("doc.json", "{}", { contentType: "application/json" }), "415", "a disallowed type");
    ok(await files.upload("small.txt", "hello", { contentType: "text/plain" }));
    ok(await files.upload("pic.png", new Uint8Array([0x89, 0x50, 0x4e, 0x47]), { contentType: "image/png" }));

    // A numeric limit works too.
    const id2 = await env.bucket({ public: false, fileSizeLimit: 10 });
    fails(await env.service.from(id2).upload("x.bin", new Uint8Array(11)), "413", "11 bytes over a 10-byte limit");
    ok(await env.service.from(id2).upload("y.bin", new Uint8Array(10)));
  });

  it("reject invalid bucket ids and unknown buckets", async () => {
    assert.ok((await env.service.createBucket("Not_Valid")).error, "uppercase/underscore ids are rejected");
    fails(await env.service.getBucket(bucketId("missing")), "404", "an unknown bucket");
    fails(await env.service.from(bucketId("missing")).upload("a.txt", "a"), "404", "uploading to an unknown bucket");
  });

  it("are managed by the service key only", async () => {
    assert.ok((await env.anon.createBucket(bucketId("anon"))).error, "anon can't create buckets");
    assert.ok((await env.user().client.createBucket(bucketId("user"))).error, "a user can't create buckets");
    const id = await env.bucket();
    assert.ok((await env.anon.deleteBucket(id)).error, "anon can't delete buckets");
    ok(await env.service.getBucket(id));
  });

  it("work with a bare StorageClient", async () => {
    const client = new StorageClient(env.url, { apikey: env.server.service_key, Authorization: `Bearer ${env.server.service_key}` });
    const id = bucketId("bare");
    ok(await client.createBucket(id));
    ok(await client.from(id).upload("hello.txt", "hi there", { contentType: "text/plain" }));
    assert.equal(await text(ok(await client.from(id).download("hello.txt"))), "hi there");
    assert.ok(ok(await client.listBuckets()).some((b) => b.id === id));
  });
});
