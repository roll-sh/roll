import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";
import { EMPTY_INBOX, type Command, type Thread } from "@/lib/agents-api";
import { agentsSurface, resetAgentsSurface, setAgentsView } from "@/lib/agents-surface";
import { Conversation } from "./session";
import { ThreadsSidebar } from "./threads-sidebar";
import { machine, mirrored, project, run, thread } from "@/test/agents";
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

  it("is lists as T3's sidebar is (no ARIA tree without arrow keys): a project's toggle says if it's open, the open thread is the current one, and Tab reaches a row's actions (M6)", async () => {
    const h = world();
    sidebar(h, "thr_b");
    expect(screen.queryByRole("tree")).toBeNull();
    const project = screen.getByRole("list", { name: /threads$/ });
    expect(within(project).getAllByRole("listitem").length).toBeGreaterThan(1);
    const toggle = screen.getAllByRole("button", { name: /^Hide (?!threads)/ })[0];
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(within(row("Add a flag")).getByRole("button", { name: "Add a flag" })).toHaveAttribute("aria-current", "true");
    expect(within(row("Fix the parser")).getByRole("button", { name: "Fix the parser" })).not.toHaveAttribute("aria-current");
    within(row("Fix the parser")).getByRole("button", { name: "Fix the parser" }).focus();
    await userEvent.tab();
    expect(within(row("Fix the parser")).getByRole("button", { name: "Settle thread" })).toHaveFocus();
    await userEvent.tab();
    expect(within(row("Fix the parser")).getByRole("button", { name: "Thread actions" })).toHaveFocus();
  });

  it("pages the settled shelf: ten, then 25 more", async () => {
    const many = Array.from({ length: 40 }, (_, i) => thread({ id: `thr_o${i}`, title: `Settled ${String(i).padStart(2, "0")}`, inbox: box({ settle: { state: "settled", at: NOW - (i + 1) * H, by: "user" } }) }));
    sidebar(world(many));
    await userEvent.click(screen.getByRole("button", { name: /^Settled/ }));
    const shown = () => screen.getAllByRole("listitem").filter((r) => r.hasAttribute("data-row") && /Settled \d\d|Old work/.test(r.textContent ?? "")).length;
    expect(shown()).toBe(10);
    await userEvent.click(screen.getByRole("button", { name: "Show 25 more" }));
    expect(shown()).toBe(35);
    await userEvent.click(screen.getByRole("button", { name: "Show 6 more" }));
    expect(shown()).toBe(41);
  });

  it("⌘⇧S settles the open thread and opens the next; Undo un-settles it; opening it was a visit", async () => {
    const h = world();
    setAgentsView({ kind: "thread", thread: "thr_a" });
    sidebar(h, "thr_a");
    await waitFor(() => expect(sent(h)).toContainEqual({ type: "thread.visit", thread: "thr_a" }));
    fireEvent.keyDown(window, { key: "S", ctrlKey: true, shiftKey: true });
    await waitFor(() => expect(sent(h)).toContainEqual({ type: "thread.settle", thread: "thr_a" }));
    await waitFor(() => expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_b" }));
    await userEvent.click(await screen.findByRole("button", { name: "Undo" }));
    expect(sent(h)).toContainEqual({ type: "thread.unsettle", thread: "thr_a" });
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

  it("snoozes from the row menu with T3's presets, or a custom time", async () => {
    const h = world();
    sidebar(h);
    await userEvent.click(within(row("Fix the parser")).getByRole("button", { name: "Thread actions" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: /Snooze/ }));
    await userEvent.click(await screen.findByRole("menuitem", { name: /In 1 hour/ }));
    await waitFor(() => expect(sent(h).find((c) => c.type === "thread.snooze")).toBeDefined());
    const snooze = sent(h).find((c): c is Extract<Command, { type: "thread.snooze" }> => c.type === "thread.snooze")!;
    expect(snooze.thread).toBe("thr_a");
    expect(Math.abs(snooze.until - (Date.now() + H))).toBeLessThan(5_000);

    await userEvent.click(within(row("Add a flag")).getByRole("button", { name: "Thread actions" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: /Snooze/ }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Custom…" }));
    const dialog = await screen.findByRole("dialog", { name: "Custom snooze" });
    // On the page, not inside the sidebar (fixed in its transformed aside, it was 268px wide), and
    // holding focus.
    expect(dialog.closest("aside, nav, [aria-label=Projects]")).toBeNull();
    await waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true));
    await userEvent.click(within(dialog).getByRole("button", { name: "Duration" }));
    const amount = within(dialog).getByRole("textbox", { name: "How long" });
    await userEvent.clear(amount);
    await userEvent.type(amount, "0");
    await userEvent.click(within(dialog).getByRole("button", { name: "Snooze" }));
    expect(within(dialog).getByText("Enter a positive duration.")).toBeInTheDocument();
    await userEvent.clear(amount);
    await userEvent.type(amount, "2");
    await userEvent.click(within(dialog).getByRole("button", { name: "Days" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Snooze" }));
    await waitFor(() => expect(sent(h).filter((c) => c.type === "thread.snooze")).toHaveLength(2));
    const custom = sent(h).filter((c): c is Extract<Command, { type: "thread.snooze" }> => c.type === "thread.snooze")[1];
    expect(custom.thread).toBe("thr_b");
    expect(Math.abs(custom.until - (Date.now() + 2 * D))).toBeLessThan(5_000);
  });

  it("on a phone, Snooze… opens the presets dialog (a submenu covered the menu it came from)", async () => {
    const h = world();
    h.ctx.phone = true;
    sidebar(h);
    await userEvent.click(within(row("Fix the parser")).getByRole("button", { name: "Thread actions" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Snooze…" }));
    const dialog = await screen.findByRole("dialog", { name: "Snooze" });
    await userEvent.click(within(dialog).getByRole("button", { name: /In 1 hour/ }));
    await waitFor(() => expect(sent(h).find((c) => c.type === "thread.snooze")).toMatchObject({ thread: "thr_a" }));
  });

  it("wakes a snoozed thread and un-settles a settled one from their rows, each saying so with its undo", async () => {
    const h = world();
    sidebar(h);
    await userEvent.click(screen.getByRole("button", { name: /Snoozed/ }));
    await userEvent.click(screen.getByRole("button", { name: /Settled/ }));
    await userEvent.click(within(row("Later")).getByRole("button", { name: "Wake thread now" }));
    expect(await screen.findByRole("status")).toHaveTextContent("Woken “Later”");
    await userEvent.click(within(screen.getByRole("status")).getByRole("button", { name: /Undo/ }));
    await waitFor(() => expect(sent(h).at(-1)).toEqual({ type: "thread.snooze", thread: "thr_z", until: NOW + 3 * H }));
    await userEvent.click(within(row("Old work")).getByRole("button", { name: "Un-settle thread" }));
    expect(await screen.findByText("Un-settled “Old work”")).toBeInTheDocument();
    await userEvent.click(within(screen.getByRole("status")).getByRole("button", { name: /Undo/ }));
    await waitFor(() => expect(sent(h).at(-1)).toEqual({ type: "thread.settle", thread: "thr_s" }));
    expect(sent(h)).toEqual(expect.arrayContaining([{ type: "thread.unsnooze", thread: "thr_z" }, { type: "thread.unsettle", thread: "thr_s" }]));
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
    expect(screen.getByText(/Delete “Spike: edge”\? Its conversation is cleared for good\. Its branch stays, and so does its worktree if it has uncommitted changes\./)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("menuitem", { name: "Delete for good?" }));
    await waitFor(() => expect(sent(h).at(-1)).toEqual({ type: "thread.delete", thread: "thr_x" }));
  });

  it("archives from a row's menu, with T3's undo notice", async () => {
    const h = world();
    sidebar(h);
    await userEvent.click(within(row("Add a flag")).getByRole("button", { name: "Thread actions" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Archive" }));
    await waitFor(() => expect(sent(h).at(-1)).toEqual({ type: "thread.archive", thread: "thr_b" }));
    await userEvent.click(within(await screen.findByRole("status")).getByRole("button", { name: "Undo" }));
    expect(sent(h).at(-1)).toEqual({ type: "thread.unarchive", thread: "thr_b" });
  });
});

describe("the conversation's menu", () => {
  it("settles, and snoozes through the dialog's presets", async () => {
    const t = thread({ id: "thr_a", title: "Fix the parser", run_count: 1 });
    const h = agentsHarness({ entities: [mirrored("project", project(), 1), mirrored("thread", t, 2), mirrored("run", run({ thread: "thr_a", status: "completed" }), 3)] });
    renderAgents(<Conversation thread={t} machine={machine()} />, h);
    await userEvent.click(screen.getAllByRole("button", { name: "More" })[0]);
    await userEvent.click(await screen.findByRole("menuitem", { name: "Snooze…" }));
    const dialog = await screen.findByRole("dialog", { name: "Snooze" });
    await userEvent.click(within(dialog).getByRole("button", { name: /Tomorrow/ }));
    await waitFor(() => expect(sent(h).some((c) => c.type === "thread.snooze")).toBe(true));
    await userEvent.click(screen.getAllByRole("button", { name: "More" })[0]);
    await act(async () => {
      await userEvent.click(await screen.findByRole("menuitem", { name: "Settle" }));
    });
    await waitFor(() => expect(sent(h)).toContainEqual({ type: "thread.settle", thread: "thr_a" }));
  });
});
