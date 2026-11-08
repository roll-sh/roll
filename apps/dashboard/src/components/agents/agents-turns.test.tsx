import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Command } from "@/lib/agents-api";
import { PLAN_IMPLEMENTATION_PREFIX } from "@/lib/agents-plan";
import { machine, mirrored, node, project, provider, run, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";
import { Composer } from "./composer";
import { Conversation } from "./session";

// xterm needs a real canvas.
vi.mock("./terminal", () => ({ TerminalView: () => <div>terminal</div> }));

const caps = { steer: true, fork: true, rollback: false, plan_mode: true, images: true, subagents: true, mode_switch: true, compact: true };
const m = machine({ info: { ...machine().info!, providers: [provider({ capabilities: caps })] } });
const sent = (h: ReturnType<typeof agentsHarness>) => h.command.mock.calls.map((c) => c[2] as Command);
const q = (id: string, text: string) => ({ id, text, attachments: [], at: 0 });

describe("the queue (T3's queued messages)", () => {
  const busy = thread({ status: "running", active_run: "run_1", run_count: 1, queued: [q("q1", "then run clippy"), q("q2", "and the docs"), q("q3", "and a changelog")] });

  it("sends one now, edits one in the composer, removes and reorders them", async () => {
    const h = agentsHarness({ machines: [m], entities: [mirrored("thread", busy, 1), mirrored("run", run({ status: "running" }), 2)] });
    renderAgents(<Composer mode={{ kind: "thread", thread: busy, machine: m }} />, h);
    const row = (name: RegExp) => screen.getByRole("listitem", { name });

    await userEvent.click(within(row(/and the docs/)).getByRole("button", { name: "Send now: into the current turn" }));
    expect(sent(h).at(-1)).toEqual({ type: "message.promote", thread: "thr_1", message: "q2" });

    await userEvent.click(within(row(/then run clippy/)).getByRole("button", { name: "Remove from queue" }));
    expect(sent(h).at(-1)).toEqual({ type: "message.unqueue", thread: "thr_1", message: "q1" });

    // ⌥↓ on a row moves it down one (in front of the one after next, or last).
    row(/then run clippy/).focus();
    await userEvent.keyboard("{Alt>}{ArrowDown}{/Alt}");
    expect(sent(h).at(-1)).toEqual({ type: "message.reorder", thread: "thr_1", message: "q1", before: "q3" });

    // Edit: the text comes into the composer, the row says so, and the button updates the message.
    const box = screen.getByRole("textbox", { name: "Message" });
    await userEvent.type(box, "my draft");
    await userEvent.click(within(row(/and a changelog/)).getByRole("button", { name: "Edit in the composer" }));
    expect(box).toHaveValue("and a changelog");
    expect(screen.getByText("Editing in the composer")).toBeInTheDocument();
    await userEvent.clear(box);
    await userEvent.type(box, "and a CHANGELOG entry");
    await userEvent.click(screen.getByRole("button", { name: "Update queued message" }));
    expect(sent(h).at(-1)).toEqual({ type: "message.edit", thread: "thr_1", message: "q3", text: "and a CHANGELOG entry", attachments: [] });
    // The draft it pushed aside comes back.
    await vi.waitFor(() => expect(box).toHaveValue("my draft"));
  });
});

describe("compaction (T3's /compact and its row)", () => {
  it("compacts from the context meter and from /compact, which queues behind a running turn", async () => {
    const idle = thread({ context: { used_tokens: 180_000, max_tokens: 200_000, total_processed_tokens: null } });
    const h = agentsHarness({ machines: [m], entities: [mirrored("thread", idle, 1)] });
    const { rerender } = renderAgents(<Composer mode={{ kind: "thread", thread: idle, machine: m }} />, h);
    await userEvent.hover(screen.getByRole("button", { name: "Context window 90% used" }));
    await userEvent.click(await screen.findByRole("button", { name: /Compact context/ }));
    expect(sent(h).at(-1)).toEqual({ type: "message.send", thread: "thr_1", text: "/compact" });

    // The slash menu offers it; Enter runs the highlighted command.
    const busy = { ...idle, status: "running" as const, active_run: "run_1" };
    rerender(<Composer mode={{ kind: "thread", thread: busy, machine: m }} />);
    const box = screen.getByRole("textbox", { name: "Message" });
    await userEvent.type(box, "/com");
    expect(within(screen.getByRole("listbox", { name: "Commands" })).getAllByRole("option")).toHaveLength(1);
    await userEvent.keyboard("{Enter}");
    expect(sent(h).at(-1)).toEqual({ type: "message.send", thread: "thr_1", text: "/compact", mode: "queue" });
  });
});

describe("plan mode (T3's proposed plan)", () => {
  const plan = "# Export invoices as CSV\n\n1. Add the route.\n2. Add the button.";
  const inPlan = thread({ interaction_mode: "plan", run_count: 1 });
  const entities = [
    mirrored("project", project(), 1),
    mirrored("thread", inPlan, 2),
    mirrored("run", run({ status: "completed", completed_at: 2 }), 3),
    mirrored("node", node({ type: "plan", text: plan }, { id: "nod_p", status: "completed" }), 4),
  ];
  const reply = (c: Command) => (c.type === "thread.create" ? ({ type: "created", id: "thr_2" } as const) : ({ type: "ok" } as const));

  it("a plan the run ended with: Implement switches to build mode and sends it; Refine sends what's typed", async () => {
    const h = agentsHarness({ machines: [m], entities, follow: ["thr_1"], reply });
    renderAgents(<Conversation thread={inPlan} machine={m} />, h);
    expect(screen.getByRole("status")).toHaveTextContent("Plan ready· Export invoices as CSV");
    expect(screen.getByLabelText("Plan: Export invoices as CSV")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Implement" }));
    expect(sent(h).slice(-2)).toEqual([
      { type: "thread.configure", thread: "thr_1", interaction_mode: "default" },
      { type: "message.send", thread: "thr_1", text: `${PLAN_IMPLEMENTATION_PREFIX}${plan}` },
    ]);
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "Stream it in pages");
    await userEvent.click(screen.getByRole("button", { name: "Refine" }));
    expect(sent(h).at(-1)).toEqual({ type: "message.send", thread: "thr_1", text: "Stream it in pages" });
  });
});

describe("subagents (T3's subagent rows)", () => {
  const busy = thread({ status: "running", active_run: "run_1", run_count: 1 });
  const sub = (id: string, ordinal: number, over: object, status: "running" | "completed" = "running") =>
    mirrored("node", node({ type: "subagent", agent: "Explore", prompt: `Find the callers of ${id}`, child: null, result: null, progress: null, model: "claude-haiku-5-5", ...over }, { id, ordinal, status, started_at: 1_000, completed_at: status === "completed" ? 31_000 : null }), 10 + ordinal);
  const entities = [
    mirrored("project", project(), 1),
    mirrored("thread", busy, 2),
    mirrored("run", run({ status: "running" }), 3),
    sub("nod_a", 1, { progress: "Reading src/queue.rs" }),
    sub("nod_b", 2, { result: "3 callers, all retry." }, "completed"),
    mirrored("node", node({ type: "tool", name: "Read", kind: "read", title: "src/queue.rs", input: null, output: null, exit_code: null, files: [] }, { id: "nod_t", parent: "nod_a", ordinal: 3, status: "completed" }), 20),
    mirrored("node", node({ type: "message", text: "Now the docs." }, { id: "nod_m", ordinal: 4, status: "completed" }), 21),
    mirrored("node", node({ type: "subagent", agent: "codex", prompt: "Write the docs", child: "thr_child", result: null }, { id: "nod_c", ordinal: 5 }), 22),
  ];

  it("groups the ones started together, with what each does now or said, and opens a provider's subagent in place", async () => {
    const h = agentsHarness({ machines: [m], entities, follow: ["thr_1"], ctx: { now: 46_000 } });
    renderAgents(<Conversation thread={busy} machine={m} />, h);
    const group = screen.getByRole("region", { name: "2 subagents" });
    expect(group).toHaveTextContent("2 subagents· 1 running");
    const a = within(group).getByRole("button", { name: "Open Find the callers of nod_a" });
    expect(a).toHaveTextContent("Reading src/queue.rs");
    expect(a).toHaveTextContent("45s");
    expect(within(group).getByRole("button", { name: "Open Find the callers of nod_b" })).toHaveTextContent("3 callers, all retry.30s");

    // Its own transcript, with T3's bar in place of the composer.
    await userEvent.click(a);
    expect(screen.getByRole("region", { name: "Subagent Explore" })).toBeInTheDocument();
    expect(screen.getByText("Read src/queue.rs")).toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "Message" })).toBeNull();
    const bar = screen.getByRole("status");
    expect(bar).toHaveTextContent("Exploreclaude-haiku-5-5Working 45s·Runs on its own");
    await userEvent.click(within(bar).getByRole("button", { name: /Open parent/ }));
    expect(screen.getByRole("textbox", { name: "Message" })).toBeInTheDocument();

    // A delegated thread opens as the thread it is.
    await userEvent.click(screen.getByRole("button", { name: "Open Write the docs" }));
    expect(h.openThread).toHaveBeenCalledWith("thr_child");
  });
});

describe("a run's changed files (T3's changed-files card reads the checkpoint diff)", () => {
  // The tool said it added a line; git says it replaced one, and a command changed another file.
  const edit = { type: "tool", name: "Edit", kind: "edit", title: "Edit ROLL_DEMO.md", input: null, output: null, exit_code: 0, files: [{ path: "ROLL_DEMO.md", change: "modify", diff: "@@ -0,0 +1 @@\n+hello" }] } as const;
  const done = run({
    status: "completed",
    completed_at: 2,
    stat: { files: 2, additions: 3, deletions: 2 },
    files: [
      { path: "ROLL_DEMO.md", change: "modify", additions: 1, deletions: 1 },
      { path: "Cargo.lock", change: "modify", additions: 2, deletions: 1 },
    ],
  });
  const entities = (r = done) => [mirrored("thread", thread({ run_count: 1 }), 1), mirrored("run", r, 2), mirrored("node", node(edit as never, { status: "completed" }), 3)];

  it("counts what the run's diff counts, files a command changed too, and the footer's numbers open that diff", async () => {
    const h = agentsHarness({ machines: [m], entities: entities(), follow: ["thr_1"] });
    renderAgents(<Conversation thread={thread({ run_count: 1 })} machine={m} />, h);
    const box = screen.getByLabelText("Changed files");
    expect(box).toHaveTextContent(/2 changed files\s*\+3 −2/);
    expect(within(box).getByText("Cargo.lock")).toBeInTheDocument();
    expect(within(box).getByRole("button", { name: /ROLL_DEMO\.md/ })).toHaveTextContent("+1 −1");
    await userEvent.click(screen.getByRole("button", { name: "Last run: 3 added, 2 removed. Open its diff" }));
    expect(sent(h).at(-1)).toEqual({ type: "diff.get", thread: "thr_1", run: "run_1" });
  });
});
