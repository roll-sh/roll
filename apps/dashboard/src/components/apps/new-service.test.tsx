import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { NewServiceSheet } from "./new-service";

const apps = vi.hoisted(() => ({ plan: vi.fn(), roots: vi.fn(async () => ({ roots: [] })), create: vi.fn(), quote: vi.fn(async () => null) }));
vi.mock("@/lib/apps-api", async (orig) => ({ ...(await orig<typeof import("@/lib/apps-api")>()), appsApi: apps }));
const code = vi.hoisted(() => ({ repos: vi.fn(), github: vi.fn(), owners: vi.fn(async (): Promise<{ name: string }[]> => []), createRepo: vi.fn(), repo: vi.fn(), land: vi.fn() }));
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
    render(<NewServiceSheet project={project} spot={{ x: 48, y: 48 }} onClose={() => {}} onCreated={created} onImported={() => {}} />);

    const fresh = await screen.findByRole("button", { name: /new postgres/i });
    await waitFor(() => expect(fresh).toHaveAttribute("aria-pressed", "true"));
    expect(screen.getByRole("button", { name: /new redis/i })).toHaveAttribute("aria-pressed", "false");
    await userEvent.click(screen.getByRole("button", { name: "Deploy" }));

    await waitFor(() => expect(created).toHaveBeenCalled());
    expect(ws.make).toHaveBeenCalledWith("prj_1", "postgres", "shop-db", expect.objectContaining({ x: expect.any(Number) }));
    expect(apps.create.mock.calls[0][0].links).toEqual([{ kind: "postgres", ref: "pg_9" }]);
  });

  it("starts a repo from a code starter and deploys the service following it", async () => {
    code.repos.mockResolvedValueOnce([]).mockResolvedValue([{ id: "r2", full_name: "acme/api" }]);
    code.github.mockRejectedValue(new Error("no app"));
    code.owners.mockResolvedValue([{ name: "acme" }]);
    code.createRepo.mockResolvedValue({ full_name: "acme/api", default_branch: "main", head: null });
    code.land.mockResolvedValue({ status: "landed", changeset: "cs1", git_commit: null, ref: "main", generation: 1 });
    apps.plan.mockResolvedValue({ provider: "node", framework: "hono", language_version: null, install: null, build: null, start: "node index.js", port: 3000, dockerfile: "", notes: [], uses: [] });
    apps.create.mockResolvedValue({ id: "svc_2" });
    const created = vi.fn();
    render(<NewServiceSheet project={project} spot={{ x: 48, y: 48 }} onClose={() => {}} onCreated={created} onImported={() => {}} />);

    await userEvent.selectOptions(await screen.findByRole("combobox", { name: "Starter" }), "hono");
    await userEvent.type(screen.getByRole("textbox", { name: "Repo name" }), "api");
    await userEvent.click(screen.getByRole("button", { name: "Create repo" }));

    await waitFor(() => expect(code.land).toHaveBeenCalled());
    expect(code.createRepo).toHaveBeenCalledWith(expect.objectContaining({ owner: "acme", name: "api", source: "native", visibility: "private" }));
    const [repo, write] = code.land.mock.calls[0];
    expect(repo).toBe("acme/api");
    expect(write.base).toBeNull();
    expect(JSON.parse(write.files["package.json"].content).name).toBe("api");
    await waitFor(() => expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled());
    await userEvent.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(created).toHaveBeenCalled());
    expect(apps.create).toHaveBeenLastCalledWith(expect.objectContaining({ source: { kind: "repo", repo: "acme/api" } }));
  });
});
