#!/usr/bin/env node
// Converges roll cloud's Cloudflare zones on zones.json: DNS records, zone settings and Worker
// routes. Idempotent: a second run plans nothing. It only changes or deletes records that carry
// zones.json's `comment` (the ones it made), and Worker routes for patterns it lists or that
// run a script it lists; everything else in the zones is left alone.
//
//   node deploy/cloud/cloudflare/apply.mjs [--dry-run] [--stack roll-cloud]
//        [--origin-dns <nlb dns name> | --origin-ips <ip,ip,ip>] [--file zones.json]
//
// CLOUDFLARE_API_TOKEN: Zone Read, DNS Edit, Zone Settings Edit and Workers Routes Edit on both
// zones. The origin (the NLB) comes from the stack's outputs (`--stack`, through the AWS CLI)
// or is given; with Elastic IPs it's A records, else a CNAME. --dry-run reads and prints the
// plan without changing anything. No dependencies.
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

const DEFAULT_API = "https://api.cloudflare.com/client/v4";

/** The Cloudflare API: JSON in and out; throws on any error Cloudflare reports. */
function client(api, token) {
  return async function cf(method, path, body) {
    const res = await fetch(api + path, {
      method,
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const json = await res.json().catch(() => ({}));
    if (!res.ok || json.success === false) {
      const errors = (json.errors ?? []).map((e) => `${e.code}: ${e.message}`).join("; ");
      throw new Error(`${method} ${path}: HTTP ${res.status}${errors ? ` (${errors})` : ""}`);
    }
    return json;
  };
}

/** Every page of a list endpoint. */
async function list(cf, path) {
  const out = [];
  for (let page = 1; ; page++) {
    const sep = path.includes("?") ? "&" : "?";
    const json = await cf("GET", `${path}${sep}page=${page}&per_page=100`);
    out.push(...(json.result ?? []));
    const pages = json.result_info?.total_pages ?? 1;
    if (page >= pages) return out;
  }
}

/** The records zones.json asks for in one zone, with `origin` expanded. */
export function desiredRecords(zone, config, origin) {
  const out = [];
  for (const r of zone.records ?? []) {
    if (r.type === "origin") {
      if (origin.ips?.length) for (const ip of origin.ips) out.push({ name: r.name, type: "A", content: ip, proxied: false });
      else if (origin.dns) out.push({ name: r.name, type: "CNAME", content: origin.dns, proxied: false });
      else throw new Error(`${r.name} is the origin: pass --stack, --origin-dns or --origin-ips`);
    } else {
      out.push({ name: r.name, type: r.type, content: r.content, proxied: Boolean(r.proxied) });
    }
  }
  return out.map((r) => ({ ...r, ttl: 1, comment: config.comment }));
}

const key = (r) => `${r.type} ${r.name.toLowerCase()}`;
const sameContent = (a, b) => a.content.replace(/\.$/, "").toLowerCase() === b.content.replace(/\.$/, "").toLowerCase();
const sameRecord = (want, have) =>
  sameContent(want, have) && Boolean(have.proxied) === want.proxied && have.comment === want.comment && (want.proxied || have.ttl === want.ttl);
const describe = (r) => `${r.type} ${r.name} → ${r.content}${r.proxied ? " (proxied)" : ""}`;
// A CNAME can't share its name with other records, except at the apex (flattened), where
// TXT, MX and the like stay beside it; addresses never do.
const ADDRESSES = new Set(["A", "AAAA", "CNAME"]);
const clashes = (a, b, zoneName) =>
  a.name.toLowerCase() === b.name.toLowerCase() &&
  a.type !== b.type &&
  (a.type === "CNAME" || b.type === "CNAME") &&
  (a.name.toLowerCase() !== zoneName.toLowerCase() || (ADDRESSES.has(a.type) && ADDRESSES.has(b.type)));

/**
 * The changes that bring `existing` (a zone's records) to `want`: only records with the
 * managed comment are changed or deleted; an unmanaged record in the way is a conflict.
 */
export function planRecords(want, existing, comment, zoneName) {
  const managed = existing.filter((r) => r.comment === comment);
  const plan = { create: [], update: [], delete: [], conflicts: [] };
  const groups = new Map();
  for (const r of want) groups.set(key(r), [...(groups.get(key(r)) ?? []), r]);
  const seen = new Set();
  for (const [k, wants] of groups) {
    seen.add(k);
    const haves = managed.filter((r) => key(r) === k);
    const left = [];
    // Same content first (A records at one name), then reuse what's left, then create.
    for (const w of wants) {
      const i = haves.findIndex((h) => sameContent(w, h));
      if (i < 0) left.push(w);
      else {
        const [h] = haves.splice(i, 1);
        if (!sameRecord(w, h)) plan.update.push({ id: h.id, from: h, to: w });
      }
    }
    for (const w of left) {
      const h = haves.shift();
      if (h) plan.update.push({ id: h.id, from: h, to: w });
      else {
        const blocker = existing.find((e) => e.comment !== comment && (key(e) === k || clashes(e, w, zoneName)));
        if (blocker) plan.conflicts.push({ want: w, have: blocker });
        else plan.create.push(w);
      }
    }
    for (const h of haves) plan.delete.push(h);
  }
  for (const h of managed) if (!seen.has(key(h))) plan.delete.push(h);
  return plan;
}

/** Zone settings that differ from zones.json. */
export function planSettings(wantSettings, current) {
  const out = [];
  for (const [id, value] of Object.entries(wantSettings ?? {})) {
    const have = current[id];
    if (!matches(value, have)) out.push({ id, value, have });
  }
  return out;
}

/** `want` is a subset of `have` (objects compared field by field; Cloudflare adds fields). */
function matches(want, have) {
  if (want !== null && typeof want === "object") {
    if (have === null || typeof have !== "object") return false;
    return Object.entries(want).every(([k, v]) => matches(v, have[k]));
  }
  return want === have;
}

/** Worker routes: create or fix the listed patterns; delete routes of listed scripts not listed. */
export function planRoutes(wantRoutes, existing) {
  const plan = { create: [], update: [], delete: [] };
  const scripts = new Set((wantRoutes ?? []).map((r) => r.script).filter(Boolean));
  for (const w of wantRoutes ?? []) {
    const have = existing.find((r) => r.pattern === w.pattern);
    if (!have) plan.create.push(w);
    else if ((have.script || null) !== w.script) plan.update.push({ id: have.id, from: have, to: w });
  }
  for (const r of existing) if (r.script && scripts.has(r.script) && !(wantRoutes ?? []).some((w) => w.pattern === r.pattern)) plan.delete.push(r);
  return plan;
}

/** The NLB's DNS name and Elastic IPs from the CloudFormation stack's outputs. */
function originFromStack(stack, region) {
  const json = execFileSync(
    "aws",
    ["cloudformation", "describe-stacks", "--stack-name", stack, "--region", region, "--query", "Stacks[0].Outputs", "--output", "json"],
    { encoding: "utf8" },
  );
  const outputs = Object.fromEntries(JSON.parse(json).map((o) => [o.OutputKey, o.OutputValue]));
  return { dns: outputs.LoadBalancerDnsName, ips: outputs.ElasticIps ? outputs.ElasticIps.split(",") : [] };
}

/** Plans (and unless dryRun, makes) every change; returns the number of conflicts. */
export async function apply({ config, token, api = DEFAULT_API, origin, dryRun, log = console.log }) {
  const cf = client(api, token);
  let conflicts = 0;
  let changes = 0;
  for (const [zoneName, zone] of Object.entries(config.zones)) {
    const zones = (await cf("GET", `/zones?name=${encodeURIComponent(zoneName)}`)).result ?? [];
    if (zones.length !== 1) throw new Error(`zone ${zoneName} isn't in this Cloudflare account (add it first)`);
    const zoneId = zones[0].id;
    const say = (line) => log(`${zoneName}: ${line}`);

    // Records.
    const existing = await list(cf, `/zones/${zoneId}/dns_records`);
    const plan = planRecords(desiredRecords(zone, config, origin), existing, config.comment, zoneName);
    for (const c of plan.conflicts) say(`! conflict: ${describe(c.want)} is blocked by an unmanaged ${describe(c.have)}; remove it or add the comment "${config.comment}" to hand it over`);
    conflicts += plan.conflicts.length;
    // Deletes first: a CNAME can replace A records at the same name only once they're gone.
    for (const r of plan.delete) {
      say(`- delete ${describe(r)}`);
      if (!dryRun) await cf("DELETE", `/zones/${zoneId}/dns_records/${r.id}`);
    }
    for (const u of plan.update) {
      say(`~ update ${describe(u.from)}  ⇒  ${describe(u.to)}`);
      if (!dryRun) await cf("PUT", `/zones/${zoneId}/dns_records/${u.id}`, u.to);
    }
    for (const r of plan.create) {
      say(`+ create ${describe(r)}`);
      if (!dryRun) await cf("POST", `/zones/${zoneId}/dns_records`, r);
    }
    changes += plan.delete.length + plan.update.length + plan.create.length;

    // Settings.
    const current = Object.fromEntries(((await cf("GET", `/zones/${zoneId}/settings`)).result ?? []).map((s) => [s.id, s.value]));
    for (const s of planSettings(zone.settings, current)) {
      say(`~ setting ${s.id}: ${JSON.stringify(s.have)}  ⇒  ${JSON.stringify(s.value)}`);
      if (!dryRun) await cf("PATCH", `/zones/${zoneId}/settings/${s.id}`, { value: s.value });
      changes++;
    }

    // Worker routes.
    if (zone.workerRoutes) {
      const routes = (await cf("GET", `/zones/${zoneId}/workers/routes`)).result ?? [];
      const rp = planRoutes(zone.workerRoutes, routes);
      const body = (r) => (r.script ? { pattern: r.pattern, script: r.script } : { pattern: r.pattern });
      for (const r of rp.delete) {
        say(`- route ${r.pattern} → ${r.script}`);
        if (!dryRun) await cf("DELETE", `/zones/${zoneId}/workers/routes/${r.id}`);
      }
      for (const u of rp.update) {
        say(`~ route ${u.to.pattern}: ${u.from.script || "no Worker"}  ⇒  ${u.to.script ?? "no Worker"}`);
        if (!dryRun) await cf("PUT", `/zones/${zoneId}/workers/routes/${u.id}`, body(u.to));
      }
      for (const r of rp.create) {
        say(`+ route ${r.pattern} → ${r.script ?? "no Worker"}`);
        if (!dryRun) await cf("POST", `/zones/${zoneId}/workers/routes`, body(r));
      }
      changes += rp.delete.length + rp.update.length + rp.create.length;
    }
  }
  log(changes === 0 ? "nothing to change" : dryRun ? `dry run: ${changes} change(s) planned, none made` : `${changes} change(s) made`);
  return conflicts;
}

async function main() {
  const { values } = parseArgs({
    options: {
      "dry-run": { type: "boolean", default: false },
      file: { type: "string", default: join(dirname(fileURLToPath(import.meta.url)), "zones.json") },
      stack: { type: "string" },
      region: { type: "string", default: process.env.AWS_REGION ?? "us-east-1" },
      "origin-dns": { type: "string" },
      "origin-ips": { type: "string" },
    },
  });
  const token = process.env.CLOUDFLARE_API_TOKEN;
  if (!token) throw new Error("CLOUDFLARE_API_TOKEN is not set");
  const config = JSON.parse(readFileSync(values.file, "utf8"));
  const origin = values.stack
    ? originFromStack(values.stack, values.region)
    : { dns: values["origin-dns"], ips: values["origin-ips"]?.split(",").map((s) => s.trim()).filter(Boolean) ?? [] };
  const conflicts = await apply({ config, token, api: process.env.CLOUDFLARE_API_URL ?? DEFAULT_API, origin, dryRun: values["dry-run"] });
  if (conflicts > 0) process.exitCode = 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((e) => {
    console.error(`apply: ${e.message}`);
    process.exit(1);
  });
}
