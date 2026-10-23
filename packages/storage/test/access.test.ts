// Who may do what: anon, signed-in users (HS256 JWTs) and the service key, with and without rules.
import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { createClient } from "../src/index.js";
import { fails, ok, setup, signJwt, text } from "./harness.js";

const { skip, env } = setup();

describe("access", { skip, concurrency: true }, () => {
  it("without a policy a user can't upload or read a private bucket", async () => {
    const id = await env.bucket();
    ok(await env.service.from(id).upload("x.txt", "private"));
    const { client } = env.user();
    fails(await client.from(id).upload("mine.txt", "hi"), "403", "a user uploading without a policy");
    assert.ok((await client.from(id).download("x.txt")).error, "a user reading without a policy");
    fails(await env.anon.from(id).download("x.txt"), "404", "anon reading a private object");
  });

  it("refuses bad tokens", async () => {
    const id = await env.bucket({ public: true });
    ok(await env.service.from(id).upload("x.txt", "x"));
    const forged = signJwt("not-the-secret-not-the-secret-not-the-secret", { sub: "u", role: "service_role", exp: Math.floor(Date.now() / 1000) + 60 });
    const bad = createClient(env.url, env.server.anon_key, { accessToken: forged });
    assert.ok((await bad.listBuckets()).error, "a token with the wrong signature is refused");

    const expired = signJwt(env.server.jwt_secret, { sub: "u", role: "authenticated", exp: Math.floor(Date.now() / 1000) - 60 });
    const old = createClient(env.url, env.server.anon_key, { accessToken: expired });
    assert.ok((await old.from(id).upload("y.txt", "y")).error, "an expired token is refused");
  });

  it("user-folders: a user reads and writes only its own folder", async () => {
    const id = await env.bucket();
    const { policy } = ok(await env.service.roll.applyRules(id, "user-folders"));
    assert.ok(policy && typeof policy === "object");

    const alice = env.user();
    const bob = env.user();
    const mine = `${alice.sub}/notes.txt`;
    ok(await alice.client.from(id).upload(mine, "alice's"), "alice writing her folder");
    assert.equal(await text(ok(await alice.client.from(id).download(mine))), "alice's");
    assert.ok(ok(await alice.client.from(id).list(alice.sub)).some((f) => f.name === "notes.txt"));

    assert.ok((await alice.client.from(id).upload(`${bob.sub}/x.txt`, "nope")).error, "alice writing bob's folder");
    assert.ok((await alice.client.from(id).upload("top.txt", "nope")).error, "alice writing outside any folder");
    assert.ok((await bob.client.from(id).download(mine)).error, "bob reading alice's file");
    ok(await bob.client.from(id).upload(`${bob.sub}/b.txt`, "bob's"));

    // Async tokens work the same way.
    const lazy = createClient(env.url, env.server.anon_key, { accessToken: async () => alice.token });
    assert.equal(await text(ok(await lazy.from(id).download(mine))), "alice's");

    // Anon is still out, and the service key still sees everything.
    assert.ok((await env.anon.from(id).download(mine)).error);
    assert.equal(await text(ok(await env.service.from(id).download(`${bob.sub}/b.txt`))), "bob's");
  });

  it("owner-only: a user reads and deletes only what it uploaded", async () => {
    const id = await env.bucket();
    ok(await env.service.roll.applyRules(id, "owner-only"));
    const alice = env.user();
    const bob = env.user();

    ok(await alice.client.from(id).upload("shared/a.txt", "from alice"));
    ok(await bob.client.from(id).upload("shared/b.txt", "from bob"));

    const info = ok(await env.service.from(id).info("shared/a.txt"));
    assert.equal((info as { owner?: string }).owner ?? (info as { owner_id?: string }).owner_id, alice.sub);

    assert.equal(await text(ok(await alice.client.from(id).download("shared/a.txt"))), "from alice");
    assert.ok((await alice.client.from(id).download("shared/b.txt")).error, "alice reading bob's upload");

    // Deleting someone else's object removes nothing.
    const notHers = await alice.client.from(id).remove(["shared/b.txt"]);
    assert.ok(notHers.error || notHers.data?.length === 0, `alice deleting bob's upload: ${JSON.stringify(notHers)}`);
    assert.equal(await text(ok(await env.service.from(id).download("shared/b.txt"))), "from bob");

    const gone = ok(await alice.client.from(id).remove(["shared/a.txt"]));
    assert.deepEqual(gone.map((f) => f.name), ["shared/a.txt"]);
    assert.equal(ok(await env.service.from(id).exists("shared/b.txt")), true);
    assert.equal((await env.service.from(id).exists("shared/a.txt")).data, false);
  });

  it("public-read and authenticated-read", async () => {
    const id = await env.bucket();
    ok(await env.service.from(id).upload("doc.txt", "readable"));
    const { client } = env.user();

    ok(await env.service.roll.applyRules(id, "authenticated-read"));
    assert.equal(await text(ok(await client.from(id).download("doc.txt"))), "readable");
    assert.ok((await env.anon.from(id).download("doc.txt")).error, "anon under authenticated-read");
    assert.ok((await client.from(id).upload("w.txt", "w")).error, "authenticated-read doesn't allow writes");

    ok(await env.service.roll.applyRules(id, "public-read"));
    assert.equal(await text(ok(await env.anon.from(id).download("doc.txt"))), "readable");
    assert.equal(await text(ok(await client.from(id).download("doc.txt"))), "readable");
  });

  it("getPolicy, setPolicy and deletePolicy", async () => {
    const id = await env.bucket();
    assert.equal(ok(await env.service.roll.getPolicy(id)).policy, null);

    const { policy } = ok(await env.service.roll.applyRules(id, "public-read"));
    assert.deepEqual(ok(await env.service.roll.getPolicy(id)).policy, policy);

    ok(await env.service.roll.setPolicy(id, policy));
    assert.deepEqual(ok(await env.service.roll.getPolicy(id)).policy, policy);
    ok(await env.service.from(id).upload("p.txt", "p"));
    assert.equal(await text(ok(await env.anon.from(id).download("p.txt"))), "p");

    ok(await env.service.roll.deletePolicy(id));
    assert.equal(ok(await env.service.roll.getPolicy(id)).policy, null);
    assert.ok((await env.anon.from(id).download("p.txt")).error, "no policy, no anon reads");

    assert.ok((await env.service.roll.setPolicy(id, { nonsense: true })).error, "an invalid document is refused");
    assert.ok((await env.service.roll.applyRules(id, "everyone" as "public-read")).error, "an unknown preset is refused");
    fails(await env.service.roll.getPolicy("nope-" + id), "404", "the policy of an unknown bucket");
  });

  it("only the service key manages policies", async () => {
    const id = await env.bucket();
    assert.ok((await env.anon.roll.applyRules(id, "public-read")).error, "anon applying rules");
    assert.ok((await env.user().client.roll.setPolicy(id, { Version: "2012-10-17", Statement: [] })).error, "a user setting a policy");
    assert.equal(ok(await env.service.roll.getPolicy(id)).policy, null);
  });
});
