import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ThreadNode } from "./nodes";
import { Conversation } from "./session";
import { ThreadMenuHost } from "./thread-menu";
import { approval, M1, machine, mirrored, node, project, run, terminal, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";

// xterm needs a real canvas: the drawer's terminal is a stand-in here.
vi.mock("./terminal", () => ({ TerminalView: ({ terminal }: { terminal: { title: string } }) => <div>terminal {terminal.title}</div> }));

describe("ThreadNode", () => {
  it("shows what the agent is doing, what waits for you, and what it cost", async () => {
    const t = thread({ status: "waiting", pending_requests: 2, activity: "Waiting to run `cargo test`", run_count: 3, usage: { input_tokens: 12000, output_tokens: 3400, cached_tokens: 0, cost_usd: 0.42 }, queued: [{ id: "q1", text: "x", attachments: [], at: 0 }] });
    const asks = [mirrored("request", approval(), 3), mirrored("request", approval({ id: "req_2", node: "nod_8" }), 4)];
    const h = agentsHarness({ entities: [mirrored("thread", t, 1), mirrored("run", run({ stat: { files: 2, additions: 14, deletions: 3 } }), 2), ...asks] });
    renderAgents(
      <>
        <ThreadNode thread={t} machine={machine()} />
        <ThreadMenuHost />
      </>,
      h,
    );
    const card = screen.getByRole("button", { name: "Thread Fix the flaky queue test" });
    // Waiting on you is its requests here: the pill says so, the line says what's asked, and how many.
    expect(within(card).getByRole("img", { name: "Pending approval" })).toBeInTheDocument();
    expect(within(card).getByText("Run `cargo test -p code-control`")).toBeInTheDocument();
    expect(within(card).getByTitle("2 waiting for you")).toHaveTextContent("2");
    expect(within(card).getByText(/15k tok · \$0\.42/)).toBeInTheDocument();
    await userEvent.click(card);
    expect(h.openThread).toHaveBeenCalledWith("thr_1");
    // Its menu on a right-click (T3's thread menu): rename it without opening it.
    await userEvent.pointer({ keys: "[MouseRight]", target: card });
    await userEvent.click(await screen.findByRole("menuitem", { name: "Rename…" }));
    const title = await screen.findByRole("textbox", { name: "Title" });
    await userEvent.clear(title);
    await userEvent.paste("Fix the queue test");
    await userEvent.click(screen.getByRole("button", { name: "Rename" }));
    expect(h.command.mock.calls.map(([, , c]) => c)).toContainEqual({ type: "thread.configure", thread: "thr_1", title: "Fix the queue test" });
  });
});

describe("Conversation", () => {
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
    const { container } = renderAgents(<Conversation thread={busy} machine={machine()} />, h);
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
    renderAgents(<Conversation thread={{ ...busy, status: "idle", active_run: null }} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Remove from queue" }));
    expect(h.command.mock.calls.at(-1)?.[2]).toEqual({ type: "message.unqueue", thread: "thr_1", message: "q1" });

    const first = screen.getByRole("region", { name: "Run 1" });
    await userEvent.click(within(first).getByRole("button", { name: /Fork/ }));
    expect(h.command.mock.calls.at(-1)?.[2]).toEqual({ type: "thread.fork", thread: "thr_1", at: "run_1" });
    await vi_wait();
    expect(h.openThread).toHaveBeenCalledWith("thr_2");

    await userEvent.click(within(first).getByRole("button", { name: /Roll back to here/ }));
    await userEvent.click(within(first).getByRole("button", { name: "Roll back" }));
    expect(h.command.mock.calls.at(-1)?.[2]).toEqual({ type: "checkpoint.rollback", thread: "thr_1", checkpoint: "chk_1" });
  });

  it("opens the thread's terminal in a drawer under the conversation (T3's), with tabs, a new one and hide", async () => {
    const term = terminal({ id: "trm_1", thread: "thr_1", title: "zsh" });
    let made = 0;
    const h = agentsHarness({
      entities: [...entities, mirrored("terminal", term, 11)],
      follow: ["thr_1"],
      reply: (c) => (c.type === "terminal.open" ? ({ type: "created", id: `trm_new${++made}` } as never) : { type: "ok" }),
    });
    renderAgents(<Conversation thread={busy} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Terminal in this thread's directory" }));
    // The open terminal is reused, not a new one.
    expect(h.command.mock.calls.some((c) => c[2].type === "terminal.open")).toBe(false);
    const drawer = screen.getByRole("region", { name: "Terminal" });
    expect(within(drawer).getByRole("tab", { name: /zsh/, selected: true })).toBeInTheDocument();
    // The header's toggle hides it.
    await userEvent.click(screen.getByRole("button", { name: "Hide the terminal" }));
    expect(screen.queryByRole("region", { name: "Terminal" })).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Terminal in this thread's directory" }));
    await userEvent.click(within(screen.getByRole("region", { name: "Terminal" })).getByRole("button", { name: "New terminal" }));
    expect(h.command.mock.calls.at(-1)?.[2]).toEqual({ type: "terminal.open", thread: "thr_1", cols: 100, rows: 26 });
  });

  it("shows the diff beside the transcript", async () => {
    const h = agentsHarness({ entities, follow: ["thr_1"], reply: (c) => (c.type === "diff.get" ? { type: "diff", files: [], stat: { files: 0, additions: 0, deletions: 0 }, truncated: false } as never : { type: "ok" }) });
    renderAgents(<Conversation thread={busy} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Diff" }));
    expect(h.command.mock.calls.at(-1)?.[2]).toEqual({ type: "diff.get", thread: "thr_1" });
    expect(await screen.findByText("No changes yet.")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Hide the diff" }));
    expect(screen.queryByText("No changes yet.")).toBeNull();
  });
});

const vi_wait = () => new Promise((r) => setTimeout(r, 0));
