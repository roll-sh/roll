import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Deployment, Environment } from "@/lib/apps-api";
import { Deployments } from "./deployments";
import type { Runner } from "./shared";

const api = vi.hoisted(() => ({ rollback: vi.fn(), cancel: vi.fn(), deploys: vi.fn(), deployment: vi.fn(), buildLog: vi.fn(), commits: vi.fn(), sbom: vi.fn(), vulnerabilities: vi.fn() }));
vi.mock("@/lib/apps-api", async (orig) => ({ ...(await orig<typeof import("@/lib/apps-api")>()), appsApi: api }));

const T0 = 1_760_000_000_000;
const dep = (id: string, status: Deployment["status"], over: Partial<Deployment> = {}): Deployment =>
  ({ id, service: "svc_1", environment: "production", status, trigger: "manual", actor: null, revision: null, message: id, image: `img@${id}`, plan: null, rollback_of: null, error: null, created_at: T0, started_at: T0, finished_at: T0, ready_ms: 900, ...over }) as Deployment;
const env = { name: "production", active: { id: "dep_live", revision: null, created_at: T0 } } as unknown as Environment;
const runner: Runner = { busy: null, error: null, failed: null, setError: () => {}, run: async (_k, fn) => fn() } as Runner;

describe("Deployments", () => {
  beforeEach(() => {
    for (const f of [api.buildLog, api.commits, api.vulnerabilities]) f.mockResolvedValue([]);
    api.deployment.mockImplementation(async (_s: string, _e: string, id: string) => dep(id, "active"));
    api.sbom.mockResolvedValue(null);
  });

  it("rolls back to one that was live only after asking, and offers nothing for a pruned one", async () => {
    api.rollback.mockResolvedValue({});
    const deploys = [dep("dep_live", "active"), dep("dep_before", "inactive"), dep("dep_old", "inactive", { pruned_at: T0 })];
    render(<Deployments service="svc_1" env={env} deploys={deploys} runner={runner} />);
    const back = screen.getAllByRole("button", { name: /roll back/i });
    expect(back).toHaveLength(1);
    await userEvent.click(back[0]);
    expect(api.rollback).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: /sure\? roll production back/i }));
    await waitFor(() => expect(api.rollback).toHaveBeenCalledWith("svc_1", "production", "dep_before", false));
  });

  it("puts a ready deployment live the same way", async () => {
    api.rollback.mockResolvedValue({});
    render(<Deployments service="svc_1" env={env} deploys={[dep("dep_ready", "ready"), dep("dep_live", "active")]} runner={runner} />);
    await userEvent.click(screen.getByRole("button", { name: /put live/i }));
    await userEvent.click(screen.getByRole("button", { name: /sure\? put it live in production/i }));
    await waitFor(() => expect(api.rollback).toHaveBeenCalledWith("svc_1", "production", "dep_ready", false));
  });
});
