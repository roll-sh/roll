import { act, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Command, Thread } from "@/lib/agents-api";
import { PLAN_IMPLEMENTATION_PREFIX } from "@/lib/agents-plan";
import { approval, machine, mirrored, node, project, provider, run, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";
import { Composer } from "./composer";
import { stepTarget } from "./queue";
import { Conversation } from "./session";
import { CompactionRow } from "./transcript";

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

  it("⌥↑ edits the last queued message, Escape stops, and ⌘⇧Enter sends the first into the turn", async () => {
    const h = agentsHarness({ machines: [m], entities: [mirrored("thread", busy, 1)] });
    renderAgents(<Composer mode={{ kind: "thread", thread: busy, machine: m }} />, h);
    const box = screen.getByRole("textbox", { name: "Message" });
    box.focus();
    await userEvent.keyboard("{Alt>}{ArrowUp}{/Alt}");
    expect(box).toHaveValue("and a changelog");
    await userEvent.keyboard("{Escape}");
    expect(box).toHaveValue("");
    await userEvent.keyboard("{Control>}{Shift>}{Enter}{/Shift}{/Control}");
    expect(sent(h).at(-1)).toEqual({ type: "message.promote", thread: "thr_1", message: "q1" });
  });

  it("says a stopped thread's queue is paused, and Resume (or the empty send button) sends the first", async () => {
    const held = { ...busy, status: "idle" as const, active_run: null };
    const h = agentsHarness({ machines: [m], entities: [mirrored("thread", held, 1)] });
    renderAgents(<Composer mode={{ kind: "thread", thread: held, machine: m }} />, h);
    expect(screen.getByText("Queue paused: the last run stopped")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Resume" }));
    expect(sent(h).at(-1)).toEqual({ type: "message.promote", thread: "thr_1", message: "q1" });
    await userEvent.click(screen.getByRole("button", { name: "Resume thread" }));
    expect(sent(h)).toHaveLength(2);
    // Three show; more wait behind "Show all".
    expect(screen.queryByRole("button", { name: /Show all/ })).toBeNull();
  });

  it("moves by one step: up in front of the one above, down in front of the one after next", () => {
    const list = [q("a", "a"), q("b", "b"), q("c", "c")];
    expect(stepTarget(list, "b", -1)?.id).toBe("a");
    expect(stepTarget(list, "a", 1)?.id).toBe("c");
    expect(stepTarget(list, "b", 1)).toBeNull();
    expect(stepTarget(list, "a", -1)).toBeUndefined();
    expect(stepTarget(list, "c", 1)).toBeUndefined();
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

  it("Compact and send: back to a big conversation whose cache expired, the message waits behind a compaction (T3's on return)", async () => {
    const hourAgo = Date.now() - 61 * 60_000;
    const big = thread({ provider: "claude", run_count: 1, context: { used_tokens: 312_000, max_tokens: 1_000_000, total_processed_tokens: null, cache_ttl_ms: 3_600_000 } });
    const h = agentsHarness({ machines: [m], entities: [mirrored("thread", big, 1), mirrored("run", run({ status: "completed", completed_at: hourAgo }), 2)] });
    renderAgents(<Composer mode={{ kind: "thread", thread: big, machine: m }} />, h);
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "Now rotate prod too");
    const button = screen.getByRole("button", { name: "Compact and send" });
    expect(button).toHaveAttribute("title", "Summarize 312k tokens of history, then send");
    await userEvent.click(button);
    expect(sent(h).slice(-2)).toEqual([
      { type: "message.send", thread: "thr_1", text: "/compact" },
      { type: "message.send", thread: "thr_1", text: "Now rotate prod too", mode: "queue" },
    ]);
  });

  it("an idle compaction the daemon started has no message of the person's, and says why it ran", () => {
    const idle = run({ id: "run_2", ordinal: 2, prompt: "/compact", idle_compaction: true, status: "completed" });
    const h = agentsHarness({ machines: [m], entities: [mirrored("thread", thread({ run_count: 2 }), 1), mirrored("run", idle, 2), mirrored("node", node({ type: "compaction", before_tokens: 312_000, after_tokens: 48_000, error: null }, { run: "run_2", status: "completed" }), 3)], follow: ["thr_1"] });
    renderAgents(<Conversation thread={thread({ run_count: 2 })} machine={m} />, h);
    expect(screen.getByRole("separator", { name: "Context automatically compacted before cache expiry" })).toHaveTextContent(/312k\s*48k tokens/);
    expect(screen.queryByText("/compact")).toBeNull();
  });

  it("shows running, compacted with the counts, and failed with why", () => {
    const at = (status: "running" | "completed" | "failed", body: object) => node({ type: "compaction", before_tokens: null, after_tokens: null, error: null, ...body }, { status });
    const { rerender } = renderAgents(<CompactionRow node={at("running", {})} />, agentsHarness());
    expect(screen.getByRole("separator", { name: "Compacting context" })).toBeInTheDocument();
    rerender(<CompactionRow node={at("completed", { before_tokens: 182_000, after_tokens: 41_000 })} />);
    expect(screen.getByRole("separator", { name: "Context compacted" })).toHaveTextContent(/Context compacted\s*182k\s*41k tokens/);
    rerender(<CompactionRow node={at("failed", { error: "Not enough messages to compact." })} />);
    expect(screen.getByRole("separator", { name: "Context compaction failed" })).toHaveTextContent("Not enough messages to compact.");
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

  it("Save to workspace writes the plan into the thread's folder, named after its title", async () => {
    const written = (c: Command) => (c.type === "fs.write" ? ({ type: "written", path: c.path } as const) : reply(c));
    const h = agentsHarness({ machines: [m], entities, follow: ["thr_1"], reply: written });
    renderAgents(<Conversation thread={inPlan} machine={m} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Plan actions" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Save to workspace" }));
    const path = screen.getByRole("textbox", { name: /Workspace path/ });
    expect(path).toHaveValue("export-invoices-as-csv.md");
    // Typing replaces the name and keeps `.md` (T3 selects the name).
    await userEvent.keyboard("plans/csv");
    expect(path).toHaveValue("plans/csv.md");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(sent(h).at(-1)).toEqual({ type: "fs.write", thread: "thr_1", path: "plans/csv.md", text: `${plan}\n` });
    expect(await screen.findByText(/Plan saved to workspace/)).toHaveTextContent("plans/csv.md");
  });

  it("Save to workspace says why it couldn't, and an empty path never goes out", async () => {
    const refuse = (c: Command) => (c.type === "fs.write" ? ({ type: "error", code: "invalid", message: "the path leaves the workspace" } as const) : reply(c));
    const h = agentsHarness({ machines: [m], entities, follow: ["thr_1"], reply: refuse });
    renderAgents(<Conversation thread={inPlan} machine={m} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Plan actions" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Save to workspace" }));
    const path = screen.getByRole("textbox", { name: /Workspace path/ });
    await userEvent.clear(path);
    await userEvent.keyboard("{Enter}");
    expect(screen.getByRole("alert")).toHaveTextContent("Enter a workspace path");
    expect(sent(h).some((c) => c.type === "fs.write")).toBe(false);
    await userEvent.type(path, "../out.md{Enter}");
    expect(await screen.findByRole("alert")).toHaveTextContent("Could not save plan: the path leaves the workspace");
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("form", { name: "Save plan to workspace" })).toBeNull();
  });

  it("Implement in a new thread starts one in build mode with the plan and opens it", async () => {
    const h = agentsHarness({ machines: [m], entities, follow: ["thr_1"], reply });
    renderAgents(<Conversation thread={inPlan} machine={m} />, h);
    await userEvent.click(screen.getByRole("button", { name: "More ways to implement" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Implement in a new thread" }));
    await vi.waitFor(() => expect(h.openThread).toHaveBeenCalledWith("thr_2"));
    expect(sent(h).at(-1)).toMatchObject({ type: "thread.create", project: "prj_1", provider: "claude", interaction_mode: "default", title: "Implement Export invoices as CSV", prompt: `${PLAN_IMPLEMENTATION_PREFIX}${plan}` });
  });

  it("a plan Claude asks to carry out: Implement accepts it, Refine declines with the words; the transcript points below", async () => {
    const asking: Thread = { ...inPlan, status: "waiting", active_run: "run_1", pending_requests: 1 };
    const h = agentsHarness({
      machines: [m],
      entities: [
        ...entities.slice(0, 2).map((e) => (e.entity.kind === "thread" ? mirrored("thread", asking, 2) : e)),
        mirrored("run", run({ status: "waiting" }), 3),
        entities[3],
        mirrored("node", node({ type: "request", request: "req_1" }, { id: "nod_r", ordinal: 2, status: "waiting" }), 5),
        mirrored("request", approval({ node: "nod_r", body: { type: "approval", action: "plan", title: "Carry out the plan", command: null, cwd: null, reason: null, tool: null, files: [], input: null, decisions: ["accept", "decline", "cancel"] } }), 6),
      ],
      follow: ["thr_1"],
    });
    renderAgents(<Conversation thread={asking} machine={m} />, h);
    expect(screen.getByText("· implement or refine it below")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Implement" }));
    expect(sent(h).at(-1)).toEqual({ type: "request.respond", request: "req_1", response: { type: "decision", decision: "accept" } });
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "Use a stream");
    await userEvent.click(screen.getByRole("button", { name: "Refine" }));
    expect(sent(h).at(-1)).toEqual({ type: "request.respond", request: "req_1", response: { type: "decision", decision: "decline", message: "Use a stream" } });
  });

  it("/plan and /default switch the mode without sending anything", async () => {
    const idle = thread();
    const h = agentsHarness({ machines: [m], entities: [mirrored("thread", idle, 1)] });
    renderAgents(<Composer mode={{ kind: "thread", thread: idle, machine: m }} />, h);
    const box = screen.getByRole("textbox", { name: "Message" });
    await userEvent.type(box, "/pl{Enter}");
    expect(sent(h).at(-1)).toEqual({ type: "thread.configure", thread: "thr_1", interaction_mode: "plan" });
    expect(box).toHaveValue("");
    expect(screen.getByRole("button", { name: "Plan mode" })).toHaveAttribute("aria-pressed", "true");
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

  it("Escape goes back from a subagent to the conversation", async () => {
    const h = agentsHarness({ machines: [m], entities, follow: ["thr_1"], ctx: { now: 46_000 } });
    renderAgents(<Conversation thread={busy} machine={m} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Open Find the callers of nod_a" }));
    act(() => void window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" })));
    expect(screen.queryByRole("region", { name: "Subagent Explore" })).toBeNull();
  });

  it("a delegated thread says who delegated it and opens the parent", async () => {
    const parent = thread({ id: "thr_p", title: "Ship the release" });
    const child = thread({ id: "thr_c", title: "Write the docs", lineage: { parent: "thr_p", relation: "subagent", root: "thr_p", from_run: null } });
    const h = agentsHarness({ machines: [m], entities: [mirrored("thread", parent, 1), mirrored("thread", child, 2)], follow: ["thr_c"] });
    renderAgents(<Conversation thread={child} machine={m} />, h);
    expect(screen.getByText("Delegated by")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /Open parent/ }));
    expect(h.openThread).toHaveBeenCalledWith("thr_p");
  });
});
