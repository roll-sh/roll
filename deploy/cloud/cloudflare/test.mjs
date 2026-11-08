// apply.mjs against a fake Cloudflare API: the dry run changes nothing, a run converges the
// zones (leaving unmanaged records alone), and a second run plans nothing.
//
//   node --test deploy/cloud/cloudflare/test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import { test } from "node:test";
import { apply } from "./apply.mjs";

const config = JSON.parse(readFileSync(new URL("./zones.json", import.meta.url), "utf8"));
const COMMENT = config.comment;
const NLB = "roll-cloud-0123.elb.us-east-1.amazonaws.com";

/** An in-memory Cloudflare API for two zones, paginated like the real one. */
function fakeCloudflare() {
  let next = 1;
  const id = () => `id${next++}`;
  const zones = {
    "roll.cloud": {
      id: "z1",
      records: [
        // Someone's own record: never touched.
        { id: id(), type: "TXT", name: "roll.cloud", content: "google-site-verification=x", proxied: false, ttl: 1 },
        // Ours, stale: deleted.
        { id: id(), type: "CNAME", name: "old.roll.cloud", content: "origin.us-east-1.roll.cloud", proxied: false, ttl: 1, comment: COMMENT },
        // Ours, wrong: updated.
        { id: id(), type: "CNAME", name: "api.roll.cloud", content: "elsewhere.example", proxied: false, ttl: 300, comment: COMMENT },
      ],
      settings: { ssl: "full", always_use_https: "off", min_tls_version: "1.0", tls_1_3: "on", tls_client_auth: "off", security_header: { strict_transport_security: { enabled: false, max_age: 0, include_subdomains: false, preload: false, nosniff: false } } },
      routes: [],
    },
    "roll.site": {
      id: "z2",
      records: [],
      settings: { ssl: "flexible", always_use_https: "off", min_tls_version: "1.0", tls_1_3: "off", tls_client_auth: "off" },
      // A route of ours on a pattern we no longer list: deleted.
      routes: [{ id: id(), pattern: "cdn.roll.site/*", script: "roll-cdn" }, { id: id(), pattern: "other.roll.site/*", script: "someone-else" }],
    },
  };
  const writes = [];
  const byId = (zid) => Object.values(zones).find((z) => z.id === zid);
  const server = createServer(async (req, res) => {
    let body = "";
    for await (const chunk of req) body += chunk;
    const url = new URL(req.url, "http://x");
    const send = (status, json) => {
      res.writeHead(status, { "content-type": "application/json" });
      res.end(JSON.stringify({ success: status < 400, errors: status < 400 ? [] : [{ code: 1000, message: "bad" }], ...json }));
    };
    if (req.headers.authorization !== "Bearer test-token") return send(403, {});
    if (req.method !== "GET") writes.push(`${req.method} ${url.pathname}`);
    const parts = url.pathname.split("/").filter(Boolean);
    if (parts[0] !== "zones") return send(404, {});
    if (parts.length === 1) {
      const name = url.searchParams.get("name");
      return send(200, { result: zones[name] ? [{ id: zones[name].id, name }] : [] });
    }
    const zone = byId(parts[1]);
    if (!zone) return send(404, {});
    const data = body ? JSON.parse(body) : undefined;
    const [, , kind, sub, rid] = parts;
    if (kind === "dns_records") {
      if (req.method === "GET") {
        const page = Number(url.searchParams.get("page") ?? 1);
        const per = 2; // small pages, so pagination is exercised
        const all = zone.records;
        return send(200, { result: all.slice((page - 1) * per, page * per), result_info: { page, total_pages: Math.max(1, Math.ceil(all.length / per)) } });
      }
      if (req.method === "POST") {
        const apex = Object.keys(zones).includes(data.name);
        const addr = (t) => ["A", "AAAA", "CNAME"].includes(t);
        if (zone.records.some((r) => r.name === data.name && (r.type === "CNAME" || data.type === "CNAME") && (!apex || (addr(r.type) && addr(data.type))))) return send(400, {});
        const r = { id: id(), ...data };
        zone.records.push(r);
        return send(200, { result: r });
      }
      const i = zone.records.findIndex((r) => r.id === sub);
      if (i < 0) return send(404, {});
      if (req.method === "PUT") zone.records[i] = { id: sub, ...data };
      if (req.method === "DELETE") zone.records.splice(i, 1);
      return send(200, { result: { id: sub } });
    }
    if (kind === "settings") {
      if (req.method === "GET" && !sub) return send(200, { result: Object.entries(zone.settings).map(([id, value]) => ({ id, value })) });
      if (req.method === "PATCH") {
        zone.settings[sub] = data.value;
        return send(200, { result: { id: sub, value: data.value } });
      }
    }
    if (kind === "workers" && sub === "routes") {
      if (req.method === "GET") return send(200, { result: zone.routes });
      if (req.method === "POST") {
        const r = { id: id(), ...data };
        zone.routes.push(r);
        return send(200, { result: r });
      }
      const i = zone.routes.findIndex((r) => r.id === rid);
      if (i < 0) return send(404, {});
      if (req.method === "PUT") zone.routes[i] = { id: rid, ...data };
      if (req.method === "DELETE") zone.routes.splice(i, 1);
      return send(200, { result: { id: rid } });
    }
    return send(404, {});
  });
  return { zones, writes, server };
}

async function withFake(fn) {
  const fake = fakeCloudflare();
  await new Promise((r) => fake.server.listen(0, "127.0.0.1", r));
  try {
    await fn(fake, `http://127.0.0.1:${fake.server.address().port}`);
  } finally {
    fake.server.close();
  }
}

const run = async (api, origin, dryRun) => {
  const lines = [];
  const conflicts = await apply({ config, token: "test-token", api, origin, dryRun, log: (l) => lines.push(l) });
  return { conflicts, lines };
};

test("dry run plans and changes nothing; a run converges; the next run plans nothing", () =>
  withFake(async (fake, api) => {
    const origin = { dns: NLB, ips: [] };
    const dry = await run(api, origin, true);
    assert.equal(fake.writes.length, 0);
    assert.equal(dry.conflicts, 0);
    assert.ok(dry.lines.includes("roll.cloud: - delete CNAME old.roll.cloud → origin.us-east-1.roll.cloud"), dry.lines.join("\n"));
    assert.ok(dry.lines.includes("roll.cloud: + create CNAME origin.us-east-1.roll.cloud → " + NLB));
    assert.ok(dry.lines.some((l) => l.startsWith("roll.cloud: ~ update CNAME api.roll.cloud → elsewhere.example")));
    assert.ok(dry.lines.includes('roll.site: ~ setting ssl: "flexible"  ⇒  "strict"'));
    assert.ok(dry.lines.at(-1).startsWith("dry run:"));

    const real = await run(api, origin, false);
    assert.equal(real.conflicts, 0);
    const cloud = fake.zones["roll.cloud"];
    const names = cloud.records.map((r) => `${r.type} ${r.name}`).sort();
    assert.ok(names.includes("TXT roll.cloud"), "unmanaged record kept");
    assert.ok(!names.includes("CNAME old.roll.cloud"), "stale managed record deleted");
    const api_ = cloud.records.find((r) => r.name === "api.roll.cloud");
    assert.equal(api_.content, "origin.us-east-1.roll.cloud");
    assert.equal(api_.proxied, true);
    assert.equal(cloud.records.find((r) => r.name === "*.pg.us-east-1.roll.cloud").proxied, false);
    assert.equal(cloud.settings.ssl, "strict");
    assert.equal(cloud.settings.security_header.strict_transport_security.enabled, true);
    const site = fake.zones["roll.site"];
    assert.equal(site.settings.min_tls_version, "1.2");
    assert.deepEqual(site.routes.map((r) => `${r.pattern} ${r.script ?? "-"}`).sort(), ["*.preview.roll.site/* -", "*.roll.site/* roll-cdn", "other.roll.site/* someone-else"]);

    fake.writes.length = 0;
    const again = await run(api, origin, false);
    assert.equal(fake.writes.length, 0, fake.writes.join("\n"));
    assert.deepEqual(again.lines, ["nothing to change"]);
  }));

test("switching the origin to Elastic IPs replaces the CNAME with A records", () =>
  withFake(async (fake, api) => {
    await run(api, { dns: NLB, ips: [] }, false);
    await run(api, { dns: NLB, ips: ["3.0.0.1", "3.0.0.2", "3.0.0.3"] }, false);
    const origin = fake.zones["roll.cloud"].records.filter((r) => r.name === "origin.us-east-1.roll.cloud");
    assert.deepEqual(origin.map((r) => `${r.type} ${r.content}`).sort(), ["A 3.0.0.1", "A 3.0.0.2", "A 3.0.0.3"]);
  }));

test("an unmanaged record in the way is a conflict, not overwritten", () =>
  withFake(async (fake, api) => {
    fake.zones["roll.cloud"].records.push({ id: "mine", type: "A", name: "git.roll.cloud", content: "192.0.2.1", proxied: false, ttl: 1 });
    const { conflicts, lines } = await run(api, { dns: NLB, ips: [] }, false);
    assert.equal(conflicts, 1);
    assert.ok(lines.some((l) => l.includes("! conflict: CNAME git.roll.cloud")));
    assert.equal(fake.zones["roll.cloud"].records.find((r) => r.id === "mine").content, "192.0.2.1");
  }));
