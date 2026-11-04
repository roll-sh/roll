import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Command } from "@/lib/agents-api";
import { setActiveClient } from "@/lib/agents-client";
import { agentsPalette, resetPalette } from "@/lib/agents-palette";
import { agentsSurface, resetAgentsSurface, showAgents } from "@/lib/agents-surface";
import { approval, mirrored, project, run, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, type AgentsHarness } from "@/test/agents-render";
import { MarkStateProvider } from "../mark-state";
import { AgentsControl } from "./agents-control";

let conn: { client: AgentsHarness["client"] | null; services: ReturnType<typeof fakeServices> | null; error: string | null } = { client: null, services: null, error: null };
vi.mock("./use-connection", () => ({ useAgentsConnection: () => conn, useDemoParam: () => false }));

const busy = thread({ status: "running", active_run: "run_1", run_count: 1, activity: "Running `cargo test`", updated_at: 9 });
const waiting = thread({ id: "thr_2", title: "Upgrade to Next 16.4", status: "waiting", pending_requests: 1, updated_at: 5 });
const entities = [mirrored("project", project(), 1), mirrored("thread", busy, 2), mirrored("thread", waiting, 3), mirrored("run", run(), 4), mirrored("request", approval({ thread: "thr_2" }), 5)];

/** The agents open over a page, with a client whose `thread.create` makes the thread. */
function setup() {
  let h: AgentsHarness | null = null;
  h = agentsHarness({
    entities,
    follow: ["thr_1", "thr_2"],
    reply: (c: Command) => {
      if (c.type !== "thread.create") return { type: "ok" };
      h!.client.enqueue({ type: "entity", ...mirrored("thread", thread({ id: "thr_9", title: c.prompt ?? "", status: "running", orchestrator: !!c.orchestrator }), 9) });
      return { type: "created", id: "thr_9" };
    },
  });
  conn = { client: h.client, services: fakeServices(), error: null };
  renderAgents(
    <MarkStateProvider>
      <AgentsControl />
    </MarkStateProvider>,
    h,
  );
  act(() => showAgents({ kind: "canvas" }));
  return h;
}

const creates = (h: AgentsHarness) => h.command.mock.calls.map((c) => c[2]).filter((c) => c.type === "thread.create");
const palette = () => screen.getByRole("dialog", { name: "Command palette" });
const selected = () => within(palette()).getByRole("option", { selected: true });

beforeEach(() => {
  resetAgentsSurface();
  resetPalette();
  localStorage.clear();
  window.history.replaceState(null, "", "/dashboard");
});
afterEach(() => setActiveClient(null));

describe("the agents' command palette", () => {
  it("opens with ⌘K over the agents (and the header's search), and a conversation found by name opens beside the canvas", async () => {
    setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    expect(palette()).toBeInTheDocument();
    // Before typing: actions, then recent conversations.
    expect(within(palette()).getByText("Actions")).toBeInTheDocument();
    expect(within(palette()).getByRole("option", { name: /Upgrade to Next 16.4/ })).toBeInTheDocument();
    await userEvent.type(screen.getByRole("combobox"), "upgr");
    expect(selected()).toHaveTextContent("Upgrade to Next 16.4");
    await userEvent.keyboard("{Enter}");
    expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_2" });
    await waitFor(() => expect(agentsPalette.get().open).toBe(false));
    // Opened from the header's button too.
    await userEvent.click(screen.getByRole("button", { name: "Command palette" }));
    expect(palette()).toBeInTheDocument();
  });

  it("is keyboard-first: arrows move, Enter opens a submenu, Backspace goes back, Escape closes", async () => {
    setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    const first = selected().textContent;
    await userEvent.keyboard("{ArrowDown}");
    expect(selected().textContent).not.toBe(first);
    await userEvent.keyboard("{ArrowUp}");
    expect(selected().textContent).toBe(first);

    await userEvent.type(screen.getByRole("combobox"), ">new agent in");
    expect(selected()).toHaveTextContent("New agent in…");
    await userEvent.keyboard("{Enter}");
    expect(within(palette()).getByRole("button", { name: "Back" })).toHaveTextContent("New agent in");
    expect(selected()).toHaveTextContent("roll");
    await userEvent.keyboard("{Backspace}");
    expect(screen.getByRole("combobox")).toHaveValue(">new agent in");
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Command palette" })).toBeNull());
    // Escape in the palette doesn't close the agents behind it.
    expect(agentsSurface.get().open).toBe(true);
  });

  it("hands a goal to an orchestrator in the last project, and opens it", async () => {
    const h = setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), "Add dark mode to the settings page");
    // A sentence: the goal rows lead.
    expect(selected()).toHaveTextContent("Orchestrate: Add dark mode to the settings page");
    expect(selected()).toHaveTextContent("Plans it and spins up agents · roll");
    await userEvent.keyboard("{Enter}");
    await waitFor(() => expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_9" }));
    expect(creates(h)).toEqual([
      { type: "thread.create", project: "prj_1", provider: "claude", model: "claude-opus-5-5", runtime_mode: "full_access", interaction_mode: "default", worktree: false, prompt: "Add dark mode to the settings page", orchestrator: true },
    ]);
    expect(localStorage.getItem("roll:agents:last-project")).toBe("prj_1");
  });

  it("starts one agent with the goal instead, one row down", async () => {
    const h = setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), "Bump the lockfile and run the tests");
    await userEvent.keyboard("{ArrowDown}");
    expect(selected()).toHaveTextContent("New agent: Bump the lockfile");
    await userEvent.keyboard("{Enter}");
    await waitFor(() => expect(creates(h)).toHaveLength(1));
    expect(creates(h)[0]).not.toHaveProperty("orchestrator");
    expect(creates(h)[0]).toMatchObject({ prompt: "Bump the lockfile and run the tests", runtime_mode: "full_access" });
  });

  it("opens straight into 'Orchestrate…' with what was typed, and lists where it can go", async () => {
    setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), "orchestr");
    expect(selected()).toHaveTextContent("Orchestrate…");
    await userEvent.keyboard("{Enter}");
    expect(screen.getByRole("combobox")).toHaveAttribute("placeholder", "What should the agents get done?");
    await userEvent.type(screen.getByRole("combobox"), "Ship it");
    expect(within(palette()).getByText("Orchestrate in")).toBeInTheDocument();
    expect(within(palette()).getByText("Or one agent in")).toBeInTheDocument();
  });
});

describe("the canvas's prompt bar", () => {
  it("orchestrates a goal with Enter, or starts one agent when switched", async () => {
    const h = setup();
    await userEvent.type(screen.getByRole("textbox", { name: "A goal for agents" }), "Make the queue survive a restart{Enter}");
    await waitFor(() => expect(creates(h)).toHaveLength(1));
    expect(creates(h)[0]).toMatchObject({ orchestrator: true, prompt: "Make the queue survive a restart", worktree: false });
    await waitFor(() => expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_9" }));

    act(() => showAgents({ kind: "canvas" }));
    await userEvent.click(screen.getByRole("textbox", { name: "A goal for agents" }));
    await userEvent.click(screen.getByRole("radio", { name: "One agent" }));
    await userEvent.type(screen.getByRole("textbox", { name: "A prompt for a new agent" }), "Fix the typo{Enter}");
    await waitFor(() => expect(creates(h)).toHaveLength(2));
    expect(creates(h)[1]).not.toHaveProperty("orchestrator");
  });
});
