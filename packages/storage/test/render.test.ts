// Image transformations and public URLs through the real storage-js: createSignedUrl,
// getPublicUrl and download with `transform`, roll's public URL helper (immutable URLs), and the
// CDN's URLs.
import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { createClient } from "../src/index.js";
import { fails, imageSize, ok, png, setup } from "./harness.js";

const { skip, env } = setup();

async function fetched(url: string, init?: RequestInit) {
  const r = await fetch(url, init);
  const body = new Uint8Array(await r.arrayBuffer());
  return { status: r.status, headers: r.headers, body, size: imageSize(body) };
}

describe("image transformations", { skip, concurrency: true }, () => {
  it("createSignedUrl with transform", async () => {
    const id = await env.bucket({ public: false });
    const files = env.service.from(id);
    ok(await files.upload("photos/cat.png", png(400, 300), { contentType: "image/png" }));

    const signed = ok(await files.createSignedUrl("photos/cat.png", 60, { transform: { width: 100, height: 100, resize: "contain" } }));
    assert.match(signed.signedUrl, /\/render\/image\/sign\/.+\/photos\/cat\.png\?token=/);
    const r = await fetched(signed.signedUrl);
    assert.equal(r.status, 200);
    assert.deepEqual(r.size, { format: "png", width: 100, height: 75 });

    // The format is negotiated like Supabase's: WebP for clients that take it.
    const webp = await fetched(signed.signedUrl, { headers: { accept: "image/webp,*/*" } });
    assert.deepEqual(webp.size, { format: "webp", width: 100, height: 75 });
    assert.equal(webp.headers.get("vary"), "Accept");

    // download and cacheNonce ride along; the transformation is the token's.
    const dl = ok(await files.createSignedUrl("photos/cat.png", 60, { transform: { width: 40, format: "origin" }, download: "small.png", cacheNonce: "1" }));
    const got = await fetched(dl.signedUrl);
    assert.deepEqual(got.size, { format: "png", width: 40, height: 30 });
    assert.match(got.headers.get("content-disposition") ?? "", /^attachment; filename=small\.png/);
    const tampered = await fetch(dl.signedUrl.replace("token=", "width=400&token="));
    assert.equal(tampered.status, 400);
    assert.equal(((await tampered.json()) as { statusCode: string }).statusCode, "400");

    // Without read access, nothing is signed.
    const user = env.user();
    fails(await user.client.from(id).createSignedUrl("photos/cat.png", 60, { transform: { width: 10 } }), "404");
  });

  it("getPublicUrl with transform", async () => {
    const id = await env.bucket({ public: true });
    const files = env.service.from(id);
    ok(await files.upload("a b/dog.png", png(200, 100), { contentType: "image/png" }));
    const { publicUrl } = files.getPublicUrl("a b/dog.png", { transform: { width: 50, quality: 60, format: "origin" } }).data;
    assert.ok(publicUrl.includes("/render/image/public/"), publicUrl);
    const r = await fetched(publicUrl);
    assert.equal(r.status, 200);
    assert.deepEqual(r.size, { format: "png", width: 50, height: 25 });
    // Anyone may fetch it; a private bucket's isn't public.
    const priv = await env.bucket({ public: false });
    ok(await env.service.from(priv).upload("x.png", png(10, 10), { contentType: "image/png" }));
    const denied = await fetch(env.service.from(priv).getPublicUrl("x.png", { transform: { width: 5 } }).data.publicUrl);
    assert.equal(denied.status, 400);
  });

  it("download with transform", async () => {
    const id = await env.bucket({ public: false });
    ok(await env.service.from(id).upload("p.png", png(300, 300), { contentType: "image/png" }));
    const blob = ok(await env.service.from(id).download("p.png", { transform: { width: 60, height: 30, resize: "fill", format: "origin" } }));
    assert.deepEqual(imageSize(new Uint8Array(await blob.arrayBuffer())), { format: "png", width: 60, height: 30 });
    // Authenticated: the user's rules decide.
    const user = env.user();
    fails(await user.client.from(id).download("p.png", { transform: { width: 10 } }), "404");
    ok(await env.service.roll.applyRules(id, "authenticated-read"));
    const mine = ok(await user.client.from(id).download("p.png", { transform: { width: 10, format: "origin" } }));
    assert.deepEqual(imageSize(new Uint8Array(await mine.arrayBuffer())), { format: "png", width: 10, height: 10 });
    fails(await env.service.from(id).download("p.png", { transform: { width: 99999 } }), "400");
  });
});

describe("public URLs", { skip, concurrency: true }, () => {
  it("roll.publicUrl: plain, transformed and immutable", async () => {
    const id = await env.bucket({ public: true });
    const files = env.service.from(id);
    ok(await files.upload("logo.png", png(64, 64), { contentType: "image/png" }));
    const plain = ok(await env.service.roll.publicUrl(id, "logo.png"));
    assert.deepEqual(plain, { publicUrl: files.getPublicUrl("logo.png").data.publicUrl, cdn: false, immutable: false });

    const thumb = ok(await env.service.roll.publicUrl(id, "logo.png", { transform: { width: 16, format: "png" }, immutable: true }));
    assert.equal(thumb.immutable, true);
    assert.match(thumb.publicUrl, /\/storage\/v1\/v\/[0-9a-f]{20}\/.+\/logo\.png\?width=16&format=png$/);
    const r = await fetched(thumb.publicUrl);
    assert.equal(r.headers.get("cache-control"), "public, max-age=31536000, immutable");
    assert.deepEqual(r.size, { format: "png", width: 16, height: 16 });
    // A new version is a new URL; the old one stops working.
    ok(await files.upload("logo.png", png(32, 32), { contentType: "image/png", upsert: true }));
    assert.equal((await fetch(thumb.publicUrl)).status, 400);
    const next = ok(await env.service.roll.publicUrl(id, "logo.png", { immutable: true }));
    assert.notEqual(next.publicUrl.split("/v/")[1].split("/")[0], thumb.publicUrl.split("/v/")[1].split("/")[0]);
    assert.deepEqual((await fetched(next.publicUrl)).size, { format: "png", width: 32, height: 32 });

    fails(await env.anon.roll.publicUrl(await env.bucket({ public: false }), "nope.png"), "404");
  });

  it("CDN URLs", async () => {
    const client = createClient(env.url, env.server.anon_key, { cdnUrl: "https://cdn.example.com/" });
    assert.equal(client.cdnUrl, "https://cdn.example.com");
    assert.equal(client.getPublicUrl("media", "a b.png").data.publicUrl, "https://cdn.example.com/storage/v1/object/public/media/a%20b.png");
    assert.equal(
      client.getPublicUrl("media", "a b.png", { transform: { width: 200, format: "webp" }, download: true }).data.publicUrl,
      "https://cdn.example.com/storage/v1/render/image/public/media/a%20b.png?download=&width=200&format=webp",
    );
    // API URLs the CDN serves move to it; the rest stay on the API.
    assert.equal(client.cdn(`${env.url}/render/image/sign/media/a.png?token=t`), "https://cdn.example.com/storage/v1/render/image/sign/media/a.png?token=t");
    assert.equal(client.cdn(`${env.url}/object/sign/media/a.png?token=t`), "https://cdn.example.com/storage/v1/object/sign/media/a.png?token=t");
    assert.equal(client.cdn(`${env.url}/v/0123/media/a.png`), "https://cdn.example.com/storage/v1/v/0123/media/a.png");
    assert.equal(client.cdn(`${env.url}/object/authenticated/media/a.png`), `${env.url}/object/authenticated/media/a.png`);
    assert.equal(client.cdn("https://elsewhere.example/x"), "https://elsewhere.example/x");
    // Without a CDN, the API's own.
    assert.equal(env.anon.getPublicUrl("media", "a.png").data.publicUrl, `${env.url}/object/public/media/a.png`);
    assert.equal(env.anon.cdn(`${env.url}/object/public/media/a.png`), `${env.url}/object/public/media/a.png`);
  });
});
