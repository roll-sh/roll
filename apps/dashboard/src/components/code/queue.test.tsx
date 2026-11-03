import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Check, Queue } from "@/lib/code-api";
import { api, resetApi } from "@/test/api";
import { entry, info, queue, repo, T0 } from "@/test/fixtures";
import { QueueTab } from "./queue";
import type { Run } from "./shared";

vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig<typeof import("@/lib/code-api")>()), ...(await import("@/test/api")).apiModule }));

beforeEach(() => resetApi());

const check = (name: string, status: Check["status"], summary: string | null = null): Check => ({
  name,
  status,
  source: "runner",
  url: null,
  summary,
  started_at: T0,
  finished_at: null,
});

const run: Run = async (_key, fn) => fn();

async function show(q: Queue | Error, opts: { writer?: boolean; policy?: "queue" | "post_merge" } = {}) {
  if (q instanceof Error) api.queue.mockRejectedValue(q);
  else api.queue.mockResolvedValue(q);
  const who = opts.writer === false ? info({ required: true, caller: { kind: "anonymous", owner: null } }) : info();
  const view = render(<QueueTab repo={repo({ land_policy: opts.policy ?? "queue" })} info={who} tick={0} busy={null} run={run} />);
  if (!(q instanceof Error)) await screen.findByText("Waiting");
  return view;
}

const panel = (title: string) => screen.getByRole("heading", { name: title }).closest("section")!;
/** The status line at the top: a dot, then the queue in a few words. */
const headline = () => document.querySelector<HTMLElement>("p:has(> span.rounded-full)")!;

describe("QueueTab", () => {
  it("says it's loading, then shows an empty, idle queue", async () => {
    let resolve!: (q: Queue) => void;
    api.queue.mockReturnValue(new Promise((r) => (resolve = r)));
    render(<QueueTab repo={repo()} info={info()} tick={0} busy={null} run={run} />);
    expect(screen.getByText("Loading the queue…")).toBeInTheDocument();
    resolve(queue());
    expect(await screen.findByText("Idle — nothing waiting")).toBeInTheDocument();
    expect(screen.getByText(/Nothing waiting\. Landing a change, or pushing to the default branch, adds it here\./)).toBeInTheDocument();
    expect(screen.getByText("Changes appear here as they land or are ejected.")).toBeInTheDocument();
    expect(screen.getByText("nothing waiting")).toBeInTheDocument();
    expect(screen.getByText("4 / 8")).toBeInTheDocument();
    expect(screen.queryByText("In flight")).toBeNull();
    expect(screen.queryByText("Urgent")).toBeNull();
    expect(api.queue).toHaveBeenCalledWith("acme/web");
  });

  it("shows the error when the queue can't be read", async () => {
    await show(new Error("queue unavailable"));
    expect(await screen.findByText("queue unavailable")).toBeInTheDocument();
  });

  it("paused: says so, and Resume asks the API then refetches", async () => {
    api.resumeQueue.mockResolvedValue(queue());
    await show(queue({ paused: true, state: "paused", entries: [entry({ number: 1 }), entry({ number: 2 })], stats: { ...queue().stats, depth: 2 } }));
    expect(headline()).toHaveTextContent("Paused · 2 waiting");
    expect(headline().querySelector(".shimmer")).toBeNull();
    const calls = api.queue.mock.calls.length;
    await userEvent.click(screen.getByRole("button", { name: "Resume" }));
    expect(api.resumeQueue).toHaveBeenCalledWith("acme/web");
    await waitFor(() => expect(api.queue.mock.calls.length).toBeGreaterThan(calls));
  });

  it("running: Pause asks the API", async () => {
    api.pauseQueue.mockResolvedValue(queue({ paused: true }));
    await show(queue());
    await userEvent.click(screen.getByRole("button", { name: "Pause" }));
    expect(api.pauseQueue).toHaveBeenCalledWith("acme/web");
  });

  it("readers see the queue without controls", async () => {
    await show(queue({ entries: [entry({ number: 3 })], stats: { ...queue().stats, depth: 1 } }), { writer: false });
    expect(screen.getByText("Change 3")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Pause|Resume/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /urgent lane|from the queue/ })).toBeNull();
  });

  it("post_merge: explains the policy, no pause, the waiting list is off", async () => {
    await show(queue({ policy: "post_merge" }), { policy: "post_merge" });
    expect(screen.getByText(/This repository lands post-merge/)).toHaveTextContent("checks run on main afterwards");
    expect(screen.getByText("The queue is off for this repository.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Pause|Resume/ })).toBeNull();
  });

  it("says when roll.toml couldn't be read", async () => {
    await show(queue({ settings: { ...queue().settings, error: "line 3: expected a table" } }));
    expect(screen.getByText(/roll\.toml on main couldn't be read, so the defaults apply: line 3: expected a table/)).toBeInTheDocument();
  });

  it("urgent and normal lanes, in order, with lane and remove controls", async () => {
    api.moveLane.mockResolvedValue({} as never);
    api.dequeue.mockResolvedValue({} as never);
    await show(
      queue({
        entries: [
          entry({ number: 5, lane: "urgent", position: 1, title: "Hotfix", enqueued_by: "bob" }),
          entry({ number: 6, position: 2, title: "Feature A" }),
          entry({ number: 7, position: 3, title: "Feature B" }),
        ],
        stats: { ...queue().stats, depth: 3 },
      }),
    );
    expect(screen.getByText("1 urgent")).toBeInTheDocument();
    const urgent = screen.getByRole("heading", { name: "Urgent" }).closest("section")!;
    const normal = screen.getByRole("heading", { name: "Normal" }).closest("section")!;
    expect(within(urgent).getAllByRole("listitem").map((li) => li.querySelector("p")!.textContent)).toEqual(["Hotfix"]);
    expect(within(normal).getAllByRole("listitem").map((li) => li.querySelector("p")!.textContent)).toEqual(["Feature A", "Feature B"]);
    expect(within(urgent).getByText(/#5 · ada · queued by bob/)).toBeInTheDocument();
    expect(within(urgent).getByRole("link")).toHaveAttribute("href", "/dashboard/code/acme/web/changes/5");
    // Urgent entries can't move up a lane; normal ones can.
    expect(within(urgent).queryByRole("button", { name: /urgent lane/ })).toBeNull();
    await userEvent.click(within(normal).getByRole("button", { name: "Move #7 to the urgent lane" }));
    expect(api.moveLane).toHaveBeenCalledWith("chg_7", "urgent");
    await userEvent.click(within(urgent).getByRole("button", { name: "Remove #5 from the queue" }));
    expect(api.dequeue).toHaveBeenCalledWith("chg_5");
  });

  it("testing: the batch in landing order, its checks, and the waiting rest", async () => {
    const entries = [entry({ number: 1, title: "One", state: "testing" }), entry({ number: 2, title: "Two", state: "testing" }), entry({ number: 3, title: "Three" })];
    await show(
      queue({
        state: "testing",
        batch: { id: "qb_1", changes: ["chg_2", "chg_1"], tip: "cs_tip0000000", started_at: T0, state: "testing", testing_prefix: 2, checks: [check("test", "running", "412 passed"), check("lint", "success")] },
        entries,
        stats: { ...queue().stats, depth: 3 },
      }),
    );
    expect(headline()).toHaveTextContent("Testing a batch of 2 · 1 waiting");
    expect(headline().querySelector(".shimmer")).not.toBeNull();
    const flight = panel("In flight");
    expect(within(flight).getByText(/Testing the batch rebased on the default branch/)).toBeInTheDocument();
    const order = within(flight).getByRole("list", { name: "Changes in the batch, in landing order" });
    const segs = within(order).getAllByRole("link");
    expect(segs.map((a) => a.getAttribute("title"))).toEqual(["Two", "One"]);
    for (const s of segs) {
      expect(s).toHaveClass("border-accent/50");
      expect(s).not.toHaveClass("opacity-50");
    }
    expect(within(flight).getByText("412 passed")).toBeInTheDocument();
    expect(within(flight).getByText("lint")).toBeInTheDocument();
    expect(within(flight).getByText("tip cs_tip0000000")).toBeInTheDocument();
    // Batch members aren't in the waiting lanes.
    const normal = screen.getByRole("heading", { name: "Normal" }).closest("section")!;
    expect(within(normal).getAllByRole("listitem")).toHaveLength(1);
    expect(within(normal).getByText("Three")).toBeInTheDocument();
  });

  it("bisecting: tests a prefix; the rest of the batch waits behind a divider", async () => {
    const entries = [1, 2, 3].map((n) => entry({ number: n, state: "bisecting" }));
    await show(
      queue({
        state: "bisecting",
        batch: { id: "qb_1", changes: ["chg_1", "chg_2", "chg_3"], tip: null, started_at: T0, state: "bisecting", testing_prefix: 1, checks: [check("test", "failure")] },
        entries,
        stats: { ...queue().stats, depth: 3, batch_size: 2 },
      }),
    );
    expect(headline()).toHaveTextContent("Bisecting a batch of 3");
    const flight = panel("In flight");
    expect(within(flight).getByText(/Testing the first 1 of 3 to find the change that broke it/)).toBeInTheDocument();
    const segs = within(within(flight).getByRole("list", { name: /landing order/ })).getAllByRole("link");
    expect(segs.map((s) => s.classList.contains("opacity-50"))).toEqual([false, true, true]);
    expect(flight.querySelectorAll("[aria-hidden].w-px")).toHaveLength(1);
    expect(screen.getByText("smaller after a red batch")).toBeInTheDocument();
  });

  it("bisecting without a prefix yet reads generically", async () => {
    await show(
      queue({
        state: "bisecting",
        batch: { id: "qb_1", changes: ["chg_1", "chg_2"], tip: null, started_at: T0, state: "bisecting", testing_prefix: null, checks: [] },
        entries: [entry({ number: 1 }), entry({ number: 2 })],
        stats: { ...queue().stats, depth: 2 },
      }),
    );
    expect(screen.getByText(/Testing its prefixes to find the change that broke it/)).toBeInTheDocument();
  });

  it("landing: green segments; pausing after the batch", async () => {
    await show(
      queue({
        state: "landing",
        paused: true,
        batch: { id: "qb_1", changes: ["chg_1"], tip: "cs_tip0000000", started_at: T0, state: "landing", testing_prefix: null, checks: [] },
        entries: [entry({ number: 1, state: "landing" })],
        stats: { ...queue().stats, depth: 1 },
      }),
    );
    expect(headline()).toHaveTextContent("Landing a batch of 1 · pausing after it");
    const flight = panel("In flight");
    expect(within(flight).getByText("Green. Landing the whole batch with one compare-and-swap.")).toBeInTheDocument();
    expect(within(flight).getByRole("link")).toHaveClass("border-ok/50");
    // Paused but still landing: the toggle offers Resume.
    expect(screen.getByRole("button", { name: "Resume" })).toBeInTheDocument();
  });

  it("recently: landed and ejected changes, with the error and its output on demand", async () => {
    await show(
      queue({
        last_green: "cs_green00000",
        recent: [
          entry({ number: 9, title: "Broke it", state: "ejected", enqueued_at: T0, finished_at: T0 + 125, error: "test failed: 2 of 410", output: "test:\nFAIL parser.spec.ts\n  expected 3, got 4" }),
          entry({ number: 10, title: "Conflicted", state: "ejected", enqueued_at: T0, finished_at: T0 + 30, error: "conflicts with main in src/app.ts", output: null }),
          entry({ number: 8, title: "Fine", state: "landed", enqueued_at: T0, finished_at: T0 + 3720 }),
        ],
      }),
    );
    const recent = panel("Recently");
    expect(within(recent).getByText("cs_green00000")).toHaveClass("text-ok");
    const rows = within(recent).getAllByRole("listitem");
    expect(rows).toHaveLength(3);
    expect(rows[0]).toHaveTextContent("#9 · ada · ejected after 2m");
    expect(rows[2]).toHaveTextContent("#8 · ada · landed after 1h 2m");
    expect(rows[2].querySelector("details")).toBeNull();

    const failed = rows[0].querySelector("details")!;
    expect(failed).not.toHaveAttribute("open");
    expect(within(failed).getByText(/test failed: 2 of 410/)).toHaveClass("text-danger");
    expect(within(failed).getByText("· show output")).toBeInTheDocument();
    await userEvent.click(within(failed).getByText(/test failed: 2 of 410/));
    expect(failed).toHaveAttribute("open");
    expect(failed.querySelector("pre")).toHaveTextContent("FAIL parser.spec.ts expected 3, got 4");

    const conflict = rows[1].querySelector("details")!;
    expect(within(conflict).getByText("conflicts with main in src/app.ts")).toBeInTheDocument();
    expect(within(conflict).queryByText("· show output")).toBeNull();
    expect(conflict.querySelector("pre")).toBeNull();
  });
});
