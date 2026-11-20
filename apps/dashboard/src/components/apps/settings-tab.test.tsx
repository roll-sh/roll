import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Service } from "@/lib/apps-api";
import { SettingsTab } from "./settings";
import type { Runner } from "./shared";

const api = vi.hoisted(() => ({ update: vi.fn(), redeploy: vi.fn() }));
vi.mock("@/lib/apps-api", async (orig) => ({ ...(await orig<typeof import("@/lib/apps-api")>()), appsApi: api }));
vi.mock("@/lib/workspace-store", () => ({ useWorkspace: () => ({ projects: [], inventory: new Map() }) }));
vi.mock("@/components/link", () => ({ Link: ({ href, children }: { href: string; children: React.ReactNode }) => <a href={href}>{children}</a> }));
vi.mock("./sidecars", () => ({ SidecarsPanel: () => null }));
vi.mock("./activity", () => ({ ActivityPanel: () => null }));
vi.mock("./duplicate", () => ({ DuplicatePanel: () => null }));

const svc = {
  id: "svc_a",
  project: "prj_a",
  name: "web",
  source: { kind: "image", image: "nginx:1.27" },
  build: { builder: "auto" },
  run: { type: "web", port: 80, min_instances: 0, max_instances: 3, concurrency: 50, cpu: 0.5, memory_mb: 512, health_timeout_s: 60, suspend_after_s: 30, sleep_after_s: 600 },
  registry: null,
  git_login: null,
  github: null,
  links: [],
  created_at: 1,
  environments: [],
} as unknown as Service;
const runner: Runner = { busy: null, error: null, failed: null, setError: () => {}, run: async (_k, fn) => fn() } as Runner;

describe("SettingsTab", () => {
  it("saves only what was changed, after a change", async () => {
    api.update.mockResolvedValue(svc);
    render(<SettingsTab service={svc} env={null} projectName="acme" runner={runner} onDeleted={() => {}} onEnvDeleted={() => {}} />);
    const save = screen.getByRole("button", { name: /^save$/i });
    expect(save).toBeDisabled();
    const field = screen.getByLabelText(/^Concurrency/);
    await userEvent.clear(field);
    await userEvent.type(field, "80");
    await userEvent.click(save);
    await waitFor(() => expect(api.update).toHaveBeenCalledWith("svc_a", { run: { concurrency: 80 } }));
  });
});
