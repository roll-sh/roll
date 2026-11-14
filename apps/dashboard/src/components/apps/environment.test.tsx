import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Environment, Service } from "@/lib/apps-api";
import { accessLink, EnvironmentPanel } from "./environment";
import type { Runner } from "./shared";

const api = vi.hoisted(() => ({ putEnvironment: vi.fn(), promote: vi.fn(), removeEnvironment: vi.fn() }));
vi.mock("@/lib/apps-api", async (orig) => ({ ...(await orig<typeof import("@/lib/apps-api")>()), appsApi: api }));

const T0 = 1_760_000_000_000;
const summary = (name: string, live: boolean) => ({
  name,
  state: "running" as const,
  url: `https://web-${name}.roll.test`,
  active: live ? { id: `dep_${name}`, revision: null, created_at: T0 } : null,
  latest: null,
});
const service = {
  id: "svc_1",
  project: "prj_1",
  name: "web",
  source: { kind: "image", image: "nginx:1.27" },
  build: { builder: "auto" },
  run: {},
  registry: null,
  created_at: T0,
  environments: [summary("production", true), summary("staging", true)],
} as unknown as Service;
const env = (over: Partial<Environment> = {}) =>
  ({ ...summary("staging", true), service: "svc_1", branch: null, ephemeral: false, instances: [], domains: [], port: null, needs_deploy: false, private: false, next_run_at: null, crash_loop: null, created_at: T0, ...over }) as Environment;
const runner: Runner = { busy: null, error: null, failed: null, setError: () => {}, run: async (_k, fn) => fn() } as Runner;

describe("EnvironmentPanel", () => {
  it("making it private sends a fresh token and shows its link once", async () => {
    api.putEnvironment.mockResolvedValue(env({ private: true }));
    render(<EnvironmentPanel service={service} env={env()} runner={runner} onDeleted={() => {}} />);
    await userEvent.click(screen.getByRole("button", { name: /make private/i }));
    const token = api.putEnvironment.mock.calls[0][2].access.token as string;
    expect(token).toMatch(/^[a-z0-9]{32}$/);
    expect(await screen.findByText(accessLink("https://web-staging.roll.test", token))).toBeInTheDocument();
  });

  it("promotes another environment's live deployment", async () => {
    api.promote.mockResolvedValue({});
    render(<EnvironmentPanel service={service} env={env()} runner={runner} onDeleted={() => {}} />);
    await userEvent.click(screen.getByRole("button", { name: /promote production/i }));
    await waitFor(() => expect(api.promote).toHaveBeenCalledWith("svc_1", "staging", "production"));
  });
});
