import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Job } from "@/lib/apps-api";
import { JobsTab } from "./jobs";
import type { Runner } from "./shared";

const api = vi.hoisted(() => ({ jobs: vi.fn(), startJob: vi.fn(), cancelJob: vi.fn(), runs: vi.fn(), cancelRun: vi.fn(), update: vi.fn() }));
vi.mock("@/lib/apps-api", async (orig) => ({ ...(await orig<typeof import("@/lib/apps-api")>()), appsApi: api, followChanges: () => () => {}, followLog: () => () => {} }));

const runner: Runner = { busy: null, error: null, failed: null, setError: () => {}, run: async (_k, fn) => fn() } as Runner;
const job = (over: Partial<Job> = {}): Job => ({ id: "job_1", deployment: "dep_1", command: "npx prisma db seed", status: "running", exit_code: null, started_at: 0, finished_at: null, ...over }) as Job;

describe("JobsTab", () => {
  it("runs a command in a fresh container and cancels it while it runs", async () => {
    api.jobs.mockResolvedValue([]);
    api.startJob.mockResolvedValue(job());
    api.cancelJob.mockResolvedValue({});
    render(<JobsTab service="svc_1" env="production" runner={runner} />);
    const run = screen.getByRole("button", { name: /^run$/i });
    expect(run).toBeDisabled();
    await userEvent.type(screen.getByLabelText("Command"), "  npx prisma db seed ");
    await userEvent.click(run);
    await waitFor(() => expect(api.startJob).toHaveBeenCalledWith("svc_1", "production", { command: "npx prisma db seed" }));
    await userEvent.click(await screen.findByRole("button", { name: /cancel/i }));
    await waitFor(() => expect(api.cancelJob).toHaveBeenCalledWith("svc_1", "production", "job_1"));
  });
});
