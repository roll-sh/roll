import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { appsApi } from "@/lib/apps-api";
import type { WorkspaceProject } from "@/lib/workspace-api";
import type { ResourceView } from "@/lib/workspace-resources";
import type { WorkspaceState } from "@/lib/workspace-store";
import { DeployChanges } from "./deploy-changes";

vi.mock("@/lib/apps-api", async (orig) => {
  const real = await orig<typeof import("@/lib/apps-api")>();
  return { ...real, appsApi: { ...real.appsApi, pending: vi.fn(), redeploy: vi.fn() } };
});
vi.mock("@/lib/workspace-store", async (orig) => ({ ...(await orig<typeof import("@/lib/workspace-store")>()), refreshInventory: vi.fn(async () => {}) }));

const view = (ref: string, name: string, toDeploy: string[]): ResourceView => ({ kind: "service", ref, name, sub: "", state: "online", stateLabel: "Running", meta: "", href: "", facts: [], toDeploy });

describe("deploying a project's staged changes", () => {
  it("redeploys each picked environment through its own route and says how each went", async () => {
    vi.mocked(appsApi.pending).mockResolvedValue({ variables: { added: ["STRIPE_KEY"], removed: [], changed: [] }, run: [], secrets: false });
    vi.mocked(appsApi.redeploy).mockImplementation(async (id, e) => {
      if (id === "svc_api") throw new Error("the service is paused");
      return { id: `dep_${id}_${e}` } as never;
    });
    const project = { id: "prj_1", name: "shop", resources: [{ kind: "service", ref: "svc_web", x: 0, y: 0 }, { kind: "service", ref: "svc_api", x: 0, y: 0 }] } as unknown as WorkspaceProject;
    const ws = {
      staged: {},
      inventory: new Map([
        ["service:svc_web", view("svc_web", "web", ["production", "staging"])],
        ["service:svc_api", view("svc_api", "api", ["production"])],
      ]),
    } as unknown as WorkspaceState;
    render(<DeployChanges ws={ws} project={project} open onClose={() => {}} />);
    // What changed in each, named, before anything is fired.
    await waitFor(() => expect(screen.getAllByText(/STRIPE_KEY/).length).toBe(3));
    fireEvent.click(screen.getByRole("checkbox", { name: "Redeploy web in staging" }));
    fireEvent.click(screen.getByRole("button", { name: /Deploy 2/ }));
    await screen.findByText("the service is paused");
    expect(vi.mocked(appsApi.redeploy).mock.calls.map(([id, e]) => `${id}/${e}`).sort()).toEqual(["svc_api/production", "svc_web/production"]);
    // The one that failed is offered again; the one that started isn't.
    expect(screen.getByRole("button", { name: /Retry 1/ })).toBeTruthy();
  });
});
