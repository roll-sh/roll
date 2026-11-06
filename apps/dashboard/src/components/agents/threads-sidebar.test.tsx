import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";
import { EMPTY_INBOX, type Command, type Thread } from "@/lib/agents-api";
import { agentsSurface, resetAgentsSurface, setAgentsView } from "@/lib/agents-surface";
import { ThreadsSidebar } from "./threads-sidebar";
import { mirrored, project, thread } from "@/test/agents";
import { agentsHarness, renderAgents, type AgentsHarness } from "@/test/agents-render";

const NOW = Date.now();
const H = 3_600_000;
const D = 24 * H;
const box = (over: Partial<Thread["inbox"]>) => ({ ...EMPTY_INBOX, ...over });

function world(extra: Thread[] = []) {
  const threads = [
    thread({ id: "thr_a", title: "Fix the parser", created_at: NOW - H }),
    thread({ id: "thr_b", title: "Add a flag", created_at: NOW - 2 * H }),
    thread({ id: "thr_z", title: "Later", inbox: box({ snooze: { until: NOW + 3 * H, at: NOW } }) }),
    thread({ id: "thr_s", title: "Old work", inbox: box({ settle: { state: "settled", at: NOW - D, by: "inactivity" } }) }),
    ...extra,
  ];
  return agentsHarness({ entities: [mirrored("project", project(), 1), ...threads.map((t, i) => mirrored("thread", t, i + 2))] });
}

function sidebar(h: AgentsHarness, openThread: string | null = null) {
  return renderAgents(<ThreadsSidebar data={h.client.data} open setOpen={() => {}} phone={false} openThread={openThread} />, h);
}

const sent = (h: AgentsHarness) => h.command.mock.calls.map(([, , c]) => c);
/** A thread's row (a list item of its project's threads; projects are list items too). */
const row = (title: string) => screen.getAllByRole("listitem").find((r) => r.hasAttribute("data-row") && r.textContent?.includes(title))!;

afterEach(() => {
  resetAgentsSurface();
  localStorage.clear();
});

describe("the threads sidebar", () => {
  it("lists active threads with the snoozed and settled shelves collapsed, and opens one in the side panel", async () => {
    const h = world();
    sidebar(h);
    const tree = screen.getByRole("navigation", { name: "Projects" });
    expect(within(tree).getByText("Fix the parser")).toBeInTheDocument();
    expect(within(tree).queryByText("Later")).not.toBeInTheDocument();
    expect(within(tree).queryByText("Old work")).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /Snoozed/ }));
    await userEvent.click(screen.getByRole("button", { name: /Settled/ }));
    expect(row("Later")).toHaveTextContent("3h");
    expect(row("Old work")).toHaveTextContent("1d");
    // Remembered.
    expect(localStorage.getItem("roll:agents:sidebar:settled-expanded")).toBe("1");
    await userEvent.click(within(row("Add a flag")).getByRole("button", { name: "Add a flag" }));
    expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_b" });
  });

  it("⌘⇧S settles the open thread and opens the next; Undo puts its inbox back as it was (not kept active by hand); opening it was a visit", async () => {
    const h = world();
    setAgentsView({ kind: "thread", thread: "thr_a" });
    sidebar(h, "thr_a");
    await waitFor(() => expect(sent(h)).toContainEqual({ type: "thread.visit", thread: "thr_a" }));
    fireEvent.keyDown(window, { key: "S", ctrlKey: true, shiftKey: true });
    await waitFor(() => expect(sent(h)).toContainEqual({ type: "thread.settle", thread: "thr_a" }));
    await waitFor(() => expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_b" }));
    await userEvent.click(await screen.findByRole("button", { name: "Undo" }));
    expect(sent(h)).toContainEqual({ type: "thread.unsettle", thread: "thr_a", restore: { settle: null, snooze: null } });
  });

  it("a row's quick action settles it, and a refusal is shown, not swallowed", async () => {
    const h = agentsHarness({
      entities: [mirrored("project", project(), 1), mirrored("thread", thread({ id: "thr_a", title: "Fix the parser" }), 2)],
      reply: (c) => (c.type === "thread.settle" ? { type: "error", code: "conflict", message: "the thread is still running: stop it before settling" } : { type: "ok" }),
    });
    sidebar(h);
    await userEvent.click(within(row("Fix the parser")).getByRole("button", { name: "Settle thread" }));
    expect(await screen.findByRole("status")).toHaveTextContent("still running");
  });

  it("snoozes from the row menu", async () => {
    const h = world();
    sidebar(h);
    await userEvent.click(within(row("Fix the parser")).getByRole("button", { name: "Thread actions" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: /Snooze/ }));
    await userEvent.click(await screen.findByRole("menuitem", { name: /In 1 hour/ }));
    await waitFor(() => expect(sent(h).find((c) => c.type === "thread.snooze")).toBeDefined());
    const snooze = sent(h).find((c): c is Extract<Command, { type: "thread.snooze" }> => c.type === "thread.snooze")!;
    expect(snooze.thread).toBe("thr_a");
    expect(Math.abs(snooze.until - (Date.now() + H))).toBeLessThan(5_000);

  });

  it("keeps each project's archived threads on a shelf of their own: unarchive, or delete after T3's confirmation", async () => {
    const h = world([thread({ id: "thr_x", title: "Spike: edge", archived_at: NOW - D, worktree: { path: "/w/thr_x", branch: "roll/spike", base: "abc", removed_at: null } })]);
    sidebar(h);
    expect(screen.queryByText("Spike: edge")).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /Archived/ }));
    expect(localStorage.getItem("roll:agents:sidebar:archived-expanded")).toBe("1");
    expect(row("Spike: edge")).toHaveTextContent("1d");
    await userEvent.click(within(row("Spike: edge")).getByRole("button", { name: "Unarchive thread" }));
    await waitFor(() => expect(sent(h).at(-1)).toEqual({ type: "thread.unarchive", thread: "thr_x" }));
    expect(await screen.findByRole("status")).toHaveTextContent("Unarchived “Spike: edge”");

    await userEvent.click(within(row("Spike: edge")).getByRole("button", { name: "Thread actions" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Delete…" }));
    expect(sent(h).some((c) => c.type === "thread.delete")).toBe(false);
    await userEvent.click(screen.getByRole("menuitem", { name: "Delete for good?" }));
    await waitFor(() => expect(sent(h).at(-1)).toEqual({ type: "thread.delete", thread: "thr_x" }));
  });
});
