import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { WorkspaceProject } from "@/lib/workspace-api";
import type { WorkspaceState } from "@/lib/workspace-store";
import { NoSuchProject } from "./missing-project";

const project = (id: string): WorkspaceProject => ({ id, name: id, description: "", icon: null, created_at: 0, resources: [] });
const ready = (projects: WorkspaceProject[]): WorkspaceState => ({ status: "ready", projects, inventory: new Map(), kinds: new Set(), spend: new Map(), staged: {} });
const secrets = (id: string) => `/dashboard/projects/${id}/secrets`;

describe("a link to a project that isn't there", () => {
  it("offers the person's own projects, all of them, and a new one", () => {
    render(<NoSuchProject ws={ready(["shop", "blog", "a", "b", "c", "d"].map(project))} to={secrets} />);
    expect(screen.getByRole("link", { name: "shop" })).toHaveAttribute("href", "/dashboard/projects/shop/secrets");
    expect(screen.queryByRole("link", { name: "d" })).toBeNull();
    expect(screen.getByRole("link", { name: "All 6 projects" })).toHaveAttribute("href", "/dashboard");
    expect(screen.getByRole("button", { name: "New project" })).toBeInTheDocument();
  });

  it("with no projects, offers to make one", () => {
    render(<NoSuchProject ws={ready([])} to={secrets} />);
    expect(screen.queryByRole("listitem")).toBeNull();
    expect(screen.getByRole("link", { name: "All projects" })).toHaveAttribute("href", "/dashboard");
    expect(screen.getByRole("button", { name: "New project" })).toBeInTheDocument();
  });
});
