import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Command } from "@/lib/agents-api";
import { setActiveClient } from "@/lib/agents-client";
import { agentsPalette, resetPalette } from "@/lib/agents-palette";
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
/** Puts a query in the search field in one input (typing it key by key re-ranks every row per key). */
async function search(box: HTMLElement, text: string) {
  box.focus();
  await userEvent.paste(text);
}

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
    await search(screen.getByRole("combobox"), "upgr");
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

  it("hands a goal to an orchestrator in the last project, and opens it", async () => {
    const h = setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await search(screen.getByRole("combobox"), "Add dark mode to the settings page");
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

  it("runs a command a phrase names: 'open the shelf' opened a thread called that (gaps7), and ⌘Enter is the ask", async () => {
    const h = setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await search(screen.getByRole("combobox"), "open the shelf");
    expect(selected()).toHaveTextContent("Open the shelf");
    // Starting agents is the Ask agents rows, below, and the footer says how.
    const labels = [...palette().querySelectorAll("#palette-list > p")].map((e) => e.textContent);
    expect(labels.at(-1)).toBe("Ask agents");
    expect(palette()).toHaveTextContent("Ask agents");
    await userEvent.keyboard("{Enter}");
    expect(await screen.findByRole("dialog", { name: "Shelf" })).toBeInTheDocument();
    expect(creates(h)).toEqual([]);

    await userEvent.click(screen.getByRole("button", { name: "Command palette" }));
    await search(within(palette()).getByRole("combobox"), "go to settings");
    expect(selected()).toHaveTextContent(/settings$/);
    await userEvent.keyboard("{Meta>}{Enter}{/Meta}");
    await waitFor(() => expect(creates(h)).toHaveLength(1));
    expect(creates(h)[0]).toMatchObject({ prompt: "go to settings", orchestrator: true });
  });

  it("switches the canvas's view, and settles the open conversation", async () => {
    const h = setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    // A submenu's option is a row of its own.
    await search(screen.getByRole("combobox"), "switch view running");
    expect(selected()).toHaveTextContent("Switch view: Running");
    await userEvent.keyboard("{Enter}");
    expect(screen.getByRole("tab", { name: /Running/ })).toHaveAttribute("aria-selected", "true");

    await waitFor(() => expect(agentsPalette.get().open).toBe(false));
    act(() => showAgents({ kind: "thread", thread: "thr_1" }));
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await search(screen.getByRole("combobox"), ">settle");
    expect(selected()).toHaveTextContent("Settle “Fix the flaky queue test”");
    await userEvent.keyboard("{Enter}");
    await waitFor(() => expect(h.command.mock.calls.map((c) => c[2])).toContainEqual({ type: "thread.settle", thread: "thr_1" }));
  });
});

