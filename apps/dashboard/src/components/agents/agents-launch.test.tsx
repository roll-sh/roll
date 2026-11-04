import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Graph, GraphNode } from "@/lib/agents-graph";
import { setActiveClient } from "@/lib/agents-client";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { machine, mirrored, project, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, type AgentsHarness } from "@/test/agents-render";
import { MarkStateProvider } from "../mark-state";
import { AgentsCanvas } from "./agents-canvas";
import { AgentsControl } from "./agents-control";
import { Launcher } from "./new-agent";

let conn: { client: AgentsHarness["client"] | null; services: ReturnType<typeof fakeServices> | null; error: string | null } = { client: null, services: null, error: null };
vi.mock("./use-connection", () => ({ useAgentsConnection: () => conn, useDemoParam: () => false }));

const commands = (h: AgentsHarness) => h.command.mock.calls.map((c) => c[2]);
const box = machine({ id: "mch_2", name: "build-box", info: { ...machine().info!, hostname: "build-box", os: "linux" } });
const created = (c: { type: string }) => (c.type === "thread.create" || c.type === "thread.fork" ? { type: "created" as const, id: "thr_new" } : { type: "ok" as const });

/** The whole surface, as the header opens it. */
function surface() {
  const h = agentsHarness({
    machines: [machine(), box],
    entities: [mirrored("project", project(), 1), mirrored("project", project({ id: "prj_2", name: "infra", path: "/home/roll/infra" }), 2, "mch_2"), mirrored("thread", thread(), 3)],
    reply: created,
  });
  conn = { client: h.client, services: fakeServices(), error: null };
  renderAgents(
    <MarkStateProvider>
      <AgentsControl />
    </MarkStateProvider>,
    h,
  );
  return h;
}

beforeEach(() => {
  resetAgentsSurface();
  window.history.replaceState(null, "", "/dashboard");
});
afterEach(() => setActiveClient(null));

describe("the launcher", () => {
  it("Enter starts the thread with the project's defaults, and it opens beside the canvas", async () => {
    const h = agentsHarness({
      entities: [mirrored("project", project({ settings: { ...project().settings, defaults: { provider: "claude", model: "claude-sonnet-5", runtime_mode: "ask", interaction_mode: "plan", worktree: true }, scripts: [] } }), 1)],
      reply: created,
    });
    const onClose = vi.fn();
    renderAgents(<Launcher onClose={onClose} />, h);
    const dialog = screen.getByRole("dialog", { name: "New agent" });
    expect(within(dialog).getByRole("button", { name: /^Model: Sonnet 5/ })).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Permissions: Ask" })).toBeInTheDocument();
    expect(within(dialog).getByRole("textbox", { name: "Message" })).toHaveFocus();
    await userEvent.keyboard("Speed up the queue tests{Enter}");
    await waitFor(() =>
      expect(commands(h)).toEqual([
        {
          type: "thread.create",
          project: "prj_1",
          provider: "claude",
          model: "claude-sonnet-5",
          runtime_mode: "ask",
          interaction_mode: "plan",
          worktree: true,
          prompt: "Speed up the queue tests",
        },
      ]),
    );
    await waitFor(() => expect(h.openThread).toHaveBeenCalledWith("thr_new"));
  });

  it("without project defaults: the machine's logged-in provider, full access, no worktree outside roll code", async () => {
    const h = agentsHarness({ entities: [mirrored("project", project(), 1)], reply: created });
    renderAgents(<Launcher onClose={vi.fn()} />, h);
    await userEvent.keyboard("hi{Enter}");
    await waitFor(() =>
      expect(commands(h)[0]).toEqual({ type: "thread.create", project: "prj_1", provider: "claude", model: "claude-opus-5-5", runtime_mode: "full_access", interaction_mode: "default", worktree: false, prompt: "hi" }),
    );
  });

  it("Escape closes it; the project picker switches project and offers adding one", async () => {
    const h = agentsHarness({ entities: [mirrored("project", project(), 1), mirrored("project", project({ id: "prj_3", name: "web", path: "/Users/ada/web" }), 2)] });
    const onClose = vi.fn();
    renderAgents(<Launcher onClose={onClose} />, h);
    await userEvent.click(screen.getByRole("combobox", { name: "Project" }));
    await userEvent.click(await screen.findByRole("option", { name: "web" }));
    expect(screen.getByRole("combobox", { name: "Project" })).toHaveTextContent("web");
    await userEvent.click(screen.getByRole("combobox", { name: "Project" }));
    await userEvent.click(await screen.findByRole("option", { name: "Add a project…" }));
    expect(screen.queryByRole("textbox", { name: "Message" })).toBeNull();
    expect(onClose).not.toHaveBeenCalled();
    screen.getByRole("dialog", { name: "New agent" }).focus();
    await userEvent.keyboard("{Escape}");
    expect(onClose).toHaveBeenCalled();
  });
});

describe("launching from the surface", () => {
  it("\"New agent\" or ⌘⇧O opens the launcher over the canvas; Enter opens the new thread and closes it", async () => {
    const h = surface();
    await userEvent.keyboard("{Meta>}{Shift>}o{/Shift}{/Meta}");
    expect(agentsSurface.get()).toEqual({ open: true, view: { kind: "new" } });
    const popup = screen.getByRole("dialog", { name: "Agents" });
    // The canvas stays under it.
    expect(within(popup).getByLabelText("Agents canvas")).toBeInTheDocument();
    await userEvent.type(within(popup).getByRole("textbox", { name: "Message" }), "Add a --json flag{Enter}");
    await waitFor(() => expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_new" }));
    expect(commands(h).at(-1)).toMatchObject({ type: "thread.create", project: "prj_1", prompt: "Add a --json flag" });
    expect(screen.queryByRole("dialog", { name: "New agent" })).toBeNull();
  });

  it("Escape in the launcher closes it back to the canvas (a picker's Escape only closes the picker)", async () => {
    surface();
    await userEvent.click(screen.getByRole("button", { name: "New agent" }));
    const launcher = screen.getByRole("dialog", { name: "New agent" });
    await userEvent.click(within(launcher).getByRole("button", { name: /^Model:/ }));
    await screen.findByRole("dialog", { name: "Choose a model" });
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Choose a model" })).toBeNull());
    expect(agentsSurface.get().view).toEqual({ kind: "new" });
    within(launcher).getByRole("textbox", { name: "Message" }).focus();
    await userEvent.keyboard("{Escape}");
    expect(agentsSurface.get()).toEqual({ open: true, view: { kind: "canvas" } });
  });

  it("the + on a machine card opens it on that machine, the + on a project card in that project", async () => {
    surface();
    await userEvent.keyboard("{Meta>}j{/Meta}");
    const machineCard = screen.getByLabelText("Machine build-box");
    await userEvent.click(within(machineCard).getByRole("button", { name: /^New agent on this machine/ }));
    let launcher = screen.getByRole("dialog", { name: "New agent" });
    expect(within(launcher).getByRole("combobox", { name: "Machine" })).toHaveTextContent("build-box");
    expect(within(launcher).getByRole("combobox", { name: "Project" })).toHaveTextContent("infra");
    await userEvent.click(within(launcher).getByRole("button", { name: "Close (Esc)" }));
    expect(agentsSurface.get().view).toEqual({ kind: "canvas" });
    const projectCard = screen.getByLabelText("Project roll");
    await userEvent.click(within(projectCard).getByRole("button", { name: /^New agent here/ }));
    launcher = screen.getByRole("dialog", { name: "New agent" });
    expect(within(launcher).getByRole("combobox", { name: "Project" })).toHaveTextContent("roll");
    expect(agentsSurface.get().view).toEqual({ kind: "new", project: "prj_1" });
  });

  it("a project's handle dragged onto the canvas opens the launcher where it's dropped", async () => {
    surface();
    await userEvent.keyboard("{Meta>}j{/Meta}");
    const handle = within(screen.getByLabelText("Project infra")).getByRole("button", { name: /^New agent here/ });
    fireEvent.pointerDown(handle, { pointerId: 1, button: 0, clientX: 10, clientY: 10 });
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 140, clientY: 160 });
    expect(screen.getByRole("status")).toHaveTextContent("New agent here");
    fireEvent.pointerUp(handle, { pointerId: 1, clientX: 140, clientY: 160 });
    expect(screen.queryByRole("status")).toBeNull();
    const view = agentsSurface.get().view;
    expect(view).toMatchObject({ kind: "new", project: "prj_2", at: { x: 140, y: 160 } });
    expect(within(screen.getByRole("dialog", { name: "New agent" })).getByRole("combobox", { name: "Project" })).toHaveTextContent("infra");
  });

  it("a thread's fork handle dragged onto the canvas forks it and opens the fork", async () => {
    const h = surface();
    await userEvent.keyboard("{Meta>}j{/Meta}");
    const handle = within(screen.getByLabelText("Thread Fix the flaky queue test")).getByRole("button", { name: /^Fork/ });
    fireEvent.pointerDown(handle, { pointerId: 1, button: 0, clientX: 10, clientY: 10 });
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 300, clientY: 40 });
    expect(screen.getByRole("status")).toHaveTextContent("Fork here");
    fireEvent.pointerUp(handle, { pointerId: 1, clientX: 300, clientY: 40 });
    await waitFor(() => expect(commands(h).at(-1)).toEqual({ type: "thread.fork", thread: "thr_1" }));
    await waitFor(() => expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_new" }));
  });
});

describe("drag to launch on the canvas", () => {
  const node = (key: string, parent: string | null, children: string[] = []): GraphNode => ({ key, kind: parent ? "project" : "machine", id: key, machine: "mch_1", parent, children, busy: false, relation: null });
  const graph: Graph = { roots: ["m"], nodes: new Map([["m", node("m", null, ["p"])], ["p", node("p", "m")]]), edges: [] };

  function canvas(onLaunch: (source: string, at: { x: number; y: number }) => void, onClick = vi.fn()) {
    render(
      <AgentsCanvas
        graph={graph}
        storageKey="o"
        render={(n) => (
          <div data-drag>
            {n.key}
            <button data-launch={`p:${n.key}`} data-nodrag onClick={onClick} aria-label={`launch ${n.key}`}>
              +
            </button>
          </div>
        )}
        defaultSize={() => ({ w: 100, h: 40 })}
        onLaunch={onLaunch}
      />,
    );
    return screen.getByRole("button", { name: "launch p" });
  }

  it("a click on the handle stays a click; a short wobble isn't a drag", () => {
    const onLaunch = vi.fn();
    const onClick = vi.fn();
    const handle = canvas(onLaunch, onClick);
    fireEvent.pointerDown(handle, { pointerId: 1, button: 0, clientX: 10, clientY: 10 });
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 13, clientY: 12 });
    fireEvent.pointerUp(handle, { pointerId: 1, clientX: 13, clientY: 12 });
    fireEvent.click(handle);
    expect(onClick).toHaveBeenCalledTimes(1);
    expect(onLaunch).not.toHaveBeenCalled();
  });

  it("dropped on empty canvas it launches there and swallows the click; on a card or after Escape nothing starts", () => {
    const onLaunch = vi.fn();
    const onClick = vi.fn();
    const handle = canvas(onLaunch, onClick);
    fireEvent.pointerDown(handle, { pointerId: 1, button: 0, clientX: 10, clientY: 10 });
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 400, clientY: 300 });
    fireEvent.pointerUp(handle, { pointerId: 1, clientX: 400, clientY: 300 });
    fireEvent.click(handle);
    expect(onLaunch).toHaveBeenCalledWith("p:p", { x: 400, y: 300 });
    expect(onClick).not.toHaveBeenCalled();

    // Over a card.
    const card = document.querySelector<HTMLElement>('[data-card][data-key="m"]')!;
    const elementFromPoint = vi.fn(() => card);
    Object.defineProperty(document, "elementFromPoint", { value: elementFromPoint, configurable: true });
    fireEvent.pointerDown(handle, { pointerId: 2, button: 0, clientX: 10, clientY: 10 });
    fireEvent.pointerMove(handle, { pointerId: 2, clientX: 50, clientY: 60 });
    expect(screen.getByRole("status")).toHaveTextContent("Drop on empty canvas");
    fireEvent.pointerUp(handle, { pointerId: 2, clientX: 50, clientY: 60 });
    delete (document as { elementFromPoint?: unknown }).elementFromPoint;

    // Escape mid-drag.
    fireEvent.pointerDown(handle, { pointerId: 3, button: 0, clientX: 10, clientY: 10 });
    fireEvent.pointerMove(handle, { pointerId: 3, clientX: 400, clientY: 300 });
    act(() => void window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" })));
    expect(screen.queryByRole("status")).toBeNull();
    fireEvent.pointerUp(handle, { pointerId: 3, clientX: 400, clientY: 300 });
    expect(onLaunch).toHaveBeenCalledTimes(1);
  });
});
