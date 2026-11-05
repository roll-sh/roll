import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AutomationsApi } from "@/lib/automations-api";
import { automation, automationRun, fakeApi, T } from "@/test/automations";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { machine, mirrored, project, provider, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { ThreadNode } from "../nodes";
import { AutomationPanel, forgetDrafts } from "./automation-panel";
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

  it("searches and filters (status, trigger), shows them as chips, and acts on a row from its hover actions", async () => {
    const hook = automation({ id: "aut_3", name: "Deploy hook", enabled: false, trigger: { type: "webhook" } as never, steps: [{ id: "post", kind: "slack.post", config: {}, when: [], retry: null, timeout_s: null }] });
    const api = fakeApi([automation(), automation({ id: "aut_2", name: "Fix a broken main", trigger: { type: "code", events: ["work"], repos: ["acme/roll"] } }), hook], [automationRun({ status: "running" })]);
    renderAgents(<AutomationsView view={{ kind: "automations" }} phone={false} />, harness(api));
    // The filter popover makes the page inert while it shows: read the rows behind it.
    const rows = () => within(screen.getByRole("list", { name: "Your automations", hidden: true })).getAllByRole("button", { name: /^(Morning triage|Fix a broken main|Deploy hook)$/, hidden: true }).map((r) => r.getAttribute("aria-label"));
    await screen.findByRole("button", { name: "Morning triage" });
    // A run going on breathes on its row; Slack is marked as what the hook's steps use.
    expect(within(screen.getByRole("button", { name: "Deploy hook" })).getByRole("img", { name: "Uses Slack" })).toBeInTheDocument();

    await userEvent.type(screen.getByRole("textbox", { name: "Search automations" }), "slack");
    expect(rows()).toEqual(["Deploy hook"]);
    await userEvent.click(screen.getByRole("button", { name: "Clear the search" }));

    await userEvent.click(screen.getByRole("button", { name: "Filter" }));
    const popup = await screen.findByRole("dialog", { name: "Filter automations" });
    await userEvent.click(within(within(popup).getByRole("group", { name: "Status" })).getByRole("button", { name: /Running now/ }));
    expect(rows()).toEqual(["Morning triage"]);
    await userEvent.click(within(within(popup).getByRole("group", { name: "Status" })).getByRole("button", { name: /^On/ }));
    await userEvent.click(within(within(popup).getByRole("group", { name: "Trigger" })).getByRole("button", { name: /roll code/ }));
    await userEvent.keyboard("{Escape}");
    expect(rows()).toEqual(["Fix a broken main"]);
    const chips = screen.getByLabelText("Filters");
    expect(chips).toHaveTextContent("Status On");
    expect(chips).toHaveTextContent("Trigger roll code");
    await userEvent.click(within(chips).getByRole("button", { name: "Remove the filter roll code" }));
    expect(rows()).toEqual(["Fix a broken main", "Morning triage"]);
    await userEvent.click(within(chips).getByRole("button", { name: "Clear" }));
    expect(rows()).toHaveLength(3);

    // ↓ moves from row to row.
    screen.getByRole("button", { name: "Fix a broken main" }).focus();
    await userEvent.keyboard("{ArrowDown}");
    expect(screen.getByRole("button", { name: "Morning triage" })).toHaveFocus();
    await userEvent.keyboard("{ArrowUp}");
    expect(screen.getByRole("button", { name: "Fix a broken main" })).toHaveFocus();

    const actions = screen.getByRole("group", { name: "Actions for Fix a broken main" });
    await userEvent.click(within(actions).getByRole("button", { name: "Run now" }));
    expect(api.run).toHaveBeenCalledWith("acme", "aut_2", { input: null });
    await waitFor(() => expect(agentsSurface.get().view).toEqual({ kind: "automations", list: undefined, open: "aut_2", tab: "runs", run: "arn_now" }));
    await userEvent.click(within(actions).getByRole("button", { name: "Duplicate" }));
    expect(api.create).toHaveBeenCalledWith("acme", expect.objectContaining({ name: "Fix a broken main (copy)", enabled: false }));
    await userEvent.click(within(actions).getByRole("button", { name: "Edit" }));
    expect(agentsSurface.get().view).toEqual({ kind: "automations", list: undefined, open: "aut_2", tab: "edit" });
    await userEvent.click(within(actions).getByRole("button", { name: "Pause" }));
    expect(api.disable).toHaveBeenCalledWith("acme", "aut_2");
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

  it("renames a step in place: its card and its open section stay while its id is typed", async () => {
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new" }} />, harness(fakeApi()));
    await userEvent.click(screen.getByRole("button", { name: "Add a step" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: /Notify me/ }));
    const card = screen.getByRole("region", { name: "Step 1: Notify me" });
    await userEvent.click(within(card).getByRole("button", { name: /Name, conditions, retries/ }));
    const id = within(card).getByRole("textbox", { name: "Step id" });
    await userEvent.clear(id);
    await userEvent.type(id, "tell_me");
    expect(screen.getByRole("textbox", { name: "Step id" })).toHaveValue("tell_me");
    expect(screen.getByRole("textbox", { name: "Step id" })).toHaveFocus();
    expect(screen.getByRole("button", { name: /Name, conditions, retries/ })).toHaveAttribute("aria-expanded", "true");
  });

  it("says what's wrong inline and doesn't save until it's fixed", async () => {
    const api = fakeApi();
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new" }} />, harness(api));
    await userEvent.click(screen.getByRole("button", { name: "Create" }));
    expect(screen.getByRole("contentinfo")).toHaveTextContent("2 things to fix");
    expect(screen.getByText("Add a step: what should happen.")).toBeInTheDocument();
    expect(api.create).not.toHaveBeenCalled();
  });

  it("opens a template ready to go, pointing at the owner's own machine and project", async () => {
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new", template: "run-on-machine" }} />, harness(fakeApi()));
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("Run on ada-mbp");
    expect(screen.getByRole("region", { name: "Step 1: Start an agent" })).toHaveTextContent("on ada-mbp");
    expect(screen.getAllByRole("region", { name: /^Step / })).toHaveLength(3);
  });
});

describe("Automation runs", () => {
  it("follows a run live: steps move as automation_run events arrive, its thread opens beside the canvas", async () => {
    const r = automationRun();
    const api = fakeApi([automation()], [r]);
    const h = harness(api);
    renderAgents(<RunDetail run="arn_1" />, h);
    const steps = await screen.findByRole("list", { name: "Steps" });
    expect(within(steps).getByText("Waiting")).toBeInTheDocument();
    expect(within(steps).getByText("Waiting for thr_1")).toBeInTheDocument();

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
