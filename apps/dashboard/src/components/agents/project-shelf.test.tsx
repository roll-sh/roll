import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";
import { EMPTY_INBOX, type Thread } from "@/lib/agents-api";
import { resetShelves, setShelfOpen } from "@/lib/agents-shelf";
import { ProjectNode } from "./nodes";
import { InboxToast, ShelfStrip, showInboxToast } from "./threads-sidebar";
import { machine, mirrored, project, thread } from "@/test/agents";
import { agentsHarness, renderAgents, type AgentsHarness } from "@/test/agents-render";

const NOW = Date.now();
const H = 3_600_000;
const D = 24 * H;
const box = (over: Partial<Thread["inbox"]>) => ({ ...EMPTY_INBOX, ...over });
const settledAt = (at: number, by: "user" | "inactivity" = "user") => box({ settle: { state: "settled", at, by } });

function world(extra: Thread[] = []) {
  const threads = [
    thread({ id: "thr_live", title: "Still going", created_at: NOW - H }),
    thread({ id: "thr_s1", title: "Bump tokio", inbox: settledAt(NOW - 2 * D, "inactivity") }),
    thread({ id: "thr_s2", title: "Document the queue", inbox: settledAt(NOW - D) }),
    thread({ id: "thr_z", title: "Review retries", inbox: box({ snooze: { until: NOW + 3 * H, at: NOW } }) }),
    thread({ id: "thr_a", title: "Spike: edge middleware", archived_at: NOW - 5 * D }),
    ...extra,
  ];
  return agentsHarness({ entities: [mirrored("project", project({ name: "roll" }), 1), ...threads.map((t, i) => mirrored("thread", t, i + 2))] });
}

function card(h: AgentsHarness) {
  return renderAgents(
    <>
      <ProjectNode project={project({ name: "roll" })} machine={machine()} />
      <InboxToast />
    </>,
    h,
  );
}

const sent = (h: AgentsHarness) => h.command.mock.calls.map(([, , c]) => c);

afterEach(() => {
  resetShelves();
  showInboxToast(null);
  localStorage.clear();
});

describe("a project's shelf on the canvas", () => {
  it("counts what left the canvas, and clicking the project opens it (remembered)", async () => {
    const h = world();
    card(h);
    const toggle = screen.getByRole("button", { name: "Settled 2 · Snoozed 1 · Archived 1" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByText("Bump tokio")).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "roll" }));
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(localStorage.getItem("roll:agents:shelves")).toBe('["prj_1234567890abcdef"]'.replace("prj_1234567890abcdef", project().id));
    // Settled first, most recently settled first.
    const list = screen.getByRole("list", { name: "Conversations on the shelf" });
    expect(within(list).getAllByRole("listitem").map((li) => li.textContent)).toEqual([expect.stringContaining("Document the queue"), expect.stringContaining("Bump tokio")]);
    await userEvent.click(screen.getByRole("button", { name: "Archived 1", pressed: false }));
    expect(screen.getByText("Spike: edge middleware")).toBeInTheDocument();
    await userEvent.click(toggle);
    expect(screen.queryByText("Spike: edge middleware")).not.toBeInTheDocument();
  });

  it("opens a conversation in the side panel, transcript and all", async () => {
    const h = world();
    setShelfOpen(project().id, true);
    card(h);
    await userEvent.click(screen.getByRole("button", { name: /^Bump tokio/ }));
    expect(h.openThread).toHaveBeenCalledWith("thr_s1");
  });

  it("restores in one click, with T3's undo notice (Undo or ⌘Z)", async () => {
    const h = world();
    setShelfOpen(project().id, true);
    card(h);
    await userEvent.click(screen.getByRole("button", { name: "Un-settle Document the queue" }));
    expect(sent(h).at(-1)).toEqual({ type: "thread.unsettle", thread: "thr_s2" });
    const notice = await screen.findByRole("status");
    expect(notice).toHaveTextContent("Un-settled “Document the queue”");
    await userEvent.click(within(notice).getByRole("button", { name: "Undo" }));
    expect(sent(h).at(-1)).toEqual({ type: "thread.settle", thread: "thr_s2" });

    await userEvent.click(screen.getByRole("button", { name: "Archived 1", pressed: false }));
    await userEvent.click(screen.getByRole("button", { name: "Unarchive Spike: edge middleware" }));
    expect(sent(h).at(-1)).toEqual({ type: "thread.unarchive", thread: "thr_a" });
    await screen.findByText("Unarchived “Spike: edge middleware”");
    await userEvent.keyboard("{Meta>}z{/Meta}");
    expect(sent(h).at(-1)).toEqual({ type: "thread.archive", thread: "thr_a" });

    // A snooze woken early comes back with its own time.
    await userEvent.click(screen.getByRole("button", { name: "Snoozed 1", pressed: false }));
    await userEvent.click(screen.getByRole("button", { name: "Wake now Review retries" }));
    expect(sent(h).at(-1)).toEqual({ type: "thread.unsnooze", thread: "thr_z" });
    await userEvent.click(within(await screen.findByRole("status")).getByRole("button", { name: "Undo" }));
    expect(sent(h).at(-1)).toEqual({ type: "thread.snooze", thread: "thr_z", until: NOW + 3 * H });
  });

  it("isn't there when nothing is shelved, and marks a cleaned-up worktree", async () => {
    const empty = agentsHarness({ entities: [mirrored("project", project({ name: "roll" }), 1), mirrored("thread", thread({ id: "thr_live" }), 2)] });
    const { unmount } = card(empty);
    expect(screen.queryByRole("button", { name: /Settled/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "roll" })).not.toBeInTheDocument();
    unmount();
    const cleaned = thread({
      id: "thr_c",
      title: "Cleaned",
      inbox: settledAt(NOW - D),
      worktree: { path: "/w/thr_c", branch: "roll/cleaned", base: "abc", removed_at: NOW - H },
    });
    setShelfOpen(project().id, true);
    card(world([cleaned]));
    expect(screen.getByLabelText("Worktree cleaned up")).toBeInTheDocument();
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
