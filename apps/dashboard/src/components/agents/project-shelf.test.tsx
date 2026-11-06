import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";
import { EMPTY_INBOX, type Thread } from "@/lib/agents-api";
import { resetShelves, shelfList } from "@/lib/agents-shelf";
import { ShelfControl } from "./project-shelf";
import { InboxToast, showInboxToast } from "./threads-sidebar";
import { mirrored, project, thread } from "@/test/agents";
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

afterEach(() => {
  resetShelves();
  showInboxToast(null);
  localStorage.clear();
});

describe("the shelf (the canvas toolbar's Shelf)", () => {
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

  it("shelfList: a tab with nothing falls to the first with something; search matches every word in title or project", () => {
    const h = world();
    const data = h.client.data;
    expect(shelfList(data, NOW, { tab: "snoozed", project: WEB.id }).tab).toBe("settled");
    expect(shelfList(data, NOW, { query: "WEB next" }).rows.map((r) => r.thread.id)).toEqual(["thr_w"]);
    expect(shelfList(data, NOW, { query: "roll tokio" }).rows.map((r) => r.thread.id)).toEqual(["thr_s1"]);
    expect(shelfList(data, NOW).counts).toEqual({ settled: 3, snoozed: 1, archived: 1 });
  });
});
