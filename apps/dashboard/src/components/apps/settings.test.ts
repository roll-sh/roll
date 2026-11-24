import { describe, expect, it } from "vitest";
import type { Service } from "@/lib/apps-api";
import { formOf, patchOf, rebase } from "./settings";

const svc: Service = {
  id: "svc_a",
  project: "prj_a",
  name: "web",
  source: { kind: "repo", repo: "acme/site" },
  build: { builder: "auto", build: "pnpm build", watch: ["web/**"] },
  run: { type: "web", min_instances: 0, max_instances: 3, concurrency: 50, cpu: 0.5, memory_mb: 512, health_timeout_s: 60, suspend_after_s: 30, sleep_after_s: 600 },
  registry: null,
  git_login: null,
  github: null,
  created_at: 1,
  environments: [],
};

describe("the settings form", () => {
  it("sends only what changed, and a cleared field as null (back to its default)", () => {
    const f = formOf(svc);
    expect(patchOf(svc, f)).toEqual({});
    expect(patchOf(svc, { ...f, build: "", watch: "web/**, packages/ui/**", preDeploy: "pnpm migrate" })).toEqual({
      build: { build: null, watch: ["web/**", "packages/ui/**"] },
      run: { pre_deploy: "pnpm migrate" },
    });
    expect(patchOf(svc, { ...f, watch: " " })).toEqual({ build: { watch: null } });
    // Log metrics as `name: pattern` lines; a cleared field none.
    expect(patchOf(svc, { ...f, logMetrics: "payments_failed: payment failed\n\nTimeout" })).toEqual({
      run: { log_metrics: [{ name: "payments_failed", pattern: "payment failed" }, { name: "timeout", pattern: "Timeout" }] },
    });
  });

  it("keeps route alerts when another alert is edited, and reads them as lines", () => {
    const routes = [{ route: "/api/checkout", method: "POST", error_pct: 2, p95_ms: 800 }, { route: "/api/*", p95_ms: 1500 }];
    const withRoutes = { ...svc, run: { ...svc.run, alerts: { memory_pct: 90, error_pct: 5, p95_ms: 0, routes } } };
    const f = formOf(withRoutes);
    expect(f.alertRoutes).toBe("POST /api/checkout 5xx >= 2% p95 >= 800ms\n/api/* p95 >= 1500ms");
    expect(patchOf(withRoutes, { ...f, alertMemory: "80" }).run?.alerts?.routes).toEqual(routes);
    // A line it can't read goes as it is, for the API to refuse, never dropped.
    expect(patchOf(withRoutes, { ...f, alertRoutes: "get /users/:id 5xx > 10%\n/oops" }).run?.alerts?.routes).toEqual([{ route: "/users/:id", method: "GET", error_pct: 10 }, { route: "/oops" }]);
  });

  it("writes floors by the clock as lines, a rule's windows together", () => {
    const f = formOf(svc);
    const patch = patchOf(svc, { ...f, floors: "mon 08:00..fri 18:00=3 Europe/Berlin\nsat 10:00..sat 14:00=3 Europe/Berlin\nmon 00:00..mon 06:00=1" });
    expect(patch.run?.schedule_scale).toEqual([
      { windows: [{ from: "mon 08:00", to: "fri 18:00" }, { from: "sat 10:00", to: "sat 14:00" }], min_instances: 3, timezone: "Europe/Berlin" },
      { windows: [{ from: "mon 00:00", to: "mon 06:00" }], min_instances: 1 },
    ]);
    expect(formOf({ ...svc, run: { ...svc.run, schedule_scale: patch.run?.schedule_scale ?? undefined } }).floors).toBe("mon 08:00..fri 18:00=3 Europe/Berlin\nsat 10:00..sat 14:00=3 Europe/Berlin\nmon 00:00..mon 06:00=1");
  });

  it("keeps what was edited when the service changes under the form", () => {
    const base = formOf(svc);
    const edited = { ...base, build: "pnpm build:prod" };
    // A panel beside it saved: the service's concurrency changed, the edit stays.
    const next = formOf({ ...svc, run: { ...svc.run, concurrency: 80 } });
    expect(rebase(edited, base, next)).toEqual({ ...next, build: "pnpm build:prod" });
  });

  it("edits the first stream push and keeps the rest as they are", () => {
    const second = { stream: "st_bbbbbbbbbbbb", topic: "audit", group: "web", path: "/audit" };
    const withPush = { ...svc, run: { ...svc.run, push: [{ stream: "st_aaaaaaaaaaaa", topic: "orders", group: "web", path: "/events", dead_letter: "dead", max_attempts: 3 }, second] } };
    const f = formOf(withPush);
    expect(patchOf(withPush, { ...f, pushPath: "/orders", pushDead: "" })).toEqual({ run: { push: [{ stream: "st_aaaaaaaaaaaa", topic: "orders", group: "web", path: "/orders", dead_letter: undefined }, second] } });
    expect(patchOf(withPush, { ...f, pushStream: "" })).toEqual({ run: { push: [second] } });
  });
});
