import { fireEvent, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { Composer } from "./composer";
import { machine, mirrored, project, provider, thread } from "@/test/agents";
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

  it("queues while a run is going, steers on request, and ⌘/Ctrl+Enter flips it once", async () => {
    const h = agentsHarness();
    renderAgents(<Composer mode={{ kind: "thread", thread: thread({ status: "running", active_run: "run_1" }), machine: machine() }} />, h);
    const box = screen.getByRole("textbox", { name: "Message" });
    await userEvent.type(box, "then run clippy{Enter}");
    await waitFor(() => expect(commands(h).at(-1)).toMatchObject({ type: "message.send", mode: "queue" }));
    await userEvent.type(box, "only Postgres");
    fireEvent.keyDown(box, { key: "Enter", ctrlKey: true });
    await waitFor(() => expect(commands(h).at(-1)).toMatchObject({ type: "message.send", text: "only Postgres", mode: "steer" }));
    await userEvent.click(screen.getByRole("radio", { name: "steer" }));
    await userEvent.type(box, "also docs{Enter}");
    await waitFor(() => expect(commands(h).at(-1)).toMatchObject({ text: "also docs", mode: "steer" }));
  });

  it("interrupts a running thread from an empty composer", async () => {
    const h = agentsHarness();
    renderAgents(<Composer mode={{ kind: "thread", thread: thread({ status: "running" }), machine: machine() }} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Interrupt" }));
    await waitFor(() => expect(commands(h)).toEqual([{ type: "run.interrupt", thread: "thr_1" }]));
  });

  it("reconfigures the thread when the model or plan mode changes", async () => {
    const h = agentsHarness();
    renderAgents(<Composer mode={{ kind: "thread", thread: thread(), machine: machine() }} />, h);
    await userEvent.selectOptions(screen.getByRole("combobox", { name: "Model" }), "claude-sonnet-5");
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
    expect(screen.getByRole("combobox", { name: "Provider" })).toHaveValue("claude");
    await userEvent.selectOptions(screen.getByRole("combobox", { name: "Effort" }), "max");
    await userEvent.click(screen.getByRole("checkbox", { name: "Worktree" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "Add a --json flag{Enter}");
    await waitFor(() =>
      expect(onSubmit).toHaveBeenCalledWith({
        provider: "claude",
        model: "claude-opus-5-5",
        effort: "max",
        runtime_mode: "auto_edit",
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
});
