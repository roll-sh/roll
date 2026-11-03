import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { ThreadNode } from "./nodes";
import { ThreadSession } from "./session";
import { approval, M1, machine, mirrored, node, project, run, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";

describe("ThreadNode", () => {
  it("shows what the agent is doing, what waits for you, and what it cost", async () => {
    const t = thread({ status: "waiting", pending_requests: 1, activity: "Waiting to run `cargo test`", run_count: 3, usage: { input_tokens: 12000, output_tokens: 3400, cached_tokens: 0, cost_usd: 0.42 }, queued: [{ id: "q1", text: "x", attachments: [], at: 0 }] });
    const h = agentsHarness({ entities: [mirrored("thread", t, 1), mirrored("run", run({ stat: { files: 2, additions: 14, deletions: 3 } }), 2)] });
    renderAgents(<ThreadNode thread={t} machine={machine()} />, h);
    const card = screen.getByRole("button", { name: "Thread Fix the flaky queue test" });
    expect(within(card).getByText("Waiting to run `cargo test`")).toBeInTheDocument();
    expect(within(card).getByTitle("1 waiting for you")).toHaveTextContent("1");
    expect(within(card).getByText("Claude Code · Opus 5.5 · high")).toBeInTheDocument();
    expect(within(card).getByText(/15k tok · \$0\.42/)).toBeInTheDocument();
    expect(within(card).getByText(/1 queued/)).toBeInTheDocument();
    expect(within(card).getByText("+14")).toBeInTheDocument();
    await userEvent.click(card);
    expect(h.setExpanded).toHaveBeenCalledWith("thr_1", true);
  });

  it("shows the last prompt when idle and the error when it failed", () => {
    const h = agentsHarness({ entities: [mirrored("run", run({ status: "completed", prompt: "Find out why" }), 2)] });
    const { rerender } = renderAgents(<ThreadNode thread={thread({ run_count: 1 })} machine={machine()} />, h);
    expect(screen.getByText("Find out why")).toBeInTheDocument();
    rerender(<ThreadNode thread={thread({ status: "error", error: "claude exited with 1" })} machine={machine()} />);
    expect(screen.getByText("claude exited with 1")).toBeInTheDocument();
  });
});

describe("ThreadSession", () => {
  const busy = thread({ status: "waiting", active_run: "run_2", run_count: 2, pending_requests: 1, queued: [{ id: "q1", text: "then run clippy", attachments: [], at: 0 }] });
  const entities = [
    mirrored("project", project(), 1),
    mirrored("thread", busy, 2),
    mirrored("run", run({ id: "run_1", ordinal: 1, status: "completed", prompt: "Look at the queue", completed_at: 1_762_000_060_000, stat: { files: 1, additions: 2, deletions: 1 } }), 3),
    mirrored("run", run({ id: "run_2", ordinal: 2, status: "waiting", prompt: "Now fix it" }), 4),
    mirrored("node", node({ type: "message", text: "The lease **expires**." }, { id: "nod_a", run: "run_1", status: "completed" }), 5),
    mirrored("node", node({ type: "reasoning", text: "Two statements race." }, { id: "nod_b", run: "run_2", ordinal: 1, status: "completed" }), 6),
    mirrored("node", node({ type: "message", text: "Fixing" }, { id: "nod_c", run: "run_2", ordinal: 2 }), 7),
    mirrored("node", node({ type: "request", request: "req_1" }, { id: "nod_d", run: "run_2", ordinal: 3, status: "waiting" }), 8),
    mirrored("request", approval({ run: "run_2", node: "nod_d" }), 9),
    mirrored("checkpoint", { id: "chk_1", thread: "thr_1", run: "run_1", ordinal: 1, commit: "abc", stat: { files: 1, additions: 2, deletions: 1 }, created_at: 0 }, 10),
  ];

  it("renders every run's transcript, streaming text and the approval in place", async () => {
    const h = agentsHarness({ entities, follow: ["thr_1"] });
    h.client.enqueue({ type: "delta", machine: M1, delta: { type: "text", thread: "thr_1", node: "nod_c", text: " the claim" } });
    const { container } = renderAgents(<ThreadSession thread={busy} machine={machine()} />, h);
    expect(screen.getByText("Look at the queue")).toBeInTheDocument();
    expect(screen.getByText("expires")).toBeInTheDocument();
    expect(screen.getByText("Fixing the claim")).toBeInTheDocument();
    expect(container.querySelector(".caret")).not.toBeNull();
    // Reasoning is collapsed.
    expect(screen.queryByText("Two statements race.")).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Thought" }));
    expect(screen.getByText("Two statements race.")).toBeInTheDocument();
    // The approval is answered right here.
    await userEvent.click(screen.getByRole("button", { name: "Approve" }));
    expect(h.command.mock.calls.at(-1)?.[2]).toEqual({ type: "request.respond", request: "req_1", response: { type: "decision", decision: "accept" } });
    expect(h.command.mock.calls.at(-1)?.[0]).toBe(M1);
  });

  it("drops queued messages, forks from a run and rolls back to its checkpoint", async () => {
    const h = agentsHarness({ entities: [...entities.slice(0, 2).map((e) => (e.entity.kind === "thread" ? mirrored("thread", { ...busy, status: "idle", active_run: null }, 2) : e)), ...entities.slice(2)], follow: ["thr_1"], reply: (c) => (c.type === "thread.fork" ? { type: "created", id: "thr_2" } : { type: "ok" }) });
    renderAgents(<ThreadSession thread={{ ...busy, status: "idle", active_run: null }} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Remove from queue" }));
    expect(h.command.mock.calls.at(-1)?.[2]).toEqual({ type: "message.unqueue", thread: "thr_1", message: "q1" });

    const first = screen.getByRole("region", { name: "Run 1" });
    await userEvent.click(within(first).getByRole("button", { name: /Fork/ }));
    expect(h.command.mock.calls.at(-1)?.[2]).toEqual({ type: "thread.fork", thread: "thr_1", at: "run_1" });
    await vi_wait();
    expect(h.setExpanded).toHaveBeenCalledWith("thr_2", true);

    await userEvent.click(within(first).getByRole("button", { name: /Roll back to here/ }));
    await userEvent.click(within(first).getByRole("button", { name: "Roll back" }));
    expect(h.command.mock.calls.at(-1)?.[2]).toEqual({ type: "checkpoint.rollback", thread: "thr_1", checkpoint: "chk_1" });
  });

  it("asks before deleting and closes with the button", async () => {
    const h = agentsHarness({ entities, follow: ["thr_1"] });
    renderAgents(<ThreadSession thread={busy} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    await userEvent.click(screen.getByRole("button", { name: "Delete thread" }));
    expect(h.command.mock.calls.at(-1)?.[2]).toEqual({ type: "thread.delete", thread: "thr_1" });
    await userEvent.click(screen.getByRole("button", { name: "Close (Esc)" }));
    expect(h.setExpanded).toHaveBeenCalledWith("thr_1", false);
  });
});

const vi_wait = () => new Promise((r) => setTimeout(r, 0));
