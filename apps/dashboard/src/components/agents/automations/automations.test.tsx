import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AutomationsApi } from "@/lib/automations-api";
import { automation, automationRun, fakeApi, T } from "@/test/automations";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { machine, mirrored, project, provider, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { ThreadNode } from "../nodes";
import { AutomationPanel, forgetDrafts, RunButton } from "./automation-panel";
import { AutomationsView } from "./automations-view";
import { RunDetail } from "./runs";

afterEach(() => {
  resetAgentsSurface();
  forgetDrafts();
});

function harness(api: AutomationsApi) {
  return agentsHarness({
    machines: [machine({ info: { ...machine().info!, providers: [provider()] } })],
    entities: [mirrored("project", project(), 1), mirrored("thread", thread(), 2)],
    ctx: { services: fakeServices({ automations: api }) },
  });
}

describe("Automations list", () => {
  it("shows each automation's trigger, last run and week, and pauses one from its switch", async () => {
    const api = fakeApi([automation(), automation({ id: "aut_2", name: "Fix a broken main", trigger: { type: "code", events: ["work"], repos: ["acme/roll"] } })], [automationRun({ status: "failed" })]);
    renderAgents(<AutomationsView view={{ kind: "automations" }} phone={false} />, harness(api));
    const row = await screen.findByRole("button", { name: "Morning triage" });
    expect(within(row).getByText("Weekdays at 09:00 UTC")).toBeInTheDocument();
    expect(within(row).getByText("Failed")).toBeInTheDocument();
    expect(within(row).getByRole("img", { name: "Last 1 run, 1 failed" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Fix a broken main" })).toHaveTextContent("When a work item changes in acme/roll");

    await userEvent.click(within(row).getByRole("switch", { name: "Pause Morning triage" }));
    expect(api.disable).toHaveBeenCalledWith("acme", "aut_1");
    await waitFor(() => expect(within(screen.getByRole("button", { name: "Morning triage" })).getByText("Paused")).toBeInTheDocument());

    await userEvent.click(row);
    expect(agentsSurface.get().view).toEqual({ kind: "automations", list: undefined, open: "aut_1" });
  });

  it("with none yet, is a gallery of templates that open in the builder", async () => {
    renderAgents(<AutomationsView view={{ kind: "automations" }} phone={false} />, harness(fakeApi()));
    expect(await screen.findByRole("heading", { name: "Let your agents run on their own" })).toBeInTheDocument();
    const gallery = screen.getByRole("list", { name: "Templates" });
    for (const t of ["Run: an agent in a worktree", "Weekdays 9:00: triage new work", "When main breaks: an agent fixes it", "When a thread finishes: publish and land"])
      expect(within(gallery).getByRole("button", { name: `Use the template ${t}` })).toBeInTheDocument();
    await userEvent.click(within(gallery).getByRole("button", { name: "Use the template When main breaks: an agent fixes it" }));
    expect(agentsSurface.get().view).toEqual({ kind: "automations", open: "new", template: "main-breaks" });
  });
});

describe("Automation builder", () => {
  it("builds one from nothing: a step from the catalog, its form, a variable from {{, a test, then Create", async () => {
    const api = fakeApi();
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new" }} />, harness(api));
    await userEvent.type(screen.getByRole("textbox", { name: "Name" }), "Say hi");

    await userEvent.click(screen.getByRole("button", { name: "Add a step" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: /Notify me/ }));
    const card = screen.getByRole("region", { name: "Step 1: Notify me" });
    // The form comes from the step's schema: a template title, a level, a link.
    expect(within(card).getByRole("combobox", { name: "Level" })).toBeInTheDocument();
    const title = within(card).getByRole("combobox", { name: "Title" });

    await userEvent.type(title, "Run {{{{ ru");
    const list = await screen.findByRole("listbox", { name: "Variables" });
    expect(within(list).getAllByRole("option")[0]).toHaveTextContent("run.id");
    await userEvent.keyboard("{Enter}");
    expect(title).toHaveValue("Run {{ run.id }}");
    expect(screen.queryByRole("listbox", { name: "Variables" })).toBeNull();

    await userEvent.click(screen.getByRole("button", { name: "Test" }));
    expect(api.test).toHaveBeenCalledWith("acme", expect.objectContaining({ event: expect.objectContaining({ source: "manual", kind: "run" }) }));
    expect(await screen.findByText(/It would run: 1 of 1 step/)).toBeInTheDocument();
    expect(within(card).getByText("Runs")).toBeInTheDocument();
    // What the template made shows under the field.
    expect(within(card).getByText("Hi there")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(api.create).toHaveBeenCalled());
    const spec = vi.mocked(api.create).mock.calls[0][1];
    expect(spec).toMatchObject({ name: "Say hi", trigger: { type: "manual" }, steps: [{ id: "notify", kind: "notify", config: { title: "Run {{ run.id }}" } }] });
    expect(agentsSurface.get().view).toEqual({ kind: "automations", open: "aut_new" });
  });

  it("says what's wrong inline and doesn't save until it's fixed", async () => {
    const api = fakeApi();
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new" }} />, harness(api));
    await userEvent.click(screen.getByRole("button", { name: "Create" }));
    expect(screen.getByRole("contentinfo")).toHaveTextContent("2 things to fix");
    expect(screen.getByText("Add a step: what should happen.")).toBeInTheDocument();
    expect(api.create).not.toHaveBeenCalled();
  });

});

describe("Automation runs", () => {
  it("lists the notices a person missed, new ones marked, each opening its thread or its run (A12)", async () => {
    localStorage.clear();
    const api = fakeApi([automation()], [automationRun()]);
    const note = (id: string, title: string, at: number) => ({ id, title, level: "success" as const, link: null, automation: "aut_1", run: "arn_1", at });
    api.notices = vi.fn(async () => [note("ntc_2", "Main is green again", T + 2), note("ntc_1", "Triage done", T + 1)]);
    const h = harness(api);
    renderAgents(<AutomationsView view={{ kind: "automations", list: "notices" }} phone={false} />, h);
    const list = await screen.findByRole("list", { name: "Notices" });
    expect(within(list).getAllByRole("listitem").map((li) => li.getAttribute("aria-label"))).toEqual(["New: Main is green again", "New: Triage done"]);
    // Seen now, on this device; the marks stay while the list is open.
    expect(Number(localStorage.getItem("roll:agents:notices-seen:acme"))).toBe(T + 2);
    const first = within(list).getAllByRole("listitem")[0];
    await userEvent.click(within(first).getByRole("button", { name: "Open the thread" }));
    expect(h.openThread).toHaveBeenCalledWith("thr_1");
    await userEvent.click(within(first).getByRole("button", { name: "View the run" }));
    expect(agentsSurface.get().view).toEqual({ kind: "automations", open: "aut_1", tab: "runs", run: "arn_1" });
  });

  it("runs another trigger's automation by hand with the event's data, starting from a sample that meets its conditions (A9)", async () => {
    const api = fakeApi();
    const x = automation({ id: "aut_2", name: "On landing", trigger: { type: "code", events: ["landed"], repos: ["acme/app"] }, filters: [{ field: "event.data.repo", op: "equals", value: "acme/web" }] });
    renderAgents(<RunButton automation={x} manual={false} />, harness(api));
    await userEvent.click(screen.getByRole("button", { name: "Run now" }));
    const box = await screen.findByRole("textbox", { name: "Event data" });
    const sample = JSON.parse((box as HTMLTextAreaElement).value);
    expect(sample.repo).toBe("acme/web");
    // Not JSON: says so and doesn't run.
    await userEvent.clear(box);
    await userEvent.type(box, "main is red");
    await userEvent.click(within(screen.getByRole("dialog", { name: "Run it now" })).getByRole("button", { name: "Run now" }));
    expect(await screen.findByText(/The event's data is JSON/)).toBeInTheDocument();
    expect(api.run).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Start from the sample again" }));
    await userEvent.click(within(screen.getByRole("dialog", { name: "Run it now" })).getByRole("button", { name: "Run now" }));
    expect(api.run).toHaveBeenCalledWith("acme", "aut_2", { input: null, data: expect.objectContaining({ repo: "acme/web" }), key: null });
  });

  it("follows a run live: steps move as automation_run events arrive, its thread opens beside the canvas", async () => {
    const r = automationRun();
    const api = fakeApi([automation()], [r]);
    const h = harness(api);
    renderAgents(<RunDetail run="arn_1" />, h);
    const steps = await screen.findByRole("list", { name: "Steps" });
    expect(within(steps).getByText("Waiting")).toBeInTheDocument();
    // The log names the thread it waits for, a link to it (titled when its record is here).
    expect(within(steps).getByLabelText("Log")).toHaveTextContent(/Waiting for (thr_1|Fix the flaky queue test)/);
    await userEvent.click(within(within(steps).getByLabelText("Log")).getByRole("button", { name: /thr_1|Fix the flaky queue test/ }));
    expect(h.openThread).toHaveBeenLastCalledWith("thr_1");

    act(() =>
      h.client.enqueue({
        type: "automation_run",
        cursor: 9,
        run: { ...r, status: "succeeded", cursor: 9, finished_at: T, steps: [r.steps[0], { ...r.steps[1], status: "succeeded", finished_at: T, output: { status: "done" } }] },
      }),
    );
    await waitFor(() => expect(within(steps).getAllByText("Done")).toHaveLength(2));
    expect(screen.getByText("Succeeded")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Cancel/ })).toBeNull();

    await userEvent.click(screen.getByRole("button", { name: /Fix the flaky queue test/ }));
    expect(h.openThread).toHaveBeenCalledWith("thr_1");
  });

  it("cancels a run that's going", async () => {
    const api = fakeApi([automation()], [automationRun()]);
    renderAgents(<RunDetail run="arn_1" />, harness(api));
    await userEvent.click(await screen.findByRole("button", { name: /Cancel/ }));
    expect(api.cancel).toHaveBeenCalledWith("acme", "arn_1");
    expect(await screen.findByText("Cancelled")).toBeInTheDocument();
  });

});

describe("the canvas card", () => {
  it("tags a thread an automation started, and opens its run", async () => {
    const h = harness(fakeApi([automation()], [automationRun()]));
    renderAgents(<ThreadNode thread={thread()} machine={machine()} />, h);
    const chip = await screen.findByRole("button", { name: "Started by Morning triage" });
    await userEvent.click(chip);
    expect(agentsSurface.get().view).toEqual({ kind: "automations", open: "aut_1", tab: "runs", run: "arn_1" });
    // The card itself didn't open the conversation.
    expect(h.openThread).not.toHaveBeenCalled();
  });
});
