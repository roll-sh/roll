import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { NewServiceSheet } from "./new-service";

const apps = vi.hoisted(() => ({ plan: vi.fn(), create: vi.fn() }));
vi.mock("@/lib/apps-api", async (orig) => ({ ...(await orig<typeof import("@/lib/apps-api")>()), appsApi: apps }));
const code = vi.hoisted(() => ({ repos: vi.fn(), github: vi.fn() }));
vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig<typeof import("@/lib/code-api")>()), codeApi: code }));
const ws = vi.hoisted(() => ({
  state: { status: "ready", projects: [], inventory: new Map(), kinds: new Set(["postgres", "redis", "service"]), spend: new Map(), staged: {} },
  make: vi.fn(),
}));
vi.mock("@/lib/workspace-store", () => ({ useWorkspace: () => ws.state, workspace: { make: ws.make }, refreshInventory: async () => {}, PlaceError: class extends Error {} }));

const project = { id: "prj_1", name: "shop", resources: [] } as unknown as WorkspaceProject;

describe("NewServiceSheet", () => {
  it("makes the database the code uses and links it before the first deploy", async () => {
    code.repos.mockResolvedValue([{ id: "r1", full_name: "acme/shop" }]);
    code.github.mockRejectedValue(new Error("no app"));
    apps.plan.mockResolvedValue({ provider: "node", framework: null, language_version: null, install: null, build: null, start: "node server.js", port: 3000, dockerfile: "", notes: ["pg in package.json: it uses Postgres"], uses: ["postgres"] });
    ws.make.mockResolvedValue("pg_9");
    apps.create.mockResolvedValue({ id: "svc_1" });
    const created = vi.fn();
    render(<NewServiceSheet project={project} spot={{ x: 48, y: 48 }} onClose={() => {}} onCreated={created} />);

    const fresh = await screen.findByRole("button", { name: /new postgres/i });
    await waitFor(() => expect(fresh).toHaveAttribute("aria-pressed", "true"));
    expect(screen.getByRole("button", { name: /new redis/i })).toHaveAttribute("aria-pressed", "false");
    await userEvent.click(screen.getByRole("button", { name: "Deploy" }));

    await waitFor(() => expect(created).toHaveBeenCalled());
    expect(ws.make).toHaveBeenCalledWith("prj_1", "postgres", "shop-db", expect.objectContaining({ x: expect.any(Number) }));
    expect(apps.create.mock.calls[0][0].links).toEqual([{ kind: "postgres", ref: "pg_9" }]);
  });
});
