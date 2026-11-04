import { describe, expect, it } from "vitest";
import { EMPTY_INBOX, type Thread } from "./agents-api";
import { inbox, inboxOrder, nextAfter, rowStatus, shelfOf, shortAgo, snoozeFor, snoozeLeft, snoozePresets } from "./agents-inbox";
import { applyEvents, emptyData } from "./agents-store";
import { ev, machine, project, run, thread } from "@/test/agents";

const NOW = new Date(2026, 10, 4, 15, 30).getTime(); // a Wednesday afternoon
const H = 3_600_000;
const D = 24 * H;
const box = (over: Partial<Thread["inbox"]>) => ({ ...EMPTY_INBOX, ...over });
const settled = (at: number) => box({ settle: { state: "settled", at, by: "inactivity" } });

function data() {
  return applyEvents(emptyData(), [
    { type: "machine", cursor: 1, machine: machine() },
    ev("project", project({ id: "prj_b", name: "web" }), 2),
    ev("project", project({ id: "prj_a", name: "roll" }), 3),
    ev("thread", thread({ id: "thr_new", project: "prj_a", created_at: NOW - H }), 4),
    ev("thread", thread({ id: "thr_old", project: "prj_a", created_at: NOW - 5 * H }), 5),
    ev("run", run({ id: "run_o", thread: "thr_old", requested_at: NOW - 10 * 60_000, completed_at: NOW - 5 * 60_000, status: "completed" }), 6),
    ev("thread", thread({ id: "thr_s1", project: "prj_a", created_at: NOW - 9 * D, inbox: settled(NOW - 3 * D) }), 7),
    ev("thread", thread({ id: "thr_s2", project: "prj_a", created_at: NOW - 9 * D, inbox: settled(NOW - D) }), 8),
    ev("thread", thread({ id: "thr_z", project: "prj_a", created_at: NOW - D, inbox: box({ snooze: { until: NOW + 2 * H, at: NOW - H } }) }), 9),
    ev("thread", thread({ id: "thr_web", project: "prj_b", created_at: NOW - 2 * H }), 10),
    ev("thread", thread({ id: "thr_arch", project: "prj_b", archived_at: NOW }), 11),
    ev("thread", thread({ id: "thr_sub", project: "prj_b", lineage: { parent: "thr_web", relation: "subagent", root: "thr_web", from_run: null } }), 12),
  ]);
}

describe("the threads sidebar", () => {
  it("puts each project's threads on shelves, ordered like T3's sidebar", () => {
    const groups = inbox(data(), NOW);
    expect(groups.map((g) => g.project.name)).toEqual(["roll", "web"]);
    const [roll, web] = groups;
    // Active: the newest return first (a run that just ended counts).
    expect(roll.active.map((r) => r.thread.id)).toEqual(["thr_old", "thr_new"]);
    expect(roll.snoozed.map((r) => r.thread.id)).toEqual(["thr_z"]);
    // Settled: most recently settled first.
    expect(roll.settled.map((r) => r.thread.id)).toEqual(["thr_s2", "thr_s1"]);
    expect(roll.settled[0].at).toBe(NOW - D);
    // Archived threads and delegated subagents aren't listed.
    expect(web.active.map((r) => r.thread.id)).toEqual(["thr_web"]);
    expect(inboxOrder(groups).map((r) => r.thread.id)).toEqual(["thr_old", "thr_new", "thr_z", "thr_s2", "thr_s1", "thr_web"]);
  });

  it("a snooze that's due reads as active until the daemon wakes it; un-settled by hand is active", () => {
    const t = thread({ inbox: box({ snooze: { until: NOW, at: NOW - H } }) });
    expect(shelfOf(t, NOW - 1)).toBe("snoozed");
    expect(shelfOf(t, NOW)).toBe("active");
    expect(shelfOf(thread({ inbox: box({ settle: { state: "active", at: NOW } }) }), NOW)).toBe("active");
    expect(shelfOf(thread({ inbox: { ...settled(NOW), snooze: { until: NOW + H, at: NOW } } }), NOW)).toBe("snoozed");
  });

  it("settle and advance goes to the next active row, wrapping, never to a shelf", () => {
    const groups = inbox(data(), NOW);
    expect(nextAfter(groups, "thr_old")).toBe("thr_new");
    expect(nextAfter(groups, "thr_new")).toBe("thr_web");
    expect(nextAfter(groups, "thr_web")).toBe("thr_old");
    // From a shelf (opened from the sidebar): the next active one after it.
    expect(nextAfter(groups, "thr_s2")).toBe("thr_web");
    const alone = applyEvents(emptyData(), [{ type: "machine", cursor: 1, machine: machine() }, ev("project", project(), 2), ev("thread", thread(), 3)]);
    expect(nextAfter(inbox(alone, NOW), "thr_1")).toBeNull();
  });

  it("says what each row is doing, most urgent first, and what's unread", () => {
    expect(rowStatus(thread({ status: "running" }), null)).toBe("working");
    expect(rowStatus(thread({ status: "waiting", pending_requests: 1 }), null)).toBe("waiting");
    expect(rowStatus(thread({ status: "error" }), NOW)).toBe("failed");
    expect(rowStatus(thread({ inbox: box({ woke_at: NOW, visited_at: NOW - 1 }) }), null)).toBe("woke");
    expect(rowStatus(thread({ inbox: box({ woke_at: NOW, visited_at: NOW }) }), null)).toBeNull();
    expect(rowStatus(thread({ inbox: box({ visited_at: NOW - 1 }) }), NOW)).toBe("done");
    expect(rowStatus(thread({ inbox: box({ visited_at: NOW }) }), NOW)).toBeNull();
  });

  it("offers T3's snooze presets, soonest first, without one that's past", () => {
    const presets = snoozePresets(NOW);
    expect(presets.map((p) => p.label)).toEqual(["In 1 hour", "This evening", "In 3 hours", "Tomorrow", "Next week"]);
    expect(presets[1].until).toBe(new Date(2026, 10, 4, 18, 0).getTime());
    expect(presets[3].until).toBe(new Date(2026, 10, 5, 9, 0).getTime());
    expect(presets[4].until).toBe(new Date(2026, 10, 9, 9, 0).getTime()); // Monday
    const late = snoozePresets(new Date(2026, 10, 4, 19, 0).getTime());
    expect(late.map((p) => p.label)).toEqual(["In 1 hour", "In 3 hours", "Tomorrow", "Next week"]);
    // On a Monday, next week is the Monday after.
    const monday = snoozePresets(new Date(2026, 10, 9, 8, 0).getTime());
    expect(monday.find((p) => p.label === "Next week")!.until).toBe(new Date(2026, 10, 16, 9, 0).getTime());
  });

  it("formats time left and time since like T3", () => {
    expect(snoozeLeft(NOW + 59_000, NOW)).toBe("1m");
    expect(snoozeLeft(NOW + 90 * 60_000, NOW)).toBe("2h");
    expect(snoozeLeft(NOW + 25 * H, NOW)).toBe("2d");
    expect(shortAgo(NOW - 30_000, NOW)).toBe("now");
    expect(shortAgo(NOW - 3 * H, NOW)).toBe("3h");
    expect(shortAgo(NOW - 3 * D, NOW)).toBe("3d");
    expect(shortAgo(NOW - 21 * D, NOW)).toBe("3w");
    expect(snoozeFor(2, "hours", NOW)).toBe(NOW + 2 * H);
    expect(snoozeFor(0, "days", NOW)).toBeNull();
  });
});
