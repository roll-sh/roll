import { act, screen, waitFor, within } from "@testing-library/react";
import { StrictMode } from "react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { EMPTY_INBOX, type Command, type Machine } from "@/lib/agents-api";
import { openProjectDialog, resetProjectDialogs } from "@/lib/agents-project-dialogs";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { loadFocused, setFocused } from "@/lib/agents-views";
import { NO_CANVAS } from "./machine-projects";
import { ProjectNode } from "./nodes";
import { projectActions, ProjectDialogs, pullSentence } from "./project-actions";
import { InboxToast, showInboxToast, ThreadsSidebar } from "./threads-sidebar";
import { machine, mirrored, project, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, SIZE, type AgentsHarness } from "@/test/agents-render";

const ROLL = project({ repo: { owner: "acme", repo: "roll" }, git: { branch: "main", head: "f35f34cf", remote: "origin", dirty: false } });
const labels = (h: AgentsHarness, opts: Parameters<typeof projectActions>[2] = {}) => projectActions({ ...h.ctx, canvas: NO_CANVAS }, ROLL, opts).map((a) => a.label);
const sent = (h: AgentsHarness) => h.command.mock.calls.map(([, , c]) => c);

afterEach(() => {
  resetProjectDialogs();
  resetAgentsSurface();
  showInboxToast(null);
  localStorage.clear();
});

describe("a project's actions", () => {
  it("are one list, in one order, with what applies to the project", () => {
    const h = agentsHarness({
      entities: [mirrored("project", ROLL, 1), mirrored("thread", thread({ id: "thr_s", inbox: { ...EMPTY_INBOX, settle: { state: "settled", at: 1, by: "user" } } }), 2)],
      ctx: { startNew: vi.fn(), focusProject: vi.fn(), services: fakeServices() },
    });
    expect(labels(h, { remove: () => {} })).toEqual([
      "New agent here",
      "New sandbox for this project",
      "Focus on this project only",
      "Pull",
      "Open in roll code",
      "Settings",
      "Settled and archived (1)",
      "Remove from roll",
    ]);
    // A folder that isn't a roll repo can't be cloned onto a sandbox; a share's replay only looks.
    const folder = projectActions({ ...h.ctx, canvas: NO_CANVAS }, project({ git: null }), {}).map((a) => a.label);
    expect(folder).not.toContain("New sandbox for this project");
    expect(folder).not.toContain("Pull");
    expect(projectActions({ ...h.ctx, readOnly: true, canvas: NO_CANVAS }, ROLL, { remove: () => {} }).map((a) => a.label)).toEqual(["Focus on this project only", "Open in roll code", "Settings", "Settled and archived (1)"]);
    // On the canvas, pinning it onto the view.
    const pinned = projectActions({ ...h.ctx, canvas: { unused: new Map(), kept: () => true, keep: vi.fn() } }, ROLL).map((a) => a.label);
    expect(pinned).toContain("Unpin from the canvas");
  });

  it("pull says what it did", async () => {
    const h = agentsHarness({
      entities: [mirrored("project", ROLL, 1)],
      reply: (c) => (c.type === "project.pull" ? { type: "pulled", at: 1, outcome: { status: "pulled", branch: "main", from: "a", to: "b", commits: 3 } } : { type: "ok" }),
    });
    renderAgents(
      <>
        <ProjectNode project={ROLL} machine={machine()} />
        <InboxToast />
      </>,
      h,
    );
    await userEvent.click(screen.getByRole("button", { name: "Project" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Pull" }));
    expect(sent(h).at(-1)).toEqual({ type: "project.pull", project: ROLL.id });
    expect(await screen.findByRole("status")).toHaveTextContent("Pulled 3 commits into roll");
    expect(pullSentence("web", { status: "skipped", reason: "local_changes", message: "the checkout has uncommitted changes" })).toBe("Didn't pull web: the checkout has uncommitted changes");
    expect(pullSentence("web", { status: "up_to_date", branch: "main" })).toBe("web is up to date");
  });

  it("are on the sidebar's project header too", async () => {
    const focusProject = vi.fn();
    const h = agentsHarness({ entities: [mirrored("project", ROLL, 1), mirrored("thread", thread(), 2)], ctx: { focusProject } });
    renderAgents(<ThreadsSidebar data={h.client.data} open setOpen={() => {}} phone={false} openThread={null} />, h);
    await userEvent.click(screen.getByRole("button", { name: "roll actions" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Focus on this project only" }));
    expect(focusProject).toHaveBeenCalledWith(ROLL.id);
  });

  it("the sidebar shows only the focused project, and its × shows them all", async () => {
    const web = project({ id: "prj_web", name: "web" });
    const h = agentsHarness({ entities: [mirrored("project", ROLL, 1), mirrored("project", web, 2), mirrored("thread", thread(), 3)] });
    const onShowAll = vi.fn();
    renderAgents(<ThreadsSidebar data={h.client.data} open setOpen={() => {}} phone={false} openThread={null} only="prj_web" onShowAll={onShowAll} />, h);
    expect(screen.getByRole("listitem", { name: "web" })).toBeInTheDocument();
    expect(screen.queryByRole("listitem", { name: "roll" })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Show every project, not only web" }));
    expect(onShowAll).toHaveBeenCalled();
  });

  it("focusing is remembered per view on this device", () => {
    let all = setFocused("acme", {}, "all", "prj_1");
    all = setFocused("acme", all, "v_running", "prj_web");
    expect(loadFocused("acme")).toEqual({ all: "prj_1", v_running: "prj_web" });
    setFocused("acme", all, "all", null);
    expect(loadFocused("acme")).toEqual({ v_running: "prj_web" });
    localStorage.setItem("roll:agents:only:acme", "not json");
    expect(loadFocused("acme")).toEqual({});
  });
});

describe("a new sandbox for a project", () => {
  const BOX: Machine = { ...machine(), id: "mch_box", name: "roll-sandbox", online: false, info: null, sandbox: { backend: "docker", size: "standard", status: "creating", hourly_cents: 3 } };

  function setup() {
    const create = vi.fn(async () => ({ sandbox: { id: "sbx_1" } as never, machine: BOX }));
    const reload = vi.fn();
    const startNew = vi.fn();
    const h = agentsHarness({
      entities: [mirrored("project", ROLL, 1)],
      reply: (c: Command) => (c.type === "project.add" ? { type: "created", id: "prj_on_box" } : { type: "ok" }),
      ctx: {
        startNew,
        services: fakeServices({ sandboxes: { create } }),
        sandboxes: { list: { sandboxes: [], sizes: [SIZE], max: 5, backend: "docker" }, error: null, byMachine: new Map(), usage: new Map(), reload },
      },
    });
    // Strict, as in development: effects run twice.
    renderAgents(
      <StrictMode>
        <ProjectDialogs />
      </StrictMode>,
      h,
    );
    return { h, create, startNew };
  }

  it("rents one, waits for it to say hello, clones the repo onto it, then starts an agent there", async () => {
    const { h, create, startNew } = setup();
    act(() => openProjectDialog({ kind: "sandbox", project: ROLL.id }));
    const dialog = await screen.findByRole("dialog", { name: "New sandbox for roll" });
    expect(within(dialog).getByRole("radio", { name: /standard/ })).toBeChecked();
    expect(within(dialog).getByRole("textbox", { name: "Sandbox name" })).toHaveValue("roll-sandbox");
    await userEvent.click(within(dialog).getByRole("button", { name: "Rent for $0.03/h and clone" }));
    expect(create).toHaveBeenCalledWith("acme", { name: "roll-sandbox", size: "standard" });
    const steps = within(dialog).getByRole("list", { name: "Progress" });
    await waitFor(() => expect(within(steps).getAllByRole("listitem")[1]).toHaveAttribute("aria-current", "step"));
    expect(sent(h).some((c) => c.type === "project.add")).toBe(false);

    // Its daemon says hello: the clone goes to it, by the project's repo.
    act(() => h.client.enqueue({ type: "machine", cursor: 9, machine: { ...BOX, online: true, info: machine().info } }));
    await within(dialog).findByText("roll is ready on roll-sandbox.", { exact: false });
    expect(h.command.mock.calls.find(([, , c]) => c.type === "project.add")?.slice(0, 1)).toEqual(["mch_box"]);
    expect(sent(h).find((c) => c.type === "project.add")).toEqual({ type: "project.add", path: "", clone: { owner: "acme", repo: "roll" } });
    await userEvent.click(within(dialog).getByRole("button", { name: "Start an agent" }));
    expect(startNew).toHaveBeenCalledWith("prj_on_box");
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("says which step failed", async () => {
    const { h } = setup();
    h.command.mockImplementation(async (_m, _id, c) => (c.type === "project.add" ? { type: "error", code: "invalid", message: "no access to acme/roll" } : { type: "ok" }));
    act(() => openProjectDialog({ kind: "sandbox", project: ROLL.id }));
    const dialog = await screen.findByRole("dialog", { name: "New sandbox for roll" });
    await userEvent.click(within(dialog).getByRole("button", { name: /^Rent for/ }));
    act(() => h.client.enqueue({ type: "machine", cursor: 9, machine: { ...BOX, online: true, info: machine().info } }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("no access to acme/roll");
    const items = within(within(dialog).getByRole("list", { name: "Progress" })).getAllByRole("listitem");
    expect(items.map((li) => li.textContent)).toEqual([expect.stringContaining("done"), expect.stringContaining("done"), expect.stringContaining("failed")]);
  });

  it("opens from a project's menu, and Open settings goes to the new project's", async () => {
    const { h } = setup();
    renderAgents(<ProjectNode project={ROLL} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Project" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "New sandbox for this project" }));
    const dialog = await screen.findByRole("dialog", { name: "New sandbox for roll" });
    await userEvent.click(within(dialog).getByRole("button", { name: /^Rent for/ }));
    act(() => h.client.enqueue({ type: "machine", cursor: 9, machine: { ...BOX, online: true, info: machine().info } }));
    await userEvent.click(await within(dialog).findByRole("button", { name: "Open settings" }));
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: "p:prj_on_box" });
  });
});
