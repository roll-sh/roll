import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { ProjectDrift } from "./project-drift";

const api = vi.hoisted(() => ({ services: vi.fn(), importProject: vi.fn() }));
vi.mock("@/lib/apps-api", async (orig) => ({ ...(await orig<typeof import("@/lib/apps-api")>()), appsApi: api }));
const ws = vi.hoisted(() => ({ state: { inventory: new Map([["postgres:pg_1", { kind: "postgres", ref: "pg_1", name: "db" }]]) } }));
vi.mock("@/lib/workspace-store", () => ({ useWorkspace: () => ws.state }));

describe("ProjectDrift", () => {
  it("names what a repo's roll.toml declares that the project lacks, and opens its import", async () => {
    api.services.mockResolvedValue([{ id: "svc_1", name: "web", source: { kind: "repo", repo: "acme/shop", root: null } }]);
    api.importProject.mockResolvedValue({ roll_toml: "", notes: [], apps: { web: {}, worker: {} }, databases: { db: { kind: "postgres" }, cache: { kind: "redis" } } });
    const project = { id: "prj_1", name: "shop", resources: [{ kind: "postgres", ref: "pg_1" }] } as unknown as WorkspaceProject;
    render(<ProjectDrift project={project} />);
    const pill = await screen.findByRole("link", { name: /2 new/ });
    expect(pill).toHaveAttribute("href", "/dashboard/deploy?repo=acme%2Fshop&project=prj_1");
    expect(pill.getAttribute("title")).toContain("worker, cache");
    expect(api.importProject).toHaveBeenCalledWith({ repo: "acme/shop" });
  });
});
