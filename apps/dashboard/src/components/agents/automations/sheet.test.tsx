import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setActiveClient } from "@/lib/agents-client";
import { agentsSurface, resetAgentsSurface, setAgentsView } from "@/lib/agents-surface";
import { mirrored, project, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { automation, automationRun, fakeApi } from "@/test/automations";
import { AgentsSurface } from "../agents-page";
import { forgetDrafts } from "./automation-panel";

const other = thread({ id: "thr_2", title: "Upgrade to Next 16.4" });

function setup() {
  const api = fakeApi([automation(), automation({ id: "aut_2", name: "Fix a broken main", trigger: { type: "code", events: ["work"], repos: ["acme/roll"] } })], [automationRun()]);
  const h = agentsHarness({ entities: [mirrored("project", project(), 1), mirrored("thread", thread(), 2), mirrored("thread", other, 3)], follow: ["thr_1"] });
  renderAgents(<AgentsSurface client={h.client} services={fakeServices({ automations: api })} demo={false} chrome="popup" />, h);
  return { h, api };
}

const sheet = () => screen.queryByRole("complementary", { name: "Automations" });
const card = (key: string) => document.querySelector<HTMLElement>(`[data-card][data-key="${key}"]`);
const view = () => agentsSurface.get().view;

beforeEach(() => {
  localStorage.clear();
  resetAgentsSurface();
});
afterEach(() => {
  setActiveClient(null);
  forgetDrafts();
});

describe("the automations sheet", () => {
  it("slides in beside the canvas from ⚡, and the canvas stays live: a card opens its conversation in the one right sheet", async () => {
    setup();
    await userEvent.click(screen.getByRole("button", { name: "Automations (A)" }));
    const aside = sheet()!;
    expect(aside).toHaveClass("panel-in");
    expect(await within(aside).findByRole("button", { name: "Morning triage" })).toBeInTheDocument();
    expect(screen.getByRole("form", { name: "Ask agents" })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Thread Upgrade to Next 16.4" }));
    expect(view()).toEqual({ kind: "thread", thread: "thr_2" });
    expect(sheet()).toBeNull();
    expect(screen.getAllByRole("complementary")).toHaveLength(1);
  });

  it("⚡ closes it and it slides out; A opens and closes it, Escape closes it from the list", async () => {
    setup();
    act(() => setAgentsView({ kind: "automations" }));
    await userEvent.click(screen.getByRole("button", { name: "Close automations (A)" }));
    expect(view()).toEqual({ kind: "canvas" });
    // It slides out, then it's gone.
    expect(sheet()).toHaveAttribute("data-leaving");
    await waitFor(() => expect(sheet()).toBeNull());

    await userEvent.keyboard("a");
    expect(view()).toEqual({ kind: "automations" });
    await userEvent.keyboard("{Escape}");
    expect(view()).toEqual({ kind: "canvas" });
    await userEvent.keyboard("a");
    await userEvent.keyboard("a");
    expect(view()).toEqual({ kind: "canvas" });
  });

  it("goes in and back out a level at a time: list → overview → builder, Back and Escape return the way you came", async () => {
    setup();
    act(() => setAgentsView({ kind: "automations" }));
    await userEvent.click(await screen.findByRole("button", { name: "Morning triage" }));
    expect(view()).toEqual({ kind: "automations", list: undefined, open: "aut_1" });
    const steps = await screen.findByRole("list", { name: "Steps" });
    expect(within(steps).getByText("Notify me")).toBeInTheDocument();
    expect(sheet()!.querySelector("[data-level]")).toHaveClass("stack-push");

    await userEvent.click(screen.getByRole("button", { name: "Edit" }));
    expect(view()).toMatchObject({ open: "aut_1", tab: "edit" });
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("Morning triage");
    // Build and Settings are one level: a tab, not a page deeper.
    await userEvent.click(within(screen.getByRole("group", { name: "Show" })).getByRole("button", { name: "Settings" }));
    expect(view()).toMatchObject({ tab: "settings" });

    await userEvent.click(screen.getByRole("button", { name: "Back to overview (Esc)" }));
    expect(view()).toEqual({ kind: "automations", list: undefined, open: "aut_1" });
    expect(sheet()!.querySelector("[data-level]")).toHaveClass("stack-pop");
    await userEvent.keyboard("{Escape}");
    expect(view()).toEqual({ kind: "automations" });
    await userEvent.keyboard("{Escape}");
    expect(view()).toEqual({ kind: "canvas" });
  });

  it("deep-links a run: its threads are highlighted on the canvas, a thread opens in the sheet with Back to the run", async () => {
    setup();
    act(() => setAgentsView({ kind: "automations", open: "aut_1", tab: "runs", run: "arn_1" }));
    await screen.findByRole("list", { name: "Steps" });
    await waitFor(() => expect(card("t:thr_1")).toHaveAttribute("data-selected"));
    expect(card("t:thr_2")).not.toHaveAttribute("data-selected");

    // The run's thread chip (its log names the thread too, a link of its own).
    await userEvent.click(within(sheet()!).getAllByRole("button", { name: /Fix the flaky queue test/ })[0]);
    expect(view()).toEqual({ kind: "automations", open: "aut_1", tab: "runs", run: "arn_1", thread: "thr_1" });
    // One right sheet: the conversation is inside it.
    expect(screen.getAllByRole("complementary")).toHaveLength(1);
    expect(within(sheet()!).getByRole("region", { name: "Thread Fix the flaky queue test" })).toBeInTheDocument();

    await userEvent.click(within(sheet()!).getByRole("button", { name: "Back to run (Esc)" }));
    expect(view()).toEqual({ kind: "automations", open: "aut_1", tab: "runs", run: "arn_1" });
    // A deep link goes up to the automation, not out.
    await userEvent.click(screen.getByRole("button", { name: "Back to overview (Esc)" }));
    expect(view()).toEqual({ kind: "automations", open: "aut_1" });
  });

  it("a run's highlighted card on the canvas opens its conversation in the sheet; any other card replaces the sheet", async () => {
    setup();
    act(() => setAgentsView({ kind: "automations", open: "aut_1", tab: "runs", run: "arn_1" }));
    await waitFor(() => expect(card("t:thr_1")).toHaveAttribute("data-selected"));
    await userEvent.click(screen.getByRole("button", { name: "Thread Fix the flaky queue test" }));
    expect(view()).toEqual({ kind: "automations", open: "aut_1", tab: "runs", run: "arn_1", thread: "thr_1" });
    expect(screen.getAllByRole("complementary")).toHaveLength(1);
    await userEvent.keyboard("{Escape}");
    expect(view()).toEqual({ kind: "automations", open: "aut_1", tab: "runs", run: "arn_1" });

    await userEvent.click(screen.getByRole("button", { name: "Thread Upgrade to Next 16.4" }));
    expect(view()).toEqual({ kind: "thread", thread: "thr_2" });
  });

  it("on a phone covers the canvas with the same stack, and Back on the list returns to the canvas", async () => {
    const mm = window.matchMedia;
    vi.spyOn(window, "matchMedia").mockImplementation((q: string) => ({ ...mm(q), matches: q.includes("max-width: 640px") }));
    setup();
    await userEvent.click(screen.getByRole("button", { name: "More" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Automations" }));
    const aside = sheet()!;
    expect(aside).toHaveClass("absolute", "inset-0");
    // No Close on a phone: Back is the way out, at every level.
    expect(within(aside).queryByRole("button", { name: "Close automations (Esc)" })).toBeNull();
    await userEvent.click(await within(aside).findByRole("button", { name: "Morning triage" }));
    await userEvent.click(within(sheet()!).getByRole("button", { name: "Back to automations (Esc)" }));
    expect(view()).toEqual({ kind: "automations" });
    await userEvent.click(within(sheet()!).getByRole("button", { name: "Back to the canvas" }));
    expect(view()).toEqual({ kind: "canvas" });
    vi.restoreAllMocks();
  });

  it("maximizes the builder over the canvas and remembers it, only for the builder", async () => {
    setup();
    act(() => setAgentsView({ kind: "automations", open: "aut_1", tab: "edit" }));
    await userEvent.click(await screen.findByRole("button", { name: "Maximize the sheet" }));
    expect(screen.getByRole("form", { name: "Ask agents", hidden: true }).closest(".bg-canvas")).toHaveClass("hidden");
    expect(localStorage.getItem("roll:agents:automations-maximized")).toBe("1");
    await userEvent.click(screen.getByRole("button", { name: "Back to overview (Esc)" }));
    expect(screen.queryByRole("button", { name: /Maximize the sheet|Restore the sheet's size/ })).toBeNull();
    expect(screen.getByRole("form", { name: "Ask agents" }).closest(".bg-canvas")).not.toHaveClass("hidden");
  });
});
