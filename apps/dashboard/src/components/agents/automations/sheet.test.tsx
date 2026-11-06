import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
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
  it("goes in and back out a level at a time: list → overview → builder, Back and Escape return the way you came", async () => {
    setup();
    act(() => setAgentsView({ kind: "automations" }));
    await userEvent.click(await screen.findByRole("button", { name: "Morning triage" }));
    expect(view()).toEqual({ kind: "automations", list: undefined, open: "aut_1" });
    const steps = await screen.findByRole("list", { name: "Steps" });
    expect(within(steps).getByText("Notify me")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Edit" }));
    expect(view()).toMatchObject({ open: "aut_1", tab: "edit" });
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("Morning triage");
    // Build and Settings are one level: a tab, not a page deeper.
    await userEvent.click(within(screen.getByRole("group", { name: "Show" })).getByRole("button", { name: "Settings" }));
    expect(view()).toMatchObject({ tab: "settings" });

    await userEvent.click(screen.getByRole("button", { name: "Back to overview (Esc)" }));
    expect(view()).toEqual({ kind: "automations", list: undefined, open: "aut_1" });
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

});
