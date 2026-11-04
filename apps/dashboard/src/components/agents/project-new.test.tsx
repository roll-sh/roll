import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Command, Reply } from "@/lib/agents-api";
import { NewProject } from "./project-new";
import { ProjectNode } from "./nodes";
import { machine, mirrored, project, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";

const repos = [
  { owner: "acme", name: "roll", full_name: "acme/roll", description: "keep the code rolling", visibility: "public" as const, updated_at: 2 },
  { owner: "acme", name: "web", full_name: "acme/web", description: null, visibility: "private" as const, updated_at: 1 },
];

describe("NewProject", () => {
  it("adds a folder from the machine", async () => {
    const reply = (c: Command): Reply =>
      c.type === "fs.list" ? { type: "entries", entries: [{ name: "roll", dir: true, size: 0 }, { name: ".config", dir: true, size: 0 }, { name: "a.txt", dir: false, size: 3 }] } : { type: "created", id: "prj_new" };
    const h = agentsHarness({ reply, ctx: { services: fakeServices() } });
    const onAdded = vi.fn();
    renderAgents(<NewProject machine={machine()} onClose={vi.fn()} onAdded={onAdded} />, h);
    const folders = await screen.findByRole("list", { name: "Folders" });
    // Hidden folders and files stay out.
    expect(within(folders).queryByText(".config")).not.toBeInTheDocument();
    await userEvent.click(within(folders).getByRole("button", { name: "roll" }));
    await userEvent.click(screen.getByRole("button", { name: "Add this folder" }));
    await waitFor(() => expect(onAdded).toHaveBeenCalledWith("prj_new"));
    expect(h.command).toHaveBeenLastCalledWith("mch_1", expect.any(String), { type: "project.add", path: "/Users/ada/roll" });
  });

  it("clones one of the owner's roll code repos, retrying with the same command id", async () => {
    let attempts = 0;
    const reply = (c: Command): Reply => {
      if (c.type === "fs.list") return { type: "entries", entries: [] };
      return ++attempts === 1 ? { type: "error", code: "timeout", message: "no reply in time" } : { type: "created", id: "prj_roll" };
    };
    const h = agentsHarness({ reply, ctx: { services: fakeServices({ repos: vi.fn(async () => repos) }) } });
    const onAdded = vi.fn();
    renderAgents(<NewProject machine={machine()} onClose={vi.fn()} onAdded={onAdded} />, h);
    await userEvent.click(screen.getByRole("tab", { name: "Clone a repo" }));
    const list = await screen.findByRole("list", { name: "Repos" });
    expect(within(list).getByText("keep the code rolling")).toBeInTheDocument();
    await userEvent.click(within(list).getByRole("button", { name: /^roll/ }));
    expect(await screen.findByText("no reply in time")).toBeInTheDocument();
    await userEvent.click(within(list).getByRole("button", { name: /^roll/ }));
    await waitFor(() => expect(onAdded).toHaveBeenCalledWith("prj_roll"));
    const clones = h.command.mock.calls.filter(([, , c]) => c.type === "project.add");
    expect(clones.map(([, , c]) => c)).toEqual([
      { type: "project.add", path: "", clone: { owner: "acme", repo: "roll" } },
      { type: "project.add", path: "", clone: { owner: "acme", repo: "roll" } },
    ]);
    expect(clones[0][1]).toBe(clones[1][1]);
  });

  it("starts a new roll code repo as a project", async () => {
    const createProject = vi.fn(async (): Promise<Reply> => ({ type: "created", id: "prj_site" }));
    const h = agentsHarness({ reply: () => ({ type: "entries", entries: [] }), ctx: { services: fakeServices({ createProject }) } });
    const onAdded = vi.fn();
    renderAgents(<NewProject machine={machine()} onClose={vi.fn()} onAdded={onAdded} />, h);
    await userEvent.click(screen.getByRole("tab", { name: "New repo" }));
    expect(screen.getByRole("button", { name: "Create and clone" })).toBeDisabled();
    await userEvent.type(screen.getByRole("textbox", { name: "Repo name" }), "site");
    await userEvent.type(screen.getByRole("textbox", { name: "Description" }), "The site");
    await userEvent.click(screen.getByRole("checkbox", { name: "Private" }));
    await userEvent.click(screen.getByRole("button", { name: "Create and clone" }));
    await waitFor(() => expect(onAdded).toHaveBeenCalledWith("prj_site"));
    expect(createProject).toHaveBeenCalledWith("acme", "mch_1", { id: expect.stringMatching(/^cmd_/), repo: { name: "site", description: "The site", visibility: "public" } });
  });

  it("a refused create shows the daemon's answer", async () => {
    const createProject = vi.fn(async (): Promise<Reply> => ({ type: "error", code: "machine_offline", message: "the machine isn't connected" }));
    renderAgents(<NewProject machine={machine()} onClose={vi.fn()} onAdded={vi.fn()} />, agentsHarness({ reply: () => ({ type: "entries", entries: [] }), ctx: { services: fakeServices({ createProject }) } }));
    await userEvent.click(screen.getByRole("tab", { name: "New repo" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Repo name" }), "site");
    await userEvent.click(screen.getByRole("button", { name: "Create and clone" }));
    expect(await screen.findByText("the machine isn't connected")).toBeInTheDocument();
  });
});

describe("ProjectNode", () => {
  it("a project with no threads offers starting one first", async () => {
    const startNew = vi.fn();
    const { unmount } = renderAgents(<ProjectNode project={project()} machine={machine()} />, agentsHarness({ ctx: { startNew } }));
    const start = screen.getByRole("button", { name: "Start an agent" });
    expect(start.className).toContain("bg-fg");
    await userEvent.click(start);
    expect(startNew).toHaveBeenCalledWith("prj_1");
    unmount();
    renderAgents(<ProjectNode project={project()} machine={machine()} />, agentsHarness({ entities: [mirrored("thread", thread(), 1)], ctx: { startNew } }));
    expect(screen.queryByRole("button", { name: "Start an agent" })).toBeNull();
    expect(screen.getByRole("button", { name: "New agent here" })).toBeInTheDocument();
  });
});
