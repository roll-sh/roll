import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { CanvasProjectsProvider, type CanvasProjects } from "./machine-projects";
import { MachineNode, ProjectNode } from "./nodes";
import { machine, mirrored, project } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";

const DOCS = project({ id: "prj_docs", name: "handbook", path: "/Users/ada/handbook" });
const CLI = project({ id: "prj_cli", name: "cli", path: "/Users/ada/cli", repo: { owner: "acme", repo: "cli" } });

function setup(unused: string[], over: Partial<CanvasProjects> = {}) {
  const startNew = vi.fn();
  const h = agentsHarness({
    entities: [mirrored("project", project(), 1), mirrored("project", DOCS, 2), mirrored("project", CLI, 3)],
    ctx: { startNew },
  });
  const keep = vi.fn();
  const value: CanvasProjects = { unused: new Map([[machine().id, unused]]), kept: () => false, keep, ...over };
  renderAgents(
    <CanvasProjectsProvider value={value}>
      <MachineNode machine={machine()} />
      <ProjectNode project={project()} machine={machine()} />
    </CanvasProjectsProvider>,
    h,
  );
  return { h, startNew, keep };
}

afterEach(() => resetAgentsSurface());

describe("a machine's projects without a card", () => {
  it("is one line with their tiles; nothing when every project has its card", () => {
    setup([]);
    expect(screen.queryByRole("button", { name: /more project/ })).not.toBeInTheDocument();
  });

  it("opens on a click into rows: the row starts an agent there (and closes it), the pin keeps it on the view, its ⋯ has the project's actions (settings)", async () => {
    const { startNew, keep } = setup(["prj_cli", "prj_docs"]);
    const line = screen.getByRole("button", { name: "2 more projects: cli, handbook" });
    await userEvent.click(line);
    const list = await screen.findByRole("dialog", { name: "Projects on ada-mbp" });
    const rows = within(list).getAllByRole("listitem");
    expect(rows.map((r) => r.textContent)).toEqual([expect.stringContaining("acme/cli"), expect.stringContaining("~/handbook")]);
    await userEvent.click(within(rows[0]).getByRole("button", { name: /^cli/ }));
    expect(startNew).toHaveBeenCalledWith("prj_cli");

    await userEvent.click(line);
    const again = await screen.findByRole("dialog", { name: "Projects on ada-mbp" });
    await userEvent.click(within(again).getAllByRole("button", { name: "Show on the canvas" })[1]);
    expect(keep).toHaveBeenCalledWith("prj_docs", true);
    await userEvent.click(within(again).getByRole("button", { name: "More for cli" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Settings" }));
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: "p:prj_cli" });
  });

  it("opens on hover too", async () => {
    setup(["prj_docs"]);
    await userEvent.hover(screen.getByRole("button", { name: "1 more project: handbook" }));
    expect(await screen.findByRole("dialog", { name: "Projects on ada-mbp" }, { timeout: 2000 })).toBeInTheDocument();
  });

  it("says “3 projects” when none of the machine's have a card, and adds one from the list", async () => {
    setup(["prj_1", "prj_cli", "prj_docs"]);
    await userEvent.click(screen.getByRole("button", { name: "3 projects: roll, cli, handbook" }));
    const list = await screen.findByRole("dialog", { name: "Projects on ada-mbp" });
    expect(within(list).getByText("Projects on ada-mbp")).toBeInTheDocument();
    await userEvent.click(within(list).getByRole("button", { name: "Add a project" }));
    expect(screen.queryByRole("dialog", { name: "Projects on ada-mbp" })).not.toBeInTheDocument();
  });

  it("a pinned project's card can be unpinned from its menu", async () => {
    const { keep } = setup([], { kept: (p) => p === project().id });
    await userEvent.click(screen.getByRole("button", { name: "Project" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Unpin from the canvas" }));
    expect(keep).toHaveBeenCalledWith(project().id, false);
  });
});
