import { describe, expect, it } from "vitest";
import type { Service } from "@/lib/apps-api";
import { formOf, patchOf } from "./settings";

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
});
