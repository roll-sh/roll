import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Command, Machine, Reply } from "@/lib/agents-api";
import { openProjectDialog, resetProjectDialogs } from "@/lib/agents-project-dialogs";
import { resetAgentsSurface } from "@/lib/agents-surface";
import { ProjectDialogs } from "./project-actions";
import { repoNameProblem } from "./project-new";
import { mirrored, project } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, type AgentsHarness } from "@/test/agents-render";

const repos = [
  { owner: "acme", name: "roll", full_name: "acme/roll", description: "keep the code rolling", visibility: "public" as const, updated_at: 2 },
  { owner: "acme", name: "web", full_name: "acme/web", description: null, visibility: "private" as const, updated_at: 1 },
];

const folders = (c: Command): Reply | null =>
  c.type === "fs.list" ? { type: "entries", entries: [{ name: "roll", dir: true, size: 0 }, { name: "site", dir: true, size: 0 }, { name: ".config", dir: true, size: 0 }, { name: "a.txt", dir: false, size: 3 }] } : null;

/** The dialog host, opened on a machine, with the project the daemon makes appearing like the stream would. */
function dialog(opts: { reply?: (c: Command) => Reply; services?: Parameters<typeof fakeServices>[0]; machines?: Machine[]; entities?: NonNullable<Parameters<typeof agentsHarness>[0]>["entities"]; on?: string | null } = {}) {
  const startNew = vi.fn();
  let h: AgentsHarness | null = null;
  h = agentsHarness({
    machines: opts.machines,
    entities: opts.entities,
    reply: (c) => {
      const r = folders(c) ?? opts.reply?.(c) ?? { type: "created", id: "prj_new" };
      if (r.type === "created" && c.type === "project.add") h!.client.enqueue({ type: "entity", ...mirrored("project", project({ id: r.id, name: c.clone?.repo ?? c.path.split("/").at(-1) ?? "x", path: c.path || `/Users/ada/roll/acme/${c.clone?.repo}` }), 50) });
      return r;
    },
    ctx: { startNew, services: fakeServices(opts.services) },
  });
  renderAgents(<ProjectDialogs />, h);
  act(() => openProjectDialog({ kind: "add", machine: opts.on === undefined ? "mch_1" : opts.on }));
  return { h, startNew };
}

// Named by its title: "Add a project", then "Project added".
const sheet = () => screen.getByRole("dialog");

afterEach(() => {
  resetProjectDialogs();
  resetAgentsSurface();
});

describe("adding a project (one dialog)", () => {
  it("adds a folder from the machine, then offers to start an agent there", async () => {
    const { h, startNew } = dialog();
    const list = await within(sheet()).findByRole("list", { name: "Folders" });
    // Hidden folders and files stay out; the home folder itself isn't a project.
    expect(within(list).queryByText(".config")).not.toBeInTheDocument();
    expect(within(sheet()).getByRole("button", { name: /^Add / })).toBeDisabled();
    await userEvent.click(within(list).getByRole("button", { name: "roll" }));
    await userEvent.click(within(sheet()).getByRole("button", { name: "Add roll" }));
    expect(h.command).toHaveBeenCalledWith("mch_1", expect.any(String), { type: "project.add", path: "/Users/ada/roll" });
    const done = await within(sheet()).findByRole("status");
    expect(done).toHaveTextContent("roll");
    expect(done).toHaveTextContent("ada-mbp");
    await userEvent.click(within(sheet()).getByRole("button", { name: "Start an agent" }));
    expect(startNew).toHaveBeenCalledWith("prj_new");
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("clones one of the owner's repos with its steps shown, retrying with the same command id", async () => {
    let attempts = 0;
    const { h } = dialog({
      reply: (c) => (c.type === "project.add" && ++attempts === 1 ? { type: "error", code: "timeout", message: "no reply in time" } : { type: "created", id: "prj_roll" }),
      services: { repos: vi.fn(async () => repos) },
    });
    await userEvent.click(within(sheet()).getByRole("button", { name: "Clone a repo" }));
    const list = await within(sheet()).findByRole("list", { name: "Repos" });
    expect(within(list).getByText("keep the code rolling")).toBeInTheDocument();
    await userEvent.type(within(sheet()).getByRole("textbox", { name: "Find a repo" }), "zzz");
    expect(within(list).getByText("No repo matches", { exact: false })).toBeInTheDocument();
    await userEvent.clear(within(sheet()).getByRole("textbox", { name: "Find a repo" }));
    await userEvent.click(within(list).getByRole("button", { name: /^roll/ }));
    expect(await within(sheet()).findByRole("alert")).toHaveTextContent("no reply in time");
    await userEvent.click(within(within(sheet()).getByRole("list", { name: "Repos" })).getByRole("button", { name: /^roll/ }));
    expect(await within(sheet()).findByRole("status")).toHaveTextContent("acme/roll");
    const clones = h.command.mock.calls.filter(([, , c]) => c.type === "project.add");
    expect(clones.map(([, , c]) => c)).toEqual([
      { type: "project.add", path: "", clone: { owner: "acme", repo: "roll" } },
      { type: "project.add", path: "", clone: { owner: "acme", repo: "roll" } },
    ]);
    expect(clones[0][1]).toBe(clones[1][1]);
    // A fresh clone pulls merged changes unless asked not to.
    expect(h.command.mock.calls.some(([, , c]) => c.type === "project.update" && c.settings?.auto_pull === true)).toBe(true);
  });

  it("starts a new roll code repo, its name checked as it's typed", async () => {
    const createProject = vi.fn(async (): Promise<Reply> => ({ type: "created", id: "prj_site" }));
    dialog({ services: { createProject } });
    await userEvent.click(within(sheet()).getByRole("button", { name: "New repo" }));
    const create = within(sheet()).getByRole("button", { name: "Create and clone" });
    expect(create).toBeDisabled();
    await userEvent.type(within(sheet()).getByRole("textbox", { name: "Repo name" }), "my site");
    expect(within(sheet()).getByText("No spaces: use - or _ instead.")).toBeInTheDocument();
    expect(create).toBeDisabled();
    await userEvent.clear(within(sheet()).getByRole("textbox", { name: "Repo name" }));
    await userEvent.type(within(sheet()).getByRole("textbox", { name: "Repo name" }), "site");
    await userEvent.type(within(sheet()).getByRole("textbox", { name: "Description" }), "The site");
    await userEvent.click(within(sheet()).getByRole("switch", { name: "Private" }));
    await userEvent.click(create);
    await waitFor(() => expect(createProject).toHaveBeenCalledWith("acme", "mch_1", { id: expect.stringMatching(/^cmd_/), repo: { name: "site", description: "The site", visibility: "public" } }));
  });

  it("repo names: what the server takes", () => {
    expect(repoNameProblem("")).toBeNull();
    expect(repoNameProblem("site")).toBeNull();
    expect(repoNameProblem("my.site_v2-x")).toBeNull();
    expect(repoNameProblem(".hidden")).toMatch(/not starting with a dot/);
    expect(repoNameProblem("a/b")).toMatch(/only/);
    expect(repoNameProblem("site.git")).toBe("Leave off .git.");
    expect(repoNameProblem("x".repeat(101))).toBe("At most 100 characters.");
  });
});
