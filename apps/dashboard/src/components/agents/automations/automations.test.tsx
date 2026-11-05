import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Automation, AutomationRun, AutomationsApi, AutomationSpec, StepRun } from "@/lib/automations-api";
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

const T = Date.now();

function automation(over: Partial<Automation> = {}): Automation {
  return {
    id: "aut_1",
    name: "Morning triage",
    enabled: true,
    trigger: { type: "schedule", cron: "0 9 * * 1-5", timezone: "UTC" },
    filters: [],
    steps: [{ id: "tell", kind: "notify", config: { title: "Hi", level: "info", link: null }, when: [], retry: null, timeout_s: null }],
    concurrency: "parallel",
    rate_limit: null,
    run_as: null,
    revision: 1,
    cursor: 1,
    created_at: T,
    updated_at: T,
    created_by: "ada",
    secrets: [],
    webhook: null,
    next_at: null,
    last_run: null,
    ...over,
  };
}

const stepRun = (id: string, kind: string, status: StepRun["status"], over: Partial<StepRun> = {}): StepRun => ({ id, kind, status, attempt: 1, started_at: T - 5000, finished_at: status === "succeeded" ? T - 1000 : null, next_at: null, output: null, error: null, log: [], ...over });

function automationRun(over: Partial<AutomationRun> = {}): AutomationRun {
  return {
    id: "arn_1",
    automation: "aut_1",
    name: "Morning triage",
    status: "running",
    event: { id: "aev_1", source: "schedule", kind: "tick", summary: "Weekdays at 09:00", at: T - 6000 },
    steps: [stepRun("start", "agent.start", "succeeded"), stepRun("wait", "agent.wait", "waiting", { log: [{ at: T - 4000, level: "info", text: "Waiting for thr_1" }] })],
    threads: [{ machine: "mch_1", thread: "thr_1", step: "start" }],
    cursor: 5,
    created_at: T - 6000,
    started_at: T - 6000,
    finished_at: null,
    reason: null,
    ...over,
  };
}

function fakeApi(list: Automation[] = [], runs: AutomationRun[] = []): AutomationsApi {
  const saved = (a: Automation) => ({ automation: a, webhook_secret: null });
  return {
    list: vi.fn(async () => list),
    get: vi.fn(async (_o: string, id: string) => list.find((a) => a.id === id)!),
    create: vi.fn(async (_o: string, spec: AutomationSpec) => saved(automation({ ...spec, id: "aut_new", cursor: 10 }))),
    update: vi.fn(async (_o: string, id: string, spec: AutomationSpec) => saved(automation({ ...spec, id, revision: 2, cursor: 11 }))),
    remove: vi.fn(async () => {}),
    enable: vi.fn(async (_o: string, id: string) => automation({ ...list.find((a) => a.id === id), enabled: true, cursor: 20 })),
    disable: vi.fn(async (_o: string, id: string) => automation({ ...list.find((a) => a.id === id), enabled: false, cursor: 20 })),
    run: vi.fn(async () => automationRun({ id: "arn_now", status: "queued" })),
    runsOf: vi.fn(async () => runs),
    runs: vi.fn(async () => runs),
    runDetail: vi.fn(async (_o: string, id: string) => ({ run: runs.find((r) => r.id === id)!, event: { id: "aev_1", source: "schedule", kind: "tick", key: "schedule:1", at: T, data: { at: T }, automation: "aut_1" } })),
    cancel: vi.fn(async (_o: string, id: string) => ({ ...runs.find((r) => r.id === id)!, status: "cancelled" as const, cursor: 30 })),
    retry: vi.fn(),
    setSecret: vi.fn(async () => {}),
    removeSecret: vi.fn(async () => {}),
    rotateWebhookSecret: vi.fn(),
    catalog: vi.fn(async () => ({ triggers: [], steps: [] })),
    test: vi.fn(async () => ({ matched: true, trigger: true, filters: [], steps: [{ id: "notify", runs: true, config: { title: "Hi there" }, error: null }] })),
  } as AutomationsApi;
}

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
    expect(within(row).getByLabelText("1 runs in 7 days, 1 failed")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Fix a broken main" })).toHaveTextContent("When a work item changes in acme/roll");

    await userEvent.click(within(row).getByRole("switch", { name: "Pause Morning triage" }));
    expect(api.disable).toHaveBeenCalledWith("acme", "aut_1");
    await waitFor(() => expect(within(screen.getByRole("button", { name: "Morning triage" })).getByText("Paused")).toBeInTheDocument());

    await userEvent.click(row);
    expect(agentsSurface.get().view).toEqual({ kind: "automations", open: "aut_1" });
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
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new" }} onClose={() => {}} />, harness(api));
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
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new" }} onClose={() => {}} />, harness(fakeApi()));
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
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new" }} onClose={() => {}} />, harness(api));
    await userEvent.click(screen.getByRole("button", { name: "Create" }));
    expect(screen.getByRole("contentinfo")).toHaveTextContent("2 things to fix");
    expect(screen.getByText("Add a step: what should happen.")).toBeInTheDocument();
    expect(api.create).not.toHaveBeenCalled();
  });

  it("opens a template ready to go, pointing at the owner's own machine and project", async () => {
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new", template: "run-on-machine" }} onClose={() => {}} />, harness(fakeApi()));
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
    renderAgents(<RunDetail run="arn_1" onBack={() => {}} />, h);
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
    renderAgents(<RunDetail run="arn_1" onBack={() => {}} />, harness(api));
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
