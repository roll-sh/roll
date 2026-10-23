// roll's extras (versions, trash, branches) and resumable uploads (TUS) against the real server.
import assert from "node:assert/strict";
import { describe, it } from "node:test";
import * as tus from "tus-js-client";
import { fails, ok, setup, sha256, text } from "./harness.js";

const { skip, env } = setup();

describe("versions", { skip, concurrency: true }, () => {
  it("a versioned bucket keeps every upload", async () => {
    const id = await env.bucket({ public: false, versioningStatus: "ENABLED" });
    assert.equal(ok(await env.service.getBucket(id)).versioning_status, "ENABLED");
    const files = env.service.from(id);
    ok(await files.upload("v.txt", "one"));
    ok(await files.upload("v.txt", "two", { upsert: true }));
    ok(await files.upload("other.txt", "x"));

    const page = ok(await env.service.roll.versions(id, { prefix: "v" }));
    assert.equal(page.next_cursor, null);
    const vs = page.versions;
    assert.equal(vs.length, 2, JSON.stringify(vs));
    assert.ok(vs.every((v) => v.name === "v.txt" && v.version_id && !v.is_delete_marker && v.size === 3 && v.etag && v.last_modified));
    assert.deepEqual(vs.map((v) => v.is_latest), [true, false], "newest first");
    assert.notEqual(vs[0].version_id, vs[1].version_id);
    assert.equal(await text(ok(await files.download("v.txt"))), "two");

    // Deleting leaves a delete marker on top.
    ok(await files.remove(["v.txt"]));
    const after = ok(await env.service.roll.versions(id, { prefix: "v.txt" })).versions;
    assert.equal(after.length, 3, JSON.stringify(after));
    assert.equal(after[0].is_delete_marker, true);
    assert.equal(after[0].is_latest, true);
    fails(await files.download("v.txt"), "404", "a deleted versioned object");
  });

  it("pages with limit and cursor", async () => {
    const id = await env.bucket({ public: false, versioningStatus: "ENABLED" });
    const files = env.service.from(id);
    await Promise.all(["a", "b", "c"].map(async (n) => ok(await files.upload(`${n}.txt`, n))));
    ok(await files.upload("a.txt", "a2", { upsert: true }));

    const seen: string[] = [];
    let cursor: string | null = null;
    for (let i = 0; i < 10; i++) {
      const page: { versions: { name: string; version_id: string }[]; next_cursor: string | null } = ok(await env.service.roll.versions(id, { limit: 2, cursor }));
      assert.ok(page.versions.length <= 2);
      seen.push(...page.versions.map((v) => `${v.name}@${v.version_id}`));
      cursor = page.next_cursor;
      if (!cursor) break;
    }
    assert.equal(seen.length, 4, seen.join(", "));
    assert.equal(new Set(seen).size, 4, "no version twice");
  });

  it("an unknown bucket is a 404 and anon can't list versions", async () => {
    fails(await env.service.roll.versions("missing-bucket-0000"), "404", "versions of an unknown bucket");
    const id = await env.bucket({ public: false, versioningStatus: "ENABLED" });
    assert.ok((await env.anon.roll.versions(id)).error, "anon listing versions");
  });
});

describe("trash", { skip, concurrency: true }, () => {
  it("removed objects show up in the trash and restore", async () => {
    const id = await env.bucket();
    const files = env.service.from(id);
    ok(await files.upload("docs/report.txt", "quarterly", { contentType: "text/plain" }));
    ok(await files.upload("docs/keep.txt", "keep"));
    ok(await files.remove(["docs/report.txt"]));
    fails(await files.download("docs/report.txt"), "404", "a removed object");

    const { items, next_cursor } = ok(await env.service.roll.trash(id));
    assert.equal(next_cursor, null);
    assert.equal(items.length, 1, JSON.stringify(items));
    const item = items[0];
    assert.equal(item.name, "docs/report.txt");
    assert.equal(item.size, 9);
    assert.ok(item.etag);
    assert.equal(typeof item.deleted_at, "number");
    assert.ok(Math.abs(item.deleted_at - Date.now()) < 120_000, `deleted_at ${item.deleted_at} is in ms`);
    assert.ok(item.expires_at > item.deleted_at);

    const restored = ok(await env.service.roll.restore(id, item.name, item.deleted_at));
    assert.equal(restored.name, "docs/report.txt");
    assert.equal(await text(ok(await files.download("docs/report.txt"))), "quarterly");
    assert.deepEqual(ok(await env.service.roll.trash(id)).items, []);

    fails(await env.service.roll.restore(id, item.name, item.deleted_at), "404", "restoring it twice");
  });

  it("filters by prefix and refuses anon", async () => {
    const id = await env.bucket();
    const files = env.service.from(id);
    ok(await files.upload("a/1.txt", "1"));
    ok(await files.upload("b/2.txt", "2"));
    ok(await files.remove(["a/1.txt", "b/2.txt"]));
    const a = ok(await env.service.roll.trash(id, { prefix: "a/" })).items;
    assert.deepEqual(a.map((i) => i.name), ["a/1.txt"]);
    assert.equal(ok(await env.service.roll.trash(id)).items.length, 2);
    assert.ok((await env.anon.roll.trash(id)).error, "anon listing the trash");
  });
});

describe("branches", { skip, concurrency: true }, () => {
  it("a branch starts with the parent's objects and then diverges", async () => {
    const id = await env.bucket();
    ok(await env.service.from(id).upload("seed.txt", "seed"));
    assert.deepEqual(ok(await env.service.roll.branches(id)), []);

    const branch = ok(await env.service.roll.createBranch(id, "dev"));
    assert.ok(branch.id && branch.name, JSON.stringify(branch));
    assert.notEqual(branch.id, id);
    const listed = ok(await env.service.roll.branches(id));
    assert.deepEqual(listed.map((b) => b.id), [branch.id]);

    const dev = env.service.from(branch.id);
    assert.equal(await text(ok(await dev.download("seed.txt"))), "seed");
    ok(await dev.upload("dev-only.txt", "dev"));
    ok(await env.service.from(id).upload("seed.txt", "changed on main", { upsert: true }));
    assert.equal(await text(ok(await dev.download("seed.txt"))), "seed", "the branch keeps its copy");
    fails(await env.service.from(id).download("dev-only.txt"), "404", "a branch's object on the parent");

    fails(await env.service.roll.createBranch(id, "dev"), "409", "a branch name twice");
    fails(await env.service.roll.branches("missing-bucket-0000"), "404", "branches of an unknown bucket");
  });
});

/** Upload with tus-js-client, like supabase's resumable upload guide. */
function tusUpload(file: Buffer, opts: { token: string; apikey: string; bucket: string; name: string; upsert?: boolean; contentType?: string; dataDuringCreation?: boolean }) {
  return new Promise<string>((resolve, reject) => {
    const upload = new tus.Upload(file, {
      endpoint: `${env.url}/upload/resumable`,
      retryDelays: [],
      headers: { authorization: `Bearer ${opts.token}`, apikey: opts.apikey, "x-upsert": String(opts.upsert ?? false) },
      uploadDataDuringCreation: opts.dataDuringCreation ?? true,
      removeFingerprintOnSuccess: true,
      chunkSize: 6 * 1024 * 1024,
      metadata: { bucketName: opts.bucket, objectName: opts.name, contentType: opts.contentType ?? "application/octet-stream", cacheControl: "3600" },
      onError: reject,
      onSuccess: () => resolve(upload.url ?? ""),
    });
    upload.start();
  });
}

/** A tus error whose response had this status. */
const status = (code: number) => (e: unknown) => {
  const got = (e as tus.DetailedError).originalResponse?.getStatus();
  assert.equal(got, code, `expected HTTP ${code}, got ${got ?? "no response"}: ${String(e)}`);
  return true;
};

describe("resumable uploads (TUS)", { skip, concurrency: true }, () => {
  it("uploads ~12 MiB in 6 MiB chunks", async () => {
    const id = await env.bucket();
    const size = 12 * 1024 * 1024 + 12344;
    const file = Buffer.alloc(size);
    for (let i = 0; i < size; i += 4) file.writeUInt32LE((i * 2654435761) >>> 0, i);
    const key = env.server.service_key;
    const url = await tusUpload(file, { token: key, apikey: key, bucket: id, name: "big/blob.bin" });
    assert.ok(url.startsWith(`${env.url}/upload/resumable/`), url);

    const info = ok(await env.service.from(id).info("big/blob.bin"));
    assert.equal(info.size, size);
    const back = Buffer.from(await ok(await env.service.from(id).download("big/blob.bin")).arrayBuffer());
    assert.equal(back.length, size);
    assert.equal(sha256(back), sha256(file));
  });

  it("respects upsert, rules and bucket limits", async () => {
    const id = await env.bucket({ public: false, fileSizeLimit: "1MB" });
    const key = env.server.service_key;
    const small = Buffer.from("tus small");
    await tusUpload(small, { token: key, apikey: key, bucket: id, name: "s.txt", contentType: "text/plain" });
    assert.equal(await text(ok(await env.service.from(id).download("s.txt"))), "tus small");

    await assert.rejects(tusUpload(small, { token: key, apikey: key, bucket: id, name: "s.txt" }), status(409), "no upsert");
    await tusUpload(Buffer.from("again"), { token: key, apikey: key, bucket: id, name: "s.txt", upsert: true });
    assert.equal(await text(ok(await env.service.from(id).download("s.txt"))), "again");

    // Without data in the creation request, the size check answers the POST.
    await assert.rejects(tusUpload(Buffer.alloc(2_000_000), { token: key, apikey: key, bucket: id, name: "big.bin", dataDuringCreation: false }), status(413), "over the bucket's limit");

    // A user follows the bucket's rules.
    ok(await env.service.roll.applyRules(id, "user-folders"));
    const u = env.user();
    const anon = env.server.anon_key;
    await tusUpload(Buffer.from("mine"), { token: u.token, apikey: anon, bucket: id, name: `${u.sub}/t.txt` });
    assert.equal(await text(ok(await u.client.from(id).download(`${u.sub}/t.txt`))), "mine");
    await assert.rejects(tusUpload(Buffer.from("theirs"), { token: u.token, apikey: anon, bucket: id, name: "elsewhere/t.txt" }), status(403), "outside the user's folder");
  });

  it("answers a creation-with-data over the bucket's limit with 413", async () => {
    // tus-js-client streams the body with the POST; the client must still see the 413.
    const id = await env.bucket({ public: false, fileSizeLimit: "1MB" });
    const key = env.server.service_key;
    await assert.rejects(tusUpload(Buffer.alloc(2_000_000), { token: key, apikey: key, bucket: id, name: "big.bin" }), status(413), "over the bucket's limit");
    assert.equal((await env.service.from(id).exists("big.bin")).data, false);
  });
});
