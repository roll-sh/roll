import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { Composer } from "./composer";
import { approval, machine, mirrored, project, provider, question, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";

const commands = (h: ReturnType<typeof agentsHarness>) => h.command.mock.calls.map((c) => c[2]);

describe("Composer", () => {
  it("sends with Enter, breaks lines with Shift+Enter", async () => {
    const h = agentsHarness();
    renderAgents(<Composer mode={{ kind: "thread", thread: thread(), machine: machine() }} />, h);
    const box = screen.getByRole("textbox", { name: "Message" });
    await userEvent.type(box, "first line{Shift>}{Enter}{/Shift}second");
    expect(h.command).not.toHaveBeenCalled();
    await userEvent.type(box, "{Enter}");
    await waitFor(() => expect(commands(h)).toEqual([{ type: "message.send", thread: "thr_1", text: "first line\nsecond" }]));
    expect(box).toHaveValue("");
  });

  it("steers a running agent (T3's default follow-up); ⌘/Ctrl queues it, and the button says which", async () => {
    const h = agentsHarness();
    renderAgents(<Composer mode={{ kind: "thread", thread: thread({ status: "running", active_run: "run_1" }), machine: machine() }} />, h);
    // There's no queue/steer chooser: the send button is the whole control.
    expect(screen.queryByRole("radio", { name: "steer" })).toBeNull();
    const box = screen.getByRole("textbox", { name: "Message" });
    await userEvent.type(box, "only Postgres");
    expect(screen.getByRole("button", { name: "Steer message" })).toHaveAttribute("title", expect.stringMatching(/to queue$/));
    await userEvent.type(box, "{Enter}");
    await waitFor(() => expect(commands(h).at(-1)).toMatchObject({ type: "message.send", text: "only Postgres", mode: "steer" }));
    await userEvent.type(box, "then run clippy");
    fireEvent.keyDown(window, { key: "Control", ctrlKey: true });
    expect(screen.getByRole("button", { name: "Queue message" })).toBeInTheDocument();
    fireEvent.keyUp(window, { key: "Control", ctrlKey: false });
    fireEvent.keyDown(box, { key: "Enter", ctrlKey: true });
    await waitFor(() => expect(commands(h).at(-1)).toMatchObject({ type: "message.send", text: "then run clippy", mode: "queue" }));
    await userEvent.type(box, "also docs");
    fireEvent.click(screen.getByRole("button", { name: "Steer message" }), { metaKey: true });
    await waitFor(() => expect(commands(h).at(-1)).toMatchObject({ text: "also docs", mode: "queue" }));
  });

  it("on a touch screen, holding the send button queues instead of steering (no ⌘ there); a tap steers", async () => {
    const h = agentsHarness();
    renderAgents(<Composer mode={{ kind: "thread", thread: thread({ status: "running", active_run: "run_1" }), machine: machine() }} />, h);
    const box = screen.getByRole("textbox", { name: "Message" });
    await userEvent.type(box, "then run clippy");
    const send = screen.getByRole("button", { name: "Steer message" });
    expect(send).toHaveAttribute("title", expect.stringMatching(/or hold to queue$/));
    vi.useFakeTimers();
    try {
      fireEvent.pointerDown(send, { pointerType: "touch" });
      vi.advanceTimersByTime(500);
      fireEvent.pointerUp(send, { pointerType: "touch" });
      fireEvent.click(send);
    } finally {
      vi.useRealTimers();
    }
    await waitFor(() => expect(commands(h)).toEqual([expect.objectContaining({ type: "message.send", text: "then run clippy", mode: "queue" })]));
    // A short tap is a click: it steers.
    await userEvent.type(box, "only Postgres");
    fireEvent.pointerDown(screen.getByRole("button", { name: "Steer message" }), { pointerType: "touch" });
    fireEvent.pointerUp(screen.getByRole("button", { name: "Steer message" }), { pointerType: "touch" });
    fireEvent.click(screen.getByRole("button", { name: "Steer message" }));
    await waitFor(() => expect(commands(h).at(-1)).toMatchObject({ text: "only Postgres", mode: "steer" }));
    expect(commands(h)).toHaveLength(2);
  });

  it("redirects (steers) while an approval waits, even with ⌘/Ctrl", async () => {
    const t = thread({ status: "waiting", active_run: "run_1", pending_requests: 1 });
    const h = agentsHarness({ entities: [mirrored("thread", t, 1), mirrored("request", approval(), 2)] });
    renderAgents(<Composer mode={{ kind: "thread", thread: t, machine: machine() }} />, h);
    const box = screen.getByRole("textbox", { name: "Message" });
    expect(box).toHaveAttribute("placeholder", "Send a message to redirect the agent");
    await userEvent.type(box, "use the fake runner");
    fireEvent.keyDown(box, { key: "Enter", ctrlKey: true });
    await waitFor(() => expect(commands(h).at(-1)).toMatchObject({ text: "use the fake runner", mode: "steer" }));
  });

  it("answers the agent's questions in T3's panel: 1–9 pick, a choice moves on, the typed text is your own answer", async () => {
    const t = thread({ status: "waiting", active_run: "run_1", pending_requests: 1 });
    const h = agentsHarness({ entities: [mirrored("thread", t, 1), mirrored("request", question(), 2)] });
    renderAgents(<Composer mode={{ kind: "thread", thread: t, machine: machine() }} />, h);
    const panel = screen.getByRole("group", { name: "Questions from the agent" });
    expect(within(panel).getByText("Which database should the test use?")).toBeInTheDocument();
    expect(within(panel).getByText("1/2")).toBeInTheDocument();
    const box = screen.getByRole("textbox", { name: "Message" });
    expect(box).toHaveAttribute("placeholder", "Type your own answer, or pick one above");
    // "2" picks Postgres from the empty composer; one choice moves on after 200 ms.
    fireEvent.keyDown(box, { key: "2" });
    expect(within(panel).getByRole("radio", { name: /Postgres/ })).toHaveAttribute("aria-checked", "true");
    expect(await within(panel).findByText("What should run after the fix?")).toBeInTheDocument();
    await userEvent.click(within(panel).getByRole("checkbox", { name: /Clippy/ }));
    await userEvent.type(box, "fmt");
    await userEvent.click(screen.getByRole("button", { name: "Submit answers" }));
    await waitFor(() =>
      expect(commands(h).at(-1)).toEqual({ type: "request.respond", request: "req_2", response: { type: "answers", answers: { db: "Postgres", checks: "Clippy, fmt" } } }),
    );
    // Answered as a response, never sent as a message.
    expect(commands(h).some((c) => c.type === "message.send")).toBe(false);
  });

  it("stops a running thread from an empty composer", async () => {
    const h = agentsHarness();
    renderAgents(<Composer mode={{ kind: "thread", thread: thread({ status: "running" }), machine: machine() }} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Stop generation" }));
    await waitFor(() => expect(commands(h)).toEqual([{ type: "run.interrupt", thread: "thr_1" }]));
  });

  it("reconfigures the thread when the model or plan mode changes", async () => {
    const h = agentsHarness();
    renderAgents(<Composer mode={{ kind: "thread", thread: thread(), machine: machine() }} />, h);
    await userEvent.click(screen.getByRole("button", { name: /^Model: Opus 5.5/ }));
    await userEvent.click(await screen.findByRole("option", { name: /Sonnet 5/ }));
    // A model without effort levels closes the picker.
    await waitFor(() => expect(screen.queryByRole("option", { name: /Sonnet 5/ })).toBeNull());
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "{Shift>}{Tab}{/Shift}");
    await waitFor(() =>
      expect(commands(h)).toEqual([
        { type: "thread.configure", thread: "thr_1", model: "claude-sonnet-5" },
        { type: "thread.configure", thread: "thr_1", interaction_mode: "plan" },
      ]),
    );
  });

  it("starts a thread with the machine's providers, models and a worktree", async () => {
    const h = agentsHarness();
    const onSubmit = vi.fn(async () => {});
    const m = machine({ info: { ...machine().info!, providers: [provider({ id: "codex", name: "Codex", auth: "missing" }), provider()] } });
    renderAgents(<Composer mode={{ kind: "new", project: project(), machine: m }} onSubmit={onSubmit} />, h);
    // The logged-in provider is picked, not the first one.
    await userEvent.click(screen.getByRole("button", { name: /^Model: Opus 5.5/ }));
    await userEvent.click(await screen.findByRole("radio", { name: "Max" }));
    expect(screen.getByRole("button", { name: "Model: Opus 5.5 · Max" })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("switch", { name: "Worktree" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "Add a --json flag{Enter}");
    await waitFor(() =>
      expect(onSubmit).toHaveBeenCalledWith({
        provider: "claude",
        model: "claude-opus-5-5",
        effort: "max",
        // New threads have full access unless their project says otherwise.
        runtime_mode: "full_access",
        interaction_mode: "default",
        worktree: true,
        prompt: "Add a --json flag",
      }),
    );
    expect(h.command).not.toHaveBeenCalled();
  });

  it("won't start a thread on a provider that isn't logged in", async () => {
    const h = agentsHarness();
    const m = machine({ info: { ...machine().info!, providers: [provider({ auth: "missing" })] } });
    renderAgents(<Composer mode={{ kind: "new", project: project(), machine: m }} onSubmit={async () => {}} />, h);
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "hi");
    expect(screen.getByRole("button", { name: "Start thread" })).toBeDisabled();
    expect(screen.getByRole("alert")).toHaveTextContent("isn't logged in");
  });

  it("keeps a draft per thread", async () => {
    const h = agentsHarness({ entities: [mirrored("thread", thread(), 1)] });
    const { unmount } = renderAgents(<Composer mode={{ kind: "thread", thread: thread(), machine: machine() }} />, h);
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "half a thought");
    await new Promise((r) => setTimeout(r, 350));
    unmount();
    renderAgents(<Composer mode={{ kind: "thread", thread: thread(), machine: machine() }} />, h);
    expect(screen.getByRole("textbox", { name: "Message" })).toHaveValue("half a thought");
  });

  it("stays open on an asleep sandbox (sending wakes it), not on an offline computer", async () => {
    const h = agentsHarness();
    const asleep = machine({ online: false, sandbox: { backend: "agent37", size: "standard", status: "stopped", hourly_cents: 3, stopped_reason: "idle" } });
    const { unmount } = renderAgents(<Composer mode={{ kind: "thread", thread: thread({ status: "waiting", pending_requests: 1 }), machine: asleep }} />, h);
    const box = screen.getByRole("textbox", { name: "Message" });
    expect(box).toBeEnabled();
    expect(box).toHaveAttribute("placeholder", "The sandbox is asleep; sending wakes it");
    await userEvent.type(box, "go on{Enter}");
    await waitFor(() => expect(commands(h)).toEqual([expect.objectContaining({ type: "message.send", thread: "thr_1", text: "go on" })]));
    unmount();
    renderAgents(<Composer mode={{ kind: "thread", thread: thread(), machine: machine({ online: false }) }} />, agentsHarness());
    expect(screen.getByRole("textbox", { name: "Message" })).toBeDisabled();
    expect(screen.getByRole("textbox", { name: "Message" })).toHaveAttribute("placeholder", "The machine is offline");
  });
});
