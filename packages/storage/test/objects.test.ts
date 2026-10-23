// Objects through storage-js against the real server: upload, download, list, move, copy, remove,
// signed URLs, signed uploads and public URLs.
import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { fails, ok, setup, text } from "./harness.js";

const { skip, env } = setup();

describe("objects", { skip, concurrency: true }, () => {
  it("upload, download, info, exists and upsert", async () => {
    const files = env.service.from(await env.bucket());
    const up = ok(await files.upload("docs/hello.txt", "hello world", { contentType: "text/plain", cacheControl: "60" }));
    assert.equal(up.path, "docs/hello.txt");
    assert.ok(up.fullPath.endsWith("docs/hello.txt"), up.fullPath);

    const blob = ok(await files.download("docs/hello.txt"));
    assert.equal(await blob.text(), "hello world");

    const info = ok(await files.info("docs/hello.txt"));
    assert.equal(info.name, "docs/hello.txt");
    assert.equal(info.size, 11);
    assert.equal(info.contentType ?? (info as { content_type?: string }).content_type, "text/plain");

    assert.equal(ok(await files.exists("docs/hello.txt")), true);
    const missing = await files.exists("docs/nope.txt");
    assert.equal(missing.data, false);

    fails(await files.upload("docs/hello.txt", "again"), "409", "uploading over an object without upsert");
    ok(await files.upload("docs/hello.txt", "replaced", { upsert: true, contentType: "text/plain" }));
    assert.equal(await text(ok(await files.download("docs/hello.txt"))), "replaced");

    ok(await files.update("docs/hello.txt", "updated", { contentType: "text/plain" }));
    assert.equal(await text(ok(await files.download("docs/hello.txt"))), "updated");

    fails(await files.download("docs/nope.txt"), "404", "downloading a missing object");
  });

  it("uploads binary bodies byte for byte", async () => {
    const files = env.service.from(await env.bucket());
    const bytes = new Uint8Array(70_000).map((_, i) => (i * 31 + 7) & 0xff);
    ok(await files.upload("bin/data.bin", bytes, { contentType: "application/octet-stream" }));
    const back = new Uint8Array(await ok(await files.download("bin/data.bin")).arrayBuffer());
    assert.deepEqual(back, bytes);
    ok(await files.upload("bin/blob.bin", new Blob([bytes]), { contentType: "application/octet-stream" }));
    assert.equal(ok(await files.info("bin/blob.bin")).size, bytes.length);
  });

  it("list folders, search, limit and offset", async () => {
    const files = env.service.from(await env.bucket());
    for (const p of ["a/1.txt", "a/2.txt", "a/3.txt", "a/sub/x.txt", "b.txt"]) ok(await files.upload(p, p));

    const root = ok(await files.list());
    assert.deepEqual(root.map((f) => f.name).sort(), ["a", "b.txt"]);
    const folder = root.find((f) => f.name === "a")!;
    assert.equal(folder.id, null, "folders have no id");

    const a = ok(await files.list("a", { sortBy: { column: "name", order: "asc" } }));
    assert.deepEqual(a.map((f) => f.name), ["1.txt", "2.txt", "3.txt", "sub"]);

    const page = ok(await files.list("a", { limit: 2, offset: 1, sortBy: { column: "name", order: "asc" } }));
    assert.deepEqual(page.map((f) => f.name), ["2.txt", "3.txt"]);

    const found = ok(await files.list("a", { search: "2" }));
    assert.deepEqual(found.map((f) => f.name), ["2.txt"]);
  });

  it("move, copy and remove", async () => {
    const id = await env.bucket();
    const files = env.service.from(id);
    ok(await files.upload("src.txt", "payload"));

    ok(await files.copy("src.txt", "copy/dst.txt"));
    assert.equal(await text(ok(await files.download("copy/dst.txt"))), "payload");
    assert.equal(await text(ok(await files.download("src.txt"))), "payload");

    ok(await files.move("src.txt", "moved/src.txt"));
    assert.equal(await text(ok(await files.download("moved/src.txt"))), "payload");
    fails(await files.download("src.txt"), "404", "the move's source");

    // Into another bucket.
    const other = await env.bucket();
    ok(await files.copy("moved/src.txt", "elsewhere.txt", { destinationBucket: other }));
    assert.equal(await text(ok(await env.service.from(other).download("elsewhere.txt"))), "payload");

    fails(await files.move("nope.txt", "x.txt"), "404", "moving a missing object");

    const removed = ok(await files.remove(["copy/dst.txt", "moved/src.txt"]));
    assert.deepEqual(removed.map((f) => f.name).sort(), ["copy/dst.txt", "moved/src.txt"]);
    assert.deepEqual(ok(await files.list()), []);
    // Removing what isn't there is fine and returns nothing.
    assert.deepEqual(ok(await files.remove(["gone.txt"])), []);
  });

  it("signed URLs download with a plain fetch", async () => {
    const files = env.service.from(await env.bucket());
    ok(await files.upload("secret/a.txt", "AAA", { contentType: "text/plain" }));
    ok(await files.upload("secret/b.txt", "BBB", { contentType: "text/plain" }));

    const { signedUrl } = ok(await files.createSignedUrl("secret/a.txt", 60));
    const res = await fetch(signedUrl);
    assert.equal(res.status, 200);
    assert.equal(await res.text(), "AAA");

    const dl = ok(await files.createSignedUrl("secret/a.txt", 60, { download: "renamed.txt" }));
    const r2 = await fetch(dl.signedUrl);
    assert.equal(r2.status, 200);
    assert.match(r2.headers.get("content-disposition") ?? "", /attachment.*renamed\.txt/);

    const many = ok(await files.createSignedUrls(["secret/a.txt", "secret/b.txt", "secret/missing.txt"], 60));
    assert.equal(many.length, 3);
    assert.equal(await (await fetch(many[0].signedUrl!)).text(), "AAA");
    assert.equal(await (await fetch(many[1].signedUrl!)).text(), "BBB");
    assert.ok(many[2].error, "a missing path has an error");
    assert.equal(many[2].signedUrl, null);

    // A tampered token is refused.
    const bad = new URL(signedUrl);
    bad.searchParams.set("token", `${bad.searchParams.get("token")}x`);
    assert.notEqual((await fetch(bad)).status, 200);

    fails(await files.createSignedUrl("secret/missing.txt", 60), "404", "signing a missing object");
  });

  it("signed upload URLs", async () => {
    const id = await env.bucket();
    const files = env.service.from(id);
    const signed = ok(await files.createSignedUploadUrl("up/file.txt"));
    assert.equal(signed.path, "up/file.txt");
    assert.ok(signed.token);

    // Anyone holding the token can upload, here with the anon key.
    const up = ok(await env.anon.from(id).uploadToSignedUrl(signed.path, signed.token, "via signed url", { contentType: "text/plain" }));
    assert.equal(up.path, "up/file.txt");
    assert.equal(await text(ok(await files.download("up/file.txt"))), "via signed url");

    // The token was for a create: a second upload without upsert conflicts.
    const again = await env.anon.from(id).uploadToSignedUrl(signed.path, signed.token, "twice");
    assert.ok(again.error, "a second upload with a non-upsert token fails");

    const upsert = ok(await files.createSignedUploadUrl("up/file.txt", { upsert: true }));
    ok(await env.anon.from(id).uploadToSignedUrl(upsert.path, upsert.token, "overwritten", { upsert: true }));
    assert.equal(await text(ok(await files.download("up/file.txt"))), "overwritten");

    // A token only works for its own path.
    const other = await env.anon.from(id).uploadToSignedUrl("up/other.txt", signed.token, "nope");
    assert.ok(other.error, "a token for another path is refused");
  });

  it("public URLs work on public buckets only", async () => {
    const pub = await env.bucket({ public: true });
    ok(await env.service.from(pub).upload("img/logo.txt", "public bytes", { contentType: "text/plain" }));
    const { publicUrl } = env.anon.from(pub).getPublicUrl("img/logo.txt").data;
    assert.ok(publicUrl.startsWith(`${env.url}/object/public/${pub}/`), publicUrl);
    const res = await fetch(publicUrl);
    assert.equal(res.status, 200);
    assert.equal(await res.text(), "public bytes");
    assert.equal(res.headers.get("content-type"), "text/plain");

    const dl = await fetch(env.anon.from(pub).getPublicUrl("img/logo.txt", { download: "x.txt" }).data.publicUrl);
    assert.match(dl.headers.get("content-disposition") ?? "", /attachment/);

    // Anyone can read a public bucket's objects through the API too.
    assert.equal(await text(ok(await env.anon.from(pub).download("img/logo.txt"))), "public bytes");

    const priv = await env.bucket({ public: false });
    ok(await env.service.from(priv).upload("x.txt", "private"));
    const r = await fetch(env.anon.from(priv).getPublicUrl("x.txt").data.publicUrl);
    assert.notEqual(r.status, 200, "a private bucket's public URL doesn't serve");
    fails(await env.anon.from(priv).download("x.txt"), "404", "anon reading a private object");
  });

  it("image transformations aren't supported", async () => {
    const pub = await env.bucket({ public: true });
    const files = env.service.from(pub);
    ok(await files.upload("p.png", new Uint8Array([0x89, 0x50, 0x4e, 0x47]), { contentType: "image/png" }));
    fails(await files.download("p.png", { transform: { width: 10, height: 10 } }), "501", "a transformed download");
  });
});
