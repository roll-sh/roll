import { afterEach, describe, expect, it, vi } from "vitest";
import { AgentsClient } from "./agents-client";
import { DemoOwner } from "./agents-demo";
import type { Automation, AutomationRun, AutomationsApi } from "./automations-api";
import { AutomationsStore, automationsFor } from "./automations-store";

const auto = (id: string, cursor: number, name = id): Automation => ({
  id,
  name,
  enabled: true,
  trigger: { type: "manual" },
  filters: [],
  steps: [],
  concurrency: "parallel",
  rate_limit: null,
  run_as: null,
  revision: 1,
  cursor,
  created_at: 0,
  updated_at: 0,
  created_by: "ada",
  secrets: [],
  webhook: null,
  next_at: null,
  last_run: null,
});

const run = (id: string, cursor: number, over: Partial<AutomationRun> = {}): AutomationRun => ({
  id,
  automation: "aut_1",
  name: "one",
  status: "running",
  event: { id: "e", source: "manual", kind: "run", summary: "", at: 0 },
  steps: [],
  threads: [],
  cursor,
  created_at: cursor,
  started_at: cursor,
  finished_at: null,
  reason: null,
  ...over,
});

function api(list: Automation[], runs: AutomationRun[] = []): AutomationsApi {
  return {
    list: vi.fn(async () => list),
    runs: vi.fn(async () => runs),
    catalog: vi.fn(async () => ({ triggers: [], steps: [] })),
  } as unknown as AutomationsApi;
}

describe("automations store", () => {
  it("keeps the newer cursor whether it came from a load or the stream", async () => {
    let release!: () => void;
    const slow = new Promise<void>((r) => (release = r));
    const a = api([auto("aut_1", 5, "old")]);
    (a.list as ReturnType<typeof vi.fn>).mockImplementation(async () => (await slow, [auto("aut_1", 5, "old")]));
    const s = new AutomationsStore("acme", a);
    const loading = s.ensure();
    // The stream brings a newer one while the list is on its way.
    s.apply({ type: "automation", cursor: 9, automation: auto("aut_1", 9, "new") });
    release();
    await loading;
    expect(s.getSnapshot().automations.get("aut_1")?.name).toBe("new");
  });

  it("doesn't bring back what was removed, and indexes the threads runs started", async () => {
    const s = new AutomationsStore("acme", api([]));
    await s.ensure();
    s.apply({ type: "automation", cursor: 3, automation: auto("aut_1", 3, "one") });
    s.apply({ type: "automation_removed", cursor: 4, automation: "aut_1" });
    s.apply({ type: "automation", cursor: 3, automation: auto("aut_1", 3, "one") });
    expect(s.getSnapshot().automations.has("aut_1")).toBe(false);

    s.apply({ type: "automation_run", cursor: 6, run: run("arn_1", 6, { threads: [{ machine: "m", thread: "thr_1", step: "start" }] }) });
    // An older copy of the run (a replay) changes nothing.
    s.apply({ type: "automation_run", cursor: 5, run: run("arn_1", 5) });
    expect(s.getSnapshot().runs.get("arn_1")?.cursor).toBe(6);
    expect(s.getSnapshot().threads.get("thr_1")).toMatchObject({ automation: "aut_1", run: "arn_1", step: "start" });
  });

  it("hears the client's stream: runs and notices off the same events the canvas gets", async () => {
    const transport = { state: async () => ({ cursor: 0, machines: [], entities: [] }), stream: () => () => {}, command: vi.fn() };
    const client = new AgentsClient("acme", transport, (fn) => fn());
    const s = automationsFor(client, api([]));
    const notices: string[] = [];
    s.onNotice((n) => notices.push(n.title));
    client.enqueue({ type: "automation_run", cursor: 2, run: run("arn_2", 2) });
    client.enqueue({ type: "notice", notice: { id: "n", title: "Done", level: "success", link: null, automation: null, run: null, at: 0 } });
    expect(s.getSnapshot().runs.has("arn_2")).toBe(true);
    expect(notices).toEqual(["Done"]);
    // The canvas's records don't change for them.
    expect(client.data.cursor).toBe(0);
    expect(automationsFor(client, s.api)).toBe(s);
  });
});

describe("automations demo", () => {
  let demo: DemoOwner;
  afterEach(() => demo?.dispose());

  it("lists a believable set, and a run starts a real thread, waits for it and sends a notice", async () => {
    demo = new DemoOwner(0.01, false);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    const api = demo.services().automations!;
    const s = automationsFor(client, api);
    await s.ensure();
    const st = s.getSnapshot();
    expect(st.automations.size).toBeGreaterThanOrEqual(5);
    expect([...st.runs.values()].some((r) => r.status === "waiting" && r.threads.length)).toBe(true);
    // A seeded run tags a thread on the canvas.
    expect(st.threads.get("thr_demo_flaky")?.name).toBe("Fix a broken main");

    const notices: string[] = [];
    s.onNotice((n) => notices.push(n.title));
    const started = await api.run("demo", "aut_demo_run", { input: { prompt: "Say hi" } });
    s.putRun(started);
    const until = async (what: string, ok: () => boolean, ms = 8000) => {
      const t0 = Date.now();
      while (!ok()) {
        if (Date.now() - t0 > ms) throw new Error(`timed out waiting for ${what}`);
        await new Promise((r) => setTimeout(r, 10));
      }
    };
    await until("a thread", () => (s.getSnapshot().runs.get(started.id)?.threads.length ?? 0) > 0);
    const thread = s.getSnapshot().runs.get(started.id)!.threads[0].thread;
    await until("the thread on the canvas", () => client.data.threads.has(thread));
    expect(client.data.threads.get(thread)?.value.worktree).not.toBeNull();
    await until("the run to finish", () => s.getSnapshot().runs.get(started.id)?.status === "succeeded");
    expect(notices.length).toBe(1);
    const detail = await api.runDetail("demo", started.id);
    expect(detail.event.data).toMatchObject({ input: { prompt: "Say hi" } });

    // A test renders templates and says which conditions hold, running nothing.
    const a = (await api.get("demo", "aut_demo_mainfix"))!;
    const t = await api.test("demo", { automation: a, event: { source: "code", kind: "work", data: { title: "Main is red", labels: ["broken-main"], state: "closed" } } });
    expect(t.filters).toEqual([true, false]);
    expect(t.matched).toBe(false);
    expect((t.steps[0].config as { title: string }).title).toBe("Fix main: Main is red");
    release();
  });
});
