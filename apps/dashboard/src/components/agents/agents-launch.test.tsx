import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { resetProjectDialogs } from "@/lib/agents-project-dialogs";
import { setActiveClient } from "@/lib/agents-client";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { machine, mirrored, project, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, type AgentsHarness } from "@/test/agents-render";
import { MarkStateProvider } from "../mark-state";
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
afterEach(() => {
  setActiveClient(null);
  resetProjectDialogs();
});

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
    expect(commands(h)).toContainEqual(expect.objectContaining({ type: "thread.create", project: "prj_1", prompt: "Add a --json flag" }));
    expect(screen.queryByRole("dialog", { name: "New agent" })).toBeNull();
  });

  it("a thread's fork handle dragged onto the canvas forks it and opens the fork", async () => {
    const h = surface();
    await userEvent.keyboard("{Meta>}j{/Meta}");
    const handle = within(screen.getByLabelText("Thread Fix the flaky queue test")).getByRole("button", { name: /^Fork/ });
    fireEvent.pointerDown(handle, { pointerId: 1, button: 0, clientX: 10, clientY: 10 });
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 300, clientY: 40 });
    expect(screen.getByRole("status")).toHaveTextContent("Fork here");
    fireEvent.pointerUp(handle, { pointerId: 1, clientX: 300, clientY: 40 });
    await waitFor(() => expect(commands(h)).toContainEqual({ type: "thread.fork", thread: "thr_1" }));
    await waitFor(() => expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_new" }));
  });
});

describe("several models at once (T3's multi-model fan-out)", () => {
  const git = project({ git: { branch: "main", head: "f35f34cf", remote: null, dirty: false } });
  const numbered = () => {
    let n = 0;
    return (c: { type: string; model?: string }) => {
      if (c.type !== "thread.create") return { type: "ok" as const };
      if (c.model === "claude-sonnet-5" && failSonnet) return { type: "error" as const, code: "provider" as const, message: "Sonnet 5 is busy" };
      return { type: "created" as const, id: `thr_${++n}` };
    };
  };
  let failSonnet = false;
  const shiftClick = async (name: RegExp) => {
    const row = await screen.findByRole("option", { name });
    // One session, so Shift stays held for the click.
    const user = userEvent.setup();
    await user.keyboard("{Shift>}");
    await user.click(row);
    await user.keyboard("{/Shift}");
  };

  it("Shift-click adds a model; Enter starts one thread per model from one prompt, each in its own worktree, and opens the first", async () => {
    failSonnet = false;
    const h = agentsHarness({ entities: [mirrored("project", git, 1)], reply: numbered() });
    renderAgents(<Launcher onClose={vi.fn()} />, h);
    await userEvent.click(screen.getByRole("button", { name: /^Model: Opus 5\.5/ }));
    expect(await screen.findByText(/to add a model/)).toBeInTheDocument();
    await shiftClick(/^Sonnet 5/);
    // The picker stays open, both rows checked; the trigger names them.
    expect(screen.getByRole("button", { name: "Models: Opus 5.5, Sonnet 5" })).toBeInTheDocument();
    expect(screen.getByText(/2 models: one thread each, in its own worktree/)).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");
    const worktree = screen.getByRole("switch", { name: "Worktree" });
    expect(worktree).toBeChecked();
    expect(worktree).toHaveAttribute("aria-disabled", "true");
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "Speed up the queue tests");
    await userEvent.click(screen.getByRole("button", { name: "Start 2 threads" }));
    await waitFor(() => expect(h.openThread).toHaveBeenCalledWith("thr_1"));
    expect(commands(h).filter((c) => c.type === "thread.create")).toEqual([
      { type: "thread.create", project: "prj_1", provider: "claude", model: "claude-opus-5-5", runtime_mode: "full_access", interaction_mode: "default", worktree: true, prompt: "Speed up the queue tests" },
      { type: "thread.create", project: "prj_1", provider: "claude", model: "claude-sonnet-5", runtime_mode: "full_access", interaction_mode: "default", worktree: true, prompt: "Speed up the queue tests" },
    ]);
    expect(h.openThread).toHaveBeenCalledTimes(1);
  });

  it("models that couldn't start stay picked with the prompt, and it says why", async () => {
    failSonnet = true;
    const h = agentsHarness({ entities: [mirrored("project", git, 1)], reply: numbered() });
    renderAgents(<Launcher onClose={vi.fn()} />, h);
    await userEvent.click(screen.getByRole("button", { name: /^Model: Opus 5\.5/ }));
    await shiftClick(/^Sonnet 5/);
    await userEvent.keyboard("{Escape}");
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "Try both");
    await userEvent.click(screen.getByRole("button", { name: "Start 2 threads" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Started 1 of 2 threads. Couldn't start Sonnet 5: Sonnet 5 is busy");
    expect(screen.getByRole("textbox", { name: "Message" })).toHaveValue("Try both");
    expect(screen.getByRole("button", { name: /^Model: Sonnet 5/ })).toBeInTheDocument();
    expect(h.openThread).not.toHaveBeenCalled();
    failSonnet = false;
  });

});
