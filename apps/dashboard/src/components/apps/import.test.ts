import { describe, expect, it } from "vitest";
import type { Link, ProjectImport, Service } from "@/lib/apps-api";
import { planImport, sealedFor, stepLine, variablesFor } from "./import";

const svc = (name: string, source: Service["source"], run: Partial<Service["run"]> = {}): Service =>
  ({ id: `svc_${name}`, project: "prj_a", name, source, build: { builder: "auto" }, run: { type: "web", min_instances: 0, max_instances: 3, concurrency: 50, cpu: 0.5, memory_mb: 512, ...run }, registry: null, created_at: 1, environments: [] }) as unknown as Service;

describe("importing a project's file", () => {
  it("plans as roll up --all does: what's there is matched by name, only what differs changes, another source stops it", () => {
    const imp: ProjectImport = {
      roll_toml: "",
      notes: [],
      databases: { db: { kind: "postgres" }, cache: { kind: "redis" } },
      apps: {
        web: { source: { kind: "image", image: "ghcr.io/acme/web:2" }, branch: null, run: { port: 3000, min_instances: 1 }, links: [{ kind: "postgres", name: "db" }], variables: {}, generate: [], ask: [] },
        jobs: { source: { kind: "upload", root: "jobs" }, branch: null, run: { type: "worker" }, links: [{ kind: "redis", name: "cache" }], variables: { MODE: "x" }, generate: [], ask: [] },
        api: { source: { kind: "repo", repo: "acme/api", root: null }, branch: "main", run: {}, links: [], variables: {}, generate: [], ask: [] },
      },
    };
    const linked: Link[] = [{ kind: "postgres", ref: "pg_1" } as Link];
    const have = {
      services: new Map([
        ["web", { service: svc("web", { kind: "image", image: "ghcr.io/acme/web:1" }, { port: 3000 }), links: linked }],
        ["api", { service: svc("api", { kind: "image", image: "x" }), links: [] }],
      ]),
      dbs: new Map([["postgres:db", "pg_1"]]),
    };
    expect(planImport(imp, have).map((s) => { const l = stepLine(s); return `${l.mark} ${l.text}`; })).toEqual([
      "= postgres database db",
      "+ redis database cache",
      "= service web (image ghcr.io/acme/web:1)",
      "~ settings web (min_instances)",
      "↑ deploy web",
      "+ service jobs (from jobs): `roll up --all` uploads its code",
      "~ link jobs to cache",
      "! api has an image source; the file makes it a repo one: rename one of them",
    ]);
    // A new service's variables: what's made for it, and what reads as a secret, is sealed from
    // the start; a reference isn't.
    const app = { source: { kind: "image" as const, image: "x" }, branch: null, run: {}, links: [], variables: { NODE_ENV: "production" }, generate: ["SESSION_SECRET"], ask: [{ name: "STRIPE_KEY", ask: "Your Stripe key", default: null }, { name: "REGION", ask: "Where", default: "eu" }] };
    const vars = variablesFor(app, { STRIPE_KEY: "sk_test_1" }, () => "made");
    expect(vars).toEqual({ NODE_ENV: "production", SESSION_SECRET: "made", STRIPE_KEY: "sk_test_1", REGION: "eu" });
    expect(sealedFor(app, { ...vars, API_TOKEN: "${{ api.TOKEN }}" })).toEqual(["SESSION_SECRET", "STRIPE_KEY"]);
  });

});
