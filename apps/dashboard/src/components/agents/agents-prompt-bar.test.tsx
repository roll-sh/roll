import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Command, Reply } from "@/lib/agents-api";
import { setActiveClient } from "@/lib/agents-client";
import { agentsPalette } from "@/lib/agents-palette";
import { resetAgentsSurface, showAgents } from "@/lib/agents-surface";
import { machine, mirrored, project, provider, thread, T0 } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, type AgentsHarness } from "@/test/agents-render";
import { MarkStateProvider } from "../mark-state";
import { AgentsControl } from "./agents-control";
import { goalDraftKey } from "./orchestrate";
import { PromptBar } from "./prompt-bar";

let conn: { client: AgentsHarness["client"] | null; services: ReturnType<typeof fakeServices> | null; error: string | null } = { client: null, services: null, error: null };
vi.mock("./use-connection", () => ({ useAgentsConnection: () => conn, useDemoParam: () => false }));

const codex = provider({ id: "codex", name: "Codex", models: [{ id: "gpt-5.5-codex", name: "GPT-5.5 Codex", default: true, options: [] }] });
const box = machine({ id: "mch_2", name: "build-box", info: { ...machine().info!, hostname: "build-box", os: "linux", providers: [codex] } });
const roll = project();
const infra = project({ id: "prj_2", name: "infra", path: "/home/roll/infra" });
const web = project({ id: "prj_3", name: "web", path: "/Users/ada/web" });

const creates = (h: AgentsHarness) => h.command.mock.calls.filter((c) => c[2].type === "thread.create");
const goal = () => screen.getByRole("textbox", { name: /A goal for agents|A prompt for a new agent/ });
const chip = () => screen.getByRole("combobox", { name: "Project" });

/** Two machines, a project on each and one more on the laptop; `reply` answers commands. */
function harness(reply?: (c: Command, h: AgentsHarness) => Reply | Promise<Reply>) {
  let h: AgentsHarness | null = null;
  h = agentsHarness({
    machines: [machine(), box],
    entities: [mirrored("project", roll, 1), mirrored("project", infra, 2, "mch_2"), mirrored("project", web, 3), mirrored("thread", thread({ updated_at: T0 + 10 }), 4)],
  });
  const answer = reply ?? created;
  h.command.mockImplementation(async (_m, _id, c) => answer(c, h!));
  return h;
}

/** The daemon: the reply, then the thread's record. */
function created(c: Command, h: AgentsHarness): Reply {
  if (c.type !== "thread.create") return { type: "ok" };
  setTimeout(() => h.client.enqueue({ type: "entity", ...mirrored("thread", thread({ id: "thr_9", project: c.project, title: c.prompt ?? "" }), 9) }), 0);
  return { type: "created", id: "thr_9" };
}

beforeEach(() => {
  localStorage.clear();
  resetAgentsSurface();
});
afterEach(() => {
  setActiveClient(null);
  conn = { client: null, services: null, error: null };
});

describe("the prompt bar", () => {
  it("keeps where a draft goes while other threads update (it used to follow the newest thread)", async () => {
    const h = harness();
    renderAgents(<PromptBar />, h);
    await userEvent.type(goal(), "Add dark mode");
    expect(chip()).toHaveTextContent("roll on ada-mbp");
    const model = screen.getByRole("button", { name: /^Model: Opus 5.5/ });
    // Work happens in another project, on another machine: the draft stays put.
    act(() => h.client.enqueue({ type: "entity", ...mirrored("thread", thread({ id: "thr_2", project: "prj_2", provider: "codex", updated_at: T0 + 99_000 }), 10, "mch_2") }));
    expect(chip()).toHaveTextContent("roll on ada-mbp");
    expect(model).toHaveAccessibleName(/^Model: Opus 5.5/);
    await userEvent.keyboard("{Enter}");
    await waitFor(() => expect(creates(h)).toHaveLength(1));
    expect(creates(h)[0][0]).toBe("mch_1");
    expect(creates(h)[0][2]).toMatchObject({ project: "prj_1", provider: "claude", orchestrator: true, prompt: "Add dark mode" });
  });

  it("moves only when the chip picks another project, and starts it there once (no palette hand-off)", async () => {
    const h = harness();
    renderAgents(<PromptBar />, h);
    await userEvent.type(goal(), "Rotate the keys");
    await userEvent.click(chip());
    await userEvent.click(await screen.findByRole("option", { name: "infra on build-box" }));
    expect(chip()).toHaveTextContent("infra on build-box");
    // The model follows the machine: build-box has Codex only.
    expect(screen.getByRole("button", { name: /^Model: GPT-5.5 Codex/ })).toBeInTheDocument();
    expect(agentsPalette.get().open).toBe(false);
    await userEvent.click(screen.getByRole("button", { name: "Orchestrate" }));
    await waitFor(() => expect(creates(h)).toHaveLength(1));
    expect(creates(h)[0][0]).toBe("mch_2");
    expect(creates(h)[0][2]).toMatchObject({ project: "prj_2", provider: "codex" });
    await waitFor(() => expect(h.openThread).toHaveBeenCalledWith("thr_9"));
    expect(goal()).toHaveValue("");
  });

  it("keeps the draft per owner across unmounting (a conversation opening, a reload), and the kind and model after a start", async () => {
    const h = harness();
    const first = renderAgents(<PromptBar />, h);
    await userEvent.type(goal(), "Half a thought");
    await userEvent.click(screen.getByRole("radio", { name: "One agent" }));
    await waitFor(() => expect(localStorage.getItem(goalDraftKey("acme"))).toContain("Half a thought"));
    first.unmount();
    renderAgents(<PromptBar />, h);
    await waitFor(() => expect(goal()).toHaveValue("Half a thought"));
    expect(screen.getByRole("radio", { name: "One agent" })).toHaveAttribute("aria-checked", "true");
    await userEvent.type(goal(), "{Enter}");
    await waitFor(() => expect(creates(h)).toHaveLength(1));
    expect(creates(h)[0][2]).not.toHaveProperty("orchestrator");
    await waitFor(() => expect(goal()).toHaveValue(""));
    // The next goal is "One agent" again (T3's sticky choices); another owner's draft is its own.
    await userEvent.click(goal());
    expect(screen.getByRole("radio", { name: "One agent" })).toHaveAttribute("aria-checked", "true");
    expect(localStorage.getItem(goalDraftKey("other"))).toBeNull();
  });

  it("shows a refusal whole, and sends again under a new id; after a timeout, under the same one", async () => {
    let answer: Reply = { type: "error", code: "provider", message: "Claude Code is logged out on ada-mbp. Log in with claude login in a terminal on the machine, then try again." };
    const h = harness((c, hh) => (c.type === "thread.create" ? answer : created(c, hh)));
    renderAgents(<PromptBar />, h);
    await userEvent.type(goal(), "Ship it{Enter}");
    expect(await screen.findByRole("alert")).toHaveTextContent("Log in with claude login in a terminal on the machine, then try again.");
    expect(goal()).toHaveValue("Ship it");
    answer = { type: "error", code: "timeout", message: "the machine didn't answer within 30 s" };
    await userEvent.click(screen.getByRole("button", { name: "Orchestrate" }));
    await waitFor(() => expect(creates(h)).toHaveLength(2));
    const ids = () => creates(h).map((c) => c[1]);
    expect(ids()[1]).not.toBe(ids()[0]);
    await screen.findByText(/didn't answer/);
    answer = { type: "created", id: "thr_9" };
    await userEvent.click(screen.getByRole("button", { name: "Orchestrate" }));
    await waitFor(() => expect(creates(h)).toHaveLength(3));
    // It may have started: the same id, so the daemon replays instead of starting a second one.
    expect(ids()[2]).toBe(ids()[1]);
  });

  it("can stop waiting for a start that hangs; the draft stays", async () => {
    let release: (r: Reply) => void = () => {};
    const h = harness((c) => (c.type === "thread.create" ? new Promise<Reply>((r) => (release = r)) : { type: "ok" }));
    renderAgents(<PromptBar />, h);
    await userEvent.type(goal(), "Slow one{Enter}");
    expect(await screen.findByText("Starting…")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByText("Starting…")).toBeNull();
    expect(goal()).toHaveValue("Slow one");
    // A late answer doesn't open anything or clear what's typed.
    await act(async () => release({ type: "created", id: "thr_9" }));
    expect(h.openThread).not.toHaveBeenCalled();
    expect(goal()).toHaveValue("Slow one");
  });

  it("works from the keyboard: Shift+Enter is a new line, arrows switch who does it, IME Enter doesn't send", async () => {
    const h = harness();
    renderAgents(<PromptBar />, h);
    await userEvent.type(goal(), "line one{Shift>}{Enter}{/Shift}line two");
    expect(goal()).toHaveValue("line one\nline two");
    fireEvent.keyDown(goal(), { key: "Enter", isComposing: true });
    expect(creates(h)).toHaveLength(0);
    const group = screen.getByRole("radiogroup", { name: "Who does it" });
    const orchestrate = within(group).getByRole("radio", { name: "Orchestrate" });
    expect(orchestrate).toHaveAttribute("tabindex", "0");
    expect(within(group).getByRole("radio", { name: "One agent" })).toHaveAttribute("tabindex", "-1");
    orchestrate.focus();
    await userEvent.keyboard("{ArrowRight}");
    expect(within(group).getByRole("radio", { name: "One agent" })).toHaveAttribute("aria-checked", "true");
    expect(document.activeElement).toBe(within(group).getByRole("radio", { name: "One agent" }));
  });

  it("says when the pinned project's machine went offline instead of moving the draft elsewhere", async () => {
    const h = harness();
    renderAgents(<PromptBar />, h);
    await userEvent.type(goal(), "Nightly cleanup");
    act(() => h.client.enqueue({ type: "machine", cursor: 20, machine: { ...machine(), online: false } }));
    expect(chip()).toHaveTextContent("roll on ada-mbp");
    await userEvent.keyboard("{Enter}");
    expect(await screen.findByRole("alert")).toHaveTextContent("ada-mbp is offline. Start it with roll agent up, or pick another project.");
    expect(creates(h)).toHaveLength(0);
  });
});

describe("the prompt bar's open and close", () => {
  const isOpen = () => screen.getByRole("form", { name: "Ask agents" }).hasAttribute("data-open");
  const withMachines = () => {
    const h = harness();
    // Codex too, so the picker's rail has another provider to click.
    act(() => h.client.enqueue({ type: "machine", cursor: 30, machine: machine({ info: { ...machine().info!, providers: [provider(), codex] } }) }));
    return h;
  };

  it("stays open while its picker is used where a click doesn't focus a button (Safari, Firefox on macOS)", async () => {
    renderAgents(
      <>
        <PromptBar />
        <div data-testid="canvas" />
      </>,
      withMachines(),
    );
    await userEvent.click(goal());
    await userEvent.click(screen.getByRole("button", { name: /^Model:/ }));
    const search = await screen.findByRole("combobox", { name: "Search models" });
    const rail = screen.getByRole("toolbar", { name: "Providers" });
    const codexTab = within(rail).getByRole("button", { name: /Codex/ });
    // Safari: the search blurs to nothing (the button it's going to doesn't take focus), and the
    // press and click reach the button without focusing it.
    fireEvent.focusOut(search, { relatedTarget: null });
    expect(isOpen()).toBe(true);
    fireEvent.pointerDown(codexTab);
    expect(isOpen()).toBe(true);
    fireEvent.mouseDown(codexTab);
    fireEvent.click(codexTab);
    expect(isOpen()).toBe(true);
    expect(screen.getByRole("toolbar", { name: "Providers" })).toBeInTheDocument();
    expect(within(screen.getByRole("toolbar", { name: "Providers" })).getByRole("button", { name: /Codex/ })).toHaveAttribute("aria-pressed", "true");
    // A press on the picker's own background is the picker's: the text doesn't take focus from it.
    const before = document.activeElement;
    fireEvent.mouseDown(rail);
    expect(document.activeElement).toBe(before);
  });

  it("closes on a press outside it (the canvas), on Escape, and when focus moves to a real element elsewhere; a blur alone never closes it", async () => {
    renderAgents(
      <>
        <PromptBar />
        <div data-testid="canvas" />
        <button type="button">Elsewhere</button>
      </>,
      harness(),
    );
    await userEvent.click(goal());
    expect(isOpen()).toBe(true);
    fireEvent.focusOut(goal(), { relatedTarget: null });
    expect(isOpen()).toBe(true);
    fireEvent.pointerDown(screen.getByTestId("canvas"));
    expect(isOpen()).toBe(false);

    await userEvent.click(goal());
    await userEvent.keyboard("{Escape}");
    expect(isOpen()).toBe(false);

    await userEvent.click(goal());
    act(() => screen.getByRole("button", { name: "Elsewhere" }).focus());
    expect(isOpen()).toBe(false);
    // Something typed keeps it open wherever focus goes.
    await userEvent.type(goal(), "half a goal");
    fireEvent.pointerDown(screen.getByTestId("canvas"));
    expect(isOpen()).toBe(true);
  });

  it("a press on the bar itself (not a control) focuses the text once", async () => {
    renderAgents(<PromptBar />, harness());
    const form = screen.getByRole("form", { name: "Ask agents" });
    const focused = vi.fn();
    goal().addEventListener("focus", focused);
    fireEvent.mouseDown(form);
    expect(document.activeElement).toBe(goal());
    expect(focused).toHaveBeenCalledTimes(1);
    expect(isOpen()).toBe(true);
  });
});

describe("the prompt bar on the page", () => {
  it("stays, with its draft, while a conversation is open beside the canvas", async () => {
    const h = harness();
    conn = { client: h.client, services: fakeServices(), error: null };
    renderAgents(
      <MarkStateProvider>
        <AgentsControl />
      </MarkStateProvider>,
      h,
    );
    act(() => showAgents({ kind: "canvas" }));
    await userEvent.type(goal(), "Keep me");
    act(() => showAgents({ kind: "thread", thread: "thr_1" }));
    expect(goal()).toHaveValue("Keep me");
  });
});
