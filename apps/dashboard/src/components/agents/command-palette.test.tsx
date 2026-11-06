import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Command } from "@/lib/agents-api";
import { setActiveClient } from "@/lib/agents-client";
import { agentsPalette, openPalette, resetPalette } from "@/lib/agents-palette";
import { agentsSurface, resetAgentsSurface, showAgents } from "@/lib/agents-surface";
import { resetShelves } from "@/lib/agents-shelf";
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
function setup(extra: typeof entities = []) {
  let h: AgentsHarness | null = null;
  h = agentsHarness({
    entities: [...entities, ...extra],
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
afterEach(() => {
  setActiveClient(null);
  resetShelves();
});

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
    // A conversation opens (the footer doesn't say Run).
    expect(palette()).toHaveTextContent("EnterOpen");
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

  it("never takes one word as a goal on Enter: 'unarchive' with nothing archived launched an orchestrator", async () => {
    const h = setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), "unarchive");
    expect(within(palette()).queryByRole("option", { selected: true })).toBeNull();
    await userEvent.keyboard("{Enter}");
    expect(creates(h)).toEqual([]);
    // Still there one ↓ away.
    await userEvent.keyboard("{ArrowDown}");
    expect(selected()).toHaveTextContent("Orchestrate: unarchive");
  });

  it("runs a command a phrase names: 'open the shelf' opened a thread called that (gaps7), and ⌘Enter is the ask", async () => {
    const h = setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), "open the shelf");
    expect(selected()).toHaveTextContent("Open the shelf");
    // Starting agents is the Ask agents rows, below, and the footer says how.
    const labels = [...palette().querySelectorAll("#palette-list > p")].map((e) => e.textContent);
    expect(labels.at(-1)).toBe("Ask agents");
    expect(palette()).toHaveTextContent("Ask agents");
    await userEvent.keyboard("{Enter}");
    expect(await screen.findByRole("dialog", { name: "Shelf" })).toBeInTheDocument();
    expect(creates(h)).toEqual([]);

    await userEvent.click(screen.getByRole("button", { name: "Command palette" }));
    await userEvent.type(within(palette()).getByRole("combobox"), "go to settings");
    expect(selected()).toHaveTextContent(/settings$/);
    await userEvent.keyboard("{Meta>}{Enter}{/Meta}");
    await waitFor(() => expect(creates(h)).toHaveLength(1));
    expect(creates(h)[0]).toMatchObject({ prompt: "go to settings", orchestrator: true });
  });

  it("finds Unarchive for an archived conversation by 'archive' or 'unarchive' (it matched only inside the word: goal rows led)", async () => {
    const h = setup([mirrored("thread", thread({ id: "thr_a", title: "Spike: edge middleware", archived_at: Date.now() - 86_400_000 }), 6)]);
    act(() => showAgents({ kind: "thread", thread: "thr_a" }));
    const options = () => within(palette()).getAllByRole("option").map((o) => o.textContent ?? "");
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), "archive");
    expect(options()).toContain("Unarchive “Spike: edge middleware”");
    expect(options()[0]).not.toMatch(/^Orchestrate/);
    await userEvent.clear(screen.getByRole("combobox"));
    await userEvent.type(screen.getByRole("combobox"), "unarchive");
    expect(selected()).toHaveTextContent("Unarchive “Spike: edge middleware”");
    await userEvent.keyboard("{Enter}");
    await waitFor(() => expect(h.command.mock.calls.map((c) => c[2])).toContainEqual({ type: "thread.unarchive", thread: "thr_a" }));
    expect(creates(h)).toEqual([]);
  });

  it("finds archived and settled conversations, and opens a project's shelf from the canvas toolbar", async () => {
    const DAY = 86_400_000;
    setup([
      mirrored("thread", thread({ id: "thr_a", title: "Spike: edge middleware", archived_at: Date.now() - DAY }), 6),
      mirrored("thread", thread({ id: "thr_s", title: "Bump tokio", inbox: { ...busy.inbox, settle: { state: "settled", at: Date.now() - DAY, by: "user" } } }), 7),
    ]);
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), "edge midd");
    expect(selected()).toHaveTextContent("Spike: edge middleware");
    expect(selected()).toHaveTextContent("Archived");
    await userEvent.keyboard("{Enter}");
    expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_a" });
    await waitFor(() => expect(agentsPalette.get().open).toBe(false));

    await userEvent.click(screen.getByRole("button", { name: "Command palette" }));
    await userEvent.type(within(palette()).getByRole("combobox"), "settled and archived");
    expect(selected()).toHaveTextContent("Settled and archived in…");
    await userEvent.keyboard("{Enter}");
    expect(within(palette()).getByRole("option", { name: /Settled 1 · Archived 1/ })).toBeInTheDocument();
    await userEvent.keyboard("{Enter}");
    // The canvas, with the toolbar's shelf open on just that project.
    const shelf = await screen.findByRole("dialog", { name: "Shelf" });
    expect(within(shelf).getByRole("combobox")).toHaveAttribute("placeholder", `Search ${project().name}’s shelf…`);
    expect(within(shelf).getByRole("option", { name: "Bump tokio" })).toBeInTheDocument();
    expect(agentsSurface.get().view).toEqual({ kind: "canvas" });
  });

  it("asks a conflicting change's agent to rebase, and opens it", async () => {
    const change = { repo: "ada/roll", number: 42, status: "conflict" as const, url: "", detail: "src/queue.rs" };
    const h = setup([mirrored("thread", thread({ id: "thr_c", title: "Queue retries", change }), 6)]);
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), "rebase");
    expect(selected()).toHaveTextContent("Ask to rebase #42 “Queue retries”");
    expect(selected()).toHaveTextContent("Conflicts in src/queue.rs");
    await userEvent.keyboard("{Enter}");
    await waitFor(() => expect(h.command.mock.calls.map((c) => c[2])).toContainEqual({ type: "message.send", thread: "thr_c", text: expect.stringContaining("Change #42 conflicts with the default branch (in src/queue.rs)") }));
    await waitFor(() => expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_c" }));
  });

  it("switches the canvas's view, and settles the open conversation", async () => {
    const h = setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    // A submenu's option is a row of its own.
    await userEvent.type(screen.getByRole("combobox"), "switch view running");
    expect(selected()).toHaveTextContent("Switch view: Running");
    await userEvent.keyboard("{Enter}");
    expect(screen.getByRole("tab", { name: /Running/ })).toHaveAttribute("aria-selected", "true");

    await waitFor(() => expect(agentsPalette.get().open).toBe(false));
    act(() => showAgents({ kind: "thread", thread: "thr_1" }));
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), ">settle");
    expect(selected()).toHaveTextContent("Settle “Fix the flaky queue test”");
    await userEvent.keyboard("{Enter}");
    await waitFor(() => expect(h.command.mock.calls.map((c) => c[2])).toContainEqual({ type: "thread.settle", thread: "thr_1" }));
  });

  it("snoozes the open conversation from the keyboard: T3's presets (no keyboard path before)", async () => {
    const h = setup();
    act(() => showAgents({ kind: "thread", thread: "thr_1" }));
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), ">snooze");
    expect(selected()).toHaveTextContent("Snooze “Fix the flaky queue test”…");
    await userEvent.keyboard("{Enter}");
    expect(screen.getByRole("combobox")).toHaveAttribute("placeholder", "Until when?");
    await userEvent.type(screen.getByRole("combobox"), "in 1 hour");
    await userEvent.keyboard("{Enter}");
    await waitFor(() => expect(h.command.mock.calls.map((c) => c[2])).toContainEqual(expect.objectContaining({ type: "thread.snooze", thread: "thr_1" })));
    const sent = h.command.mock.calls.map((c) => c[2] as { type: string; until?: number }).find((c) => c.type === "thread.snooze")!;
    expect(sent.until! - Date.now()).toBeGreaterThan(55 * 60_000);
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

  it("“Project: roll” has the project's actions, its card's own", async () => {
    setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), "project: roll");
    expect(selected()).toHaveTextContent("Project: roll");
    await userEvent.keyboard("{Enter}");
    const names = within(palette())
      .getAllByRole("option")
      .map((o) => o.textContent);
    expect(names).toEqual([
      expect.stringContaining("Show on the canvas"),
      expect.stringContaining("New agent here"),
      expect.stringContaining("Focus on this project only"),
      expect.stringContaining("Settings"),
    ]);
    await userEvent.type(within(palette()).getByRole("combobox"), "settings");
    await userEvent.keyboard("{Enter}");
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: `p:${project().id}` });
  });

  it("opened by F, it's on Focus a project… (its row's shortcut), listing the projects", async () => {
    setup([mirrored("project", project({ id: "prj_2", name: "web" }), 20)]);
    act(() => openPalette({ kind: "focus-project" }));
    const names = await waitFor(() => {
      const n = within(palette()).getAllByRole("option").map((o) => o.textContent ?? "");
      expect(n.some((x) => x.includes("web"))).toBe(true);
      return n;
    });
    expect(names.some((x) => x.includes("roll"))).toBe(true);
    expect(names.some((x) => x.includes("Upgrade to Next"))).toBe(false);
  });
});
