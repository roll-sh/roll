import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AutomationsApi } from "@/lib/automations-api";
import { automation, automationRun, fakeApi, T } from "@/test/automations";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { machine, mirrored, project, provider, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { macClick } from "@/test/mac-click";
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
  it("links a step whose integration isn't connected to the settings that connect it, searched for it", async () => {
    const api = fakeApi();
    api.catalog = vi.fn(async () => ({
      triggers: [],
      steps: [{ kind: "linear.comment", title: "Comment on a Linear issue", description: "", config: { type: "object", properties: {} }, output: [], integration: "Linear", needs_setup: true }],
    }));
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new" }} />, harness(api));
    await userEvent.click(screen.getByRole("button", { name: "Add a step" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: /Comment on a Linear issue/ }));
    const card = screen.getByRole("region", { name: /Step 1: Comment on a Linear issue/ });
    expect(card).toHaveTextContent("Connect Linear in Agents settings before this step can run.");
    macClick(within(card).getByRole("button", { name: "Agents settings" }));
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: "owner" });
  });

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

  it("a step added to a saved automation isn't red before it's filled in or a save is tried", async () => {
    const api = fakeApi([automation()]);
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "aut_1", tab: "edit" }} />, harness(api));
    await userEvent.click(await screen.findByRole("button", { name: "Add a step" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: /Call a webhook/ }));
    const card = screen.getByRole("region", { name: "Step 2: Call a webhook" });
    expect(card.querySelector(".text-danger")).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(api.update).not.toHaveBeenCalled();
    await waitFor(() => expect(screen.getByRole("region", { name: "Step 2: Call a webhook" }).querySelector(".text-danger")).not.toBeNull());
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

  it("keeps a step's wait between tries while its tries are retyped (10 passes through 1, which has none)", async () => {
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new" }} />, harness(fakeApi()));
    await userEvent.click(screen.getByRole("button", { name: "Add a step" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: /Notify me/ }));
    const card = screen.getByRole("region", { name: "Step 1: Notify me" });
    await userEvent.click(within(card).getByRole("button", { name: /Name, conditions, retries/ }));
    const tries = within(card).getByRole("spinbutton", { name: "Tries" });
    await userEvent.clear(tries);
    await userEvent.type(tries, "3");
    const wait = within(card).getByRole("spinbutton", { name: "Backoff seconds" });
    await userEvent.clear(wait);
    await userEvent.type(wait, "120");
    await userEvent.clear(tries);
    await userEvent.type(tries, "10");
    expect(within(card).getByRole("spinbutton", { name: "Tries" })).toHaveValue(10);
    expect(within(card).getByRole("spinbutton", { name: "Backoff seconds" })).toHaveValue(120);
  });

  it("says what's wrong inline and doesn't save until it's fixed", async () => {
    const api = fakeApi();
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new" }} />, harness(api));
    await userEvent.click(screen.getByRole("button", { name: "Create" }));
    expect(screen.getByRole("contentinfo")).toHaveTextContent("2 things to fix");
    expect(screen.getByText("Add a step: what should happen.")).toBeInTheDocument();
    expect(api.create).not.toHaveBeenCalled();
  });

  it("refuses a name another automation has, and a Create that can't go takes you to what's wrong", async () => {
    const api = fakeApi([automation({ id: "aut_1", name: "Morning triage" })]);
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new" }} />, harness(api));
    await userEvent.type(screen.getByRole("textbox", { name: "Name" }), "morning triage");
    await userEvent.click(screen.getByRole("button", { name: "Add a step" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: /Notify me/ }));
    await userEvent.click(screen.getByRole("button", { name: "Create" }));
    expect(api.create).not.toHaveBeenCalled();
    const name = screen.getByRole("textbox", { name: "Name" });
    expect(name).toHaveAttribute("aria-invalid", "true");
    expect(name).toHaveAttribute("title", "Another automation is called morning triage.");
    await waitFor(() => expect(name).toHaveFocus());
    // The footer goes there again.
    await userEvent.click(screen.getByRole("button", { name: "Add a step" }));
    macClick(within(screen.getByRole("contentinfo")).getByRole("button", { name: "2 things to fix" }));
    await waitFor(() => expect(name).toHaveFocus());
  });

  it("opens a template ready to go, pointing at the owner's own machine and project", async () => {
    renderAgents(<AutomationPanel view={{ kind: "automations", open: "new", template: "run-on-machine" }} />, harness(fakeApi()));
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("Run on ada-mbp");
    expect(screen.getByRole("region", { name: "Step 1: Start an agent" })).toHaveTextContent("on ada-mbp");
    expect(screen.getAllByRole("region", { name: /^Step / })).toHaveLength(3);
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

  it("keeps a retried run's failure in sight and times it from the retry", async () => {
    const base = automationRun();
    const now = Date.now();
    const hour = 3600_000;
    const r = {
      ...base,
      status: "running" as const,
      started_at: now - 30 * hour,
      retried_at: now - 5000,
      steps: [
        base.steps[0],
        {
          ...base.steps[1],
          status: "running" as const,
          started_at: now - 4000,
          error: null,
          log: [
            { at: now - 29 * hour, level: "error" as const, text: "hooks.example.com answered 400" },
            { at: now - 5000, level: "info" as const, text: "Retried after the run failed: step wait failed" },
            { at: now - 2000, level: "info" as const, text: "Waiting for thr_1" },
          ],
        },
      ],
    };
    renderAgents(<RunDetail run="arn_1" />, harness(fakeApi([automation()], [r])));
    const card = (await screen.findByText(/· retried/)).parentElement!;
    // From the retry (seconds), not from the first start a day earlier.
    expect(card).not.toHaveTextContent(/\b(29|30)h\b/);
    const log = screen.getByLabelText("Log");
    expect(log).toHaveTextContent("hooks.example.com answered 400");
    expect(log).toHaveTextContent("+2.0s");
    expect(log).not.toHaveTextContent("+-");
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
