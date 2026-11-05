import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";
import { EMPTY_INBOX, type Thread } from "@/lib/agents-api";
import { openShelf, resetShelves, shelfList } from "@/lib/agents-shelf";
import { ProjectNode } from "./nodes";
import { ShelfControl } from "./project-shelf";
import { InboxToast, ShelfStrip, showInboxToast } from "./threads-sidebar";
import { machine, mirrored, project, thread } from "@/test/agents";
import { agentsHarness, renderAgents, type AgentsHarness } from "@/test/agents-render";

const NOW = Date.now();
const H = 3_600_000;
const D = 24 * H;
const box = (over: Partial<Thread["inbox"]>) => ({ ...EMPTY_INBOX, ...over });
const settledAt = (at: number, by: "user" | "inactivity" = "user") => box({ settle: { state: "settled", at, by } });
const WEB = project({ id: "prj_web", name: "web", path: "/Users/ada/web" });

function world(extra: Thread[] = []) {
  const threads = [
    thread({ id: "thr_live", title: "Still going", created_at: NOW - H }),
    thread({ id: "thr_s1", title: "Bump tokio", inbox: settledAt(NOW - 2 * D, "inactivity") }),
    thread({ id: "thr_s2", title: "Document the queue", inbox: settledAt(NOW - D) }),
    thread({ id: "thr_z", title: "Review retries", inbox: box({ snooze: { until: NOW + 3 * H, at: NOW } }) }),
    thread({ id: "thr_a", title: "Spike: edge middleware", archived_at: NOW - 5 * D }),
    thread({ id: "thr_w", project: WEB.id, title: "Upgrade Next", inbox: settledAt(NOW - 3 * D) }),
    ...extra,
  ];
  return agentsHarness({
    entities: [mirrored("project", project({ name: "roll" }), 1), mirrored("project", WEB, 2), ...threads.map((t, i) => mirrored("thread", t, i + 3))],
  });
}

function toolbar(h: AgentsHarness, focused: string | null = null) {
  return renderAgents(
    <>
      <ShelfControl data={h.client.data} focused={focused} phone={false} />
      <InboxToast />
    </>,
    h,
  );
}

const sent = (h: AgentsHarness) => h.command.mock.calls.map(([, , c]) => c);
const options = () => screen.getAllByRole("option").map((o) => o.getAttribute("aria-label"));

afterEach(() => {
  resetShelves();
  showInboxToast(null);
  localStorage.clear();
});

describe("the shelf (the canvas toolbar's Shelf)", () => {
  it("lists what left the canvas, every project's, grouped by project; tabs per shelf", async () => {
    const h = world();
    toolbar(h);
    const trigger = screen.getByRole("button", { name: "Shelf: 5 settled, snoozed or archived" });
    await userEvent.click(trigger);
    const pop = await screen.findByRole("dialog", { name: "Shelf" });
    // Settled first, by project, most recently settled first.
    expect(within(within(pop).getByRole("listbox")).getAllByRole("group").map((g) => g.getAttribute("aria-label"))).toEqual(["roll", "web"]);
    expect(options()).toEqual(["Document the queue", "Bump tokio", "Upgrade Next"]);
    expect(within(pop).getByRole("button", { name: "Settled 3", pressed: true })).toBeInTheDocument();
    await userEvent.click(within(pop).getByRole("button", { name: "Archived 1", pressed: false }));
    expect(options()).toEqual(["Spike: edge middleware"]);
  });

  it("searches, walks with the arrows, restores with ⌘Enter (with the undo notice) and opens with Enter", async () => {
    const h = world();
    toolbar(h);
    await userEvent.click(screen.getByRole("button", { name: /^Shelf/ }));
    const search = await screen.findByRole("combobox", { name: "Search the shelf" });
    expect(search).toHaveFocus();
    await userEvent.type(search, "queue");
    expect(options()).toEqual(["Document the queue"]);
    await userEvent.clear(search);
    await userEvent.keyboard("{ArrowDown}");
    expect(screen.getByRole("option", { name: "Bump tokio" })).toHaveAttribute("aria-selected", "true");
    await userEvent.keyboard("{Meta>}{Enter}{/Meta}");
    expect(sent(h).at(-1)).toEqual({ type: "thread.unsettle", thread: "thr_s1" });
    const notice = await screen.findByRole("status");
    expect(notice).toHaveTextContent("Un-settled “Bump tokio”");
    // Enter opens the conversation in the side panel, and the shelf closes.
    await userEvent.keyboard("{Enter}");
    expect(h.openThread).toHaveBeenCalledWith("thr_s1");
    expect(screen.queryByRole("dialog", { name: "Shelf" })).not.toBeInTheDocument();
  });

  it("restores from a row's button: unarchive, and a snooze woken early comes back with its own time", async () => {
    const h = world();
    toolbar(h);
    await userEvent.click(screen.getByRole("button", { name: /^Shelf/ }));
    await userEvent.click(await screen.findByRole("button", { name: "Archived 1" }));
    await userEvent.click(screen.getByRole("button", { name: "Unarchive Spike: edge middleware" }));
    expect(sent(h).at(-1)).toEqual({ type: "thread.unarchive", thread: "thr_a" });
    await screen.findByText("Unarchived “Spike: edge middleware”");
    await userEvent.keyboard("{Meta>}z{/Meta}");
    expect(sent(h).at(-1)).toEqual({ type: "thread.archive", thread: "thr_a" });
    await userEvent.click(screen.getByRole("button", { name: "Snoozed 1" }));
    await userEvent.click(screen.getByRole("button", { name: "Wake now Review retries" }));
    expect(sent(h).at(-1)).toEqual({ type: "thread.unsnooze", thread: "thr_z" });
    await userEvent.click(within(await screen.findByRole("status")).getByRole("button", { name: "Undo" }));
    expect(sent(h).at(-1)).toEqual({ type: "thread.snooze", thread: "thr_z", until: NOW + 3 * H });
  });

  it("opens narrowed to a project from its ⋯ menu; the chip's × widens it; a focused project narrows it", async () => {
    const h = world();
    renderAgents(
      <>
        <ProjectNode project={project({ name: "roll" })} machine={machine()} />
        <ShelfControl data={h.client.data} focused={null} phone={false} />
      </>,
      h,
    );
    // The card itself carries no shelf.
    expect(screen.queryByText("Bump tokio")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Settled \d/ })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Project" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Settled and archived (4)" }));
    await screen.findByRole("dialog", { name: "Shelf" });
    expect(options()).toEqual(["Document the queue", "Bump tokio"]);
    await userEvent.click(screen.getByRole("button", { name: "Show every project's shelf, not only roll" }));
    expect(options()).toEqual(["Document the queue", "Bump tokio", "Upgrade Next"]);
  });

  it("opens from a link: ?shelf=<project> narrowed, an unknown project on every one", async () => {
    window.history.replaceState(null, "", "/dashboard/agents?demo&shelf=prj_web");
    try {
      const h = world();
      const { unmount } = toolbar(h);
      await screen.findByRole("dialog", { name: "Shelf" });
      expect(options()).toEqual(["Upgrade Next"]);
      unmount();
      resetShelves();
      window.history.replaceState(null, "", "/dashboard/agents?shelf=prj_gone");
      toolbar(world());
      await screen.findByRole("dialog", { name: "Shelf" });
      expect(options()).toHaveLength(3);
    } finally {
      window.history.replaceState(null, "", "/");
    }
  });

  it("counts the focused project's on its trigger, and opens on it", async () => {
    const h = world();
    toolbar(h, WEB.id);
    await userEvent.click(screen.getByRole("button", { name: "Shelf: 1 settled, snoozed or archived" }));
    await screen.findByRole("dialog", { name: "Shelf" });
    expect(options()).toEqual(["Upgrade Next"]);
  });

  it("isn't there when nothing is shelved, and marks a cleaned-up worktree", async () => {
    const empty = agentsHarness({ entities: [mirrored("project", project({ name: "roll" }), 1), mirrored("thread", thread({ id: "thr_live" }), 2)] });
    const { unmount } = toolbar(empty);
    expect(screen.queryByRole("button", { name: /^Shelf/ })).not.toBeInTheDocument();
    unmount();
    const cleaned = thread({
      id: "thr_c",
      title: "Cleaned",
      inbox: settledAt(NOW - D),
      worktree: { path: "/w/thr_c", branch: "roll/cleaned", base: "abc", removed_at: NOW - H },
    });
    openShelf();
    toolbar(world([cleaned]));
    expect(await screen.findByLabelText("Worktree cleaned up")).toBeInTheDocument();
  });

  it("shelfList: a tab with nothing falls to the first with something; search matches every word in title or project", () => {
    const h = world();
    const data = h.client.data;
    expect(shelfList(data, NOW, { tab: "snoozed", project: WEB.id }).tab).toBe("settled");
    expect(shelfList(data, NOW, { query: "WEB next" }).rows.map((r) => r.thread.id)).toEqual(["thr_w"]);
    expect(shelfList(data, NOW, { query: "roll tokio" }).rows.map((r) => r.thread.id)).toEqual(["thr_s1"]);
    expect(shelfList(data, NOW).counts).toEqual({ settled: 3, snoozed: 1, archived: 1 });
  });
});

describe("the conversation's shelf line", () => {
  it("says where a thread is and restores it (T3's Settled line with Un-settle)", async () => {
    const t = thread({ id: "thr_s2", title: "Document the queue", inbox: settledAt(NOW - D, "inactivity") });
    const h = agentsHarness({ entities: [mirrored("project", project(), 1), mirrored("thread", t, 2)] });
    renderAgents(
      <>
        <ShelfStrip thread={t} now={NOW} />
        <InboxToast />
      </>,
      h,
    );
    const line = screen.getByRole("status", { name: "Where this thread is" });
    expect(line).toHaveTextContent("Settled 1d ago after a quiet spell · a message brings it back");
    await userEvent.click(within(line).getByRole("button", { name: "Un-settle" }));
    expect(sent(h).at(-1)).toEqual({ type: "thread.unsettle", thread: "thr_s2" });
  });

  it("an archived thread reads archived; a cleaned-up worktree says its next message puts it back", () => {
    const t = thread({ archived_at: NOW - 2 * D, worktree: { path: "/w/x", branch: "roll/x", base: "abc", removed_at: NOW - D } });
    const h = agentsHarness({ entities: [mirrored("project", project(), 1), mirrored("thread", t, 2)] });
    renderAgents(<ShelfStrip thread={t} now={NOW} />, h);
    const line = screen.getByRole("status", { name: "Where this thread is" });
    expect(line).toHaveTextContent("Archived 2d ago");
    expect(within(line).getByRole("button", { name: "Unarchive" })).toBeInTheDocument();
    expect(line).toHaveTextContent("Worktree cleaned up · roll/x keeps its commits; your next message puts it back");
  });

  it("is nothing for an ordinary thread", () => {
    const t = thread();
    const h = agentsHarness({ entities: [mirrored("project", project(), 1), mirrored("thread", t, 2)] });
    const { container } = renderAgents(<ShelfStrip thread={t} now={NOW} />, h);
    expect(container).toBeEmptyDOMElement();
  });
});
