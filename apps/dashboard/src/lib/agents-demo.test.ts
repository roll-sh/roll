import { afterEach, describe, expect, it } from "vitest";
import { machineState, type Published } from "./agents-api";
import { AgentsClient } from "./agents-client";
import { DemoOwner } from "./agents-demo";
import { busyThreads, pendingRequests, runsOf } from "./agents-store";

const until = async (what: string, ok: () => boolean, ms = 4000) => {
  const t0 = Date.now();
  while (!ok()) {
    if (Date.now() - t0 > ms) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 5));
  }
};

describe("agents demo", () => {
  let demo: DemoOwner;
  afterEach(() => demo?.dispose());

  it("drives the store like a relay: threads run, approvals wait, answers continue them", async () => {
    demo = new DemoOwner(0.01, false);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    client.follow("test", ["thr_demo_flaky"]);
    await until("the machines", () => client.data.machines.size === 4);
    // roll, web and infra, plus four with no thread yet (their machines list them).
    expect(client.data.projects.size).toBe(7);
    await until("busy threads", () => busyThreads(client.data).length >= 2);

    // The flaky-test thread streams, delegates to a subagent, then asks before running tests.
    await until("an approval", () => pendingRequests(client.data, "thr_demo_flaky").length === 1);
    const nodes = [...client.data.nodes.get("thr_demo_flaky")!.values()].map((n) => n.value.body.type);
    expect(nodes).toEqual(expect.arrayContaining(["reasoning", "message", "todos", "subagent", "tool", "request"]));
    expect(client.data.threads.get("thr_demo_flaky")?.value.status).toBe("waiting");

    const [req] = pendingRequests(client.data, "thr_demo_flaky");
    expect(await client.send("mch_demo_ada", { type: "request.respond", request: req.id, response: { type: "decision", decision: "accept" } })).toEqual({ type: "ok" });
    await until("the run to complete", () => runsOf(client.data, "thr_demo_flaky")[0]?.status === "completed");
    const t = client.data.threads.get("thr_demo_flaky")!.value;
    expect(t.status).toBe("idle");
    expect(t.usage.output_tokens).toBeGreaterThan(0);
    expect(runsOf(client.data, "thr_demo_flaky")[0].stat?.files).toBe(2);
    release();
  });

  it("reports each thread's context window, growing while it runs", async () => {
    demo = new DemoOwner(0.01, false);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    const ctx = (id: string) => client.data.threads.get(id)?.value.context ?? null;
    await until("the finished thread", () => ctx("thr_demo_json") !== null);
    expect(ctx("thr_demo_json")).toEqual({ used_tokens: 31_800, max_tokens: 258_400, total_processed_tokens: 45_100 });
    await until("a first report", () => ctx("thr_demo_ci") !== null);
    const first = ctx("thr_demo_ci")!.used_tokens;
    await until("the context to grow", () => (ctx("thr_demo_ci")?.used_tokens ?? 0) > first);
    expect(ctx("thr_demo_ci")).toMatchObject({ max_tokens: 258_400 });
    expect(ctx("thr_demo_ci")!.total_processed_tokens).toBeGreaterThan(ctx("thr_demo_ci")!.used_tokens);
    await until("claude's report", () => ctx("thr_demo_flaky") !== null);
    expect(ctx("thr_demo_flaky")).toMatchObject({ max_tokens: 1_000_000, total_processed_tokens: null });
    const run = runsOf(client.data, "thr_demo_ci").at(-1)!;
    expect(run.context).toEqual(ctx("thr_demo_ci"));
    release();
  });

  it("answers commands: queue while running, fork, offline machines", async () => {
    demo = new DemoOwner(0.01, false);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    await until("a running thread", () => client.data.threads.get("thr_demo_flaky")?.value.status === "running");
    await client.run("mch_demo_ada", { type: "message.send", thread: "thr_demo_flaky", text: "then run clippy", mode: "queue" });
    await until("the queued message", () => client.data.threads.get("thr_demo_flaky")!.value.queued.length === 1);
    const fork = await client.run<{ type: "created"; id: string }>("mch_demo_ada", { type: "thread.fork", thread: "thr_demo_json" }, "created");
    await until("the fork", () => client.data.threads.get(fork.id)?.value.lineage?.parent === "thr_demo_json");
    expect(await client.send("mch_nope", { type: "provider.refresh" })).toMatchObject({ type: "error", code: "machine_offline" });
    release();
  });

  it("puts its sandbox to sleep while it waits on you, and the answer wakes it", async () => {
    demo = new DemoOwner(0.01, false);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    const box = () => client.data.machines.get("mch_demo_box")?.value;
    await until("the plan approval", () => pendingRequests(client.data, "thr_demo_ci").length === 1);
    await until("the sandbox asleep", () => box()?.sandbox?.stopped_reason === "idle");
    expect(machineState(box()!)).toBe("asleep");
    expect((await demo.services().sandboxes.list("demo")).sandboxes[0]).toMatchObject({ status: "stopped", desired: "asleep", stopped_reason: "idle" });
    // Nothing lost: the thread still waits, its approval still answerable.
    expect(client.data.threads.get("thr_demo_ci")?.value.status).toBe("waiting");
    const [req] = pendingRequests(client.data, "thr_demo_ci");
    const answered = client.send("mch_demo_box", { type: "request.respond", request: req.id, response: { type: "decision", decision: "accept" } });
    await until("waking", () => box()?.sandbox?.status === "starting");
    expect(machineState(box()!)).toBe("waking");
    expect(await answered).toEqual({ type: "ok" });
    expect(box()?.online).toBe(true);
    await until("the run to complete", () => runsOf(client.data, "thr_demo_ci")[0]?.status === "completed");
    release();
  });

  it("starts with no machines when asked (?demo=empty, the first-run screen) and a sandbox is the first", async () => {
    demo = new DemoOwner(0.01, false, true);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    const services = demo.services();
    expect((await services.sandboxes.list("demo")).sandboxes).toEqual([]);
    await new Promise((r) => setTimeout(r, 50));
    expect(client.data.machines.size).toBe(0);
    expect(client.data.threads.size).toBe(0);
    const { machine } = await services.sandboxes.create("demo", { name: "first", env: {} });
    await until("it to boot", () => client.data.machines.get(machine.id)?.value.online === true);
    expect(client.data.machines.size).toBe(1);
    // Its login runs in a terminal, finishes, and the machine shows the provider logged in.
    const claude = () => client.data.machines.get(machine.id)?.value.info?.providers.find((p) => p.id === "claude");
    expect(claude()?.auth).toBe("missing");
    const opened = await client.send(machine.id, { type: "terminal.open", command: "claude /login", cols: 80, rows: 24 });
    if (opened.type !== "created") throw new Error(JSON.stringify(opened));
    await until("the login", () => claude()?.auth === "ready");
    expect(client.data.terminals.get(opened.id)?.value).toMatchObject({ status: "exited", exit_code: 0 });
    expect(client.data.machines.get(machine.id)?.value.info?.providers.find((p) => p.id === "codex")?.auth).toBe("missing");
    release();
  });

  it("rents sandboxes that boot into machines, and follows a published change to landed", async () => {
    demo = new DemoOwner(0.01, false);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    const services = demo.services();
    const list = await services.sandboxes.list("demo");
    expect(list.sizes[0]).toMatchObject({ name: "standard", hourly_cents: 3 });
    expect(list.sandboxes.map((s) => s.machine)).toEqual(["mch_demo_box", "mch_demo_dock"]);

    const { sandbox, machine } = await services.sandboxes.create("demo", { name: "scratch", env: { ANTHROPIC_API_KEY: "sk-test" } });
    expect(sandbox.status).toBe("creating");
    await until("the sandbox machine", () => client.data.machines.get(machine.id)?.value.sandbox?.status === "creating");
    await until("it to boot", () => client.data.machines.get(machine.id)?.value.online === true);
    const providers = client.data.machines.get(machine.id)!.value.info!.providers;
    // The key it was given logs Claude in; Codex waits for a login.
    expect(providers.map((p) => [p.id, p.auth])).toEqual([
      ["claude", "ready"],
      ["codex", "missing"],
    ]);
    await services.sandboxes.stop("demo", sandbox.id);
    expect(client.data.machines.get(machine.id)?.value.online).toBe(false);
    await services.sandboxes.remove("demo", sandbox.id);
    await until("the machine to go", () => !client.data.machines.has(machine.id));

    const published = await client.run<{ type: "published" } & Published>("mch_demo_ada", { type: "thread.publish", thread: "thr_demo_json", message: "Add --json", land: true }, "published");
    expect(published.landing).toMatchObject({ status: "queued" });
    await until("the change to land", () => client.data.threads.get("thr_demo_json")?.value.change?.status === "landed");

    const work = await services.takeWork("demo", "thr_demo_fork", { work: "acme/roll#77" });
    expect(work).toMatchObject({ repo: "acme/roll", number: 77 });
    await until("the work on the thread", () => client.data.threads.get("thr_demo_fork")?.value.work?.number === 77);
    release();
  });

  it("orchestrates a goal: plans, spins up agents under it in their own worktrees, and sums up once they're done", async () => {
    demo = new DemoOwner(0.01, false);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    await until("the machines", () => client.data.machines.size === 4);
    const goal = "Add dark mode to the settings page";
    const { id } = await client.run<{ type: "created"; id: string }>(
      "mch_demo_ada",
      { type: "thread.create", project: "prj_demo_roll", provider: "claude", runtime_mode: "full_access", prompt: goal, orchestrator: true },
      "created",
    );
    client.follow("test", [id]);
    const children = () => [...client.data.threads.values()].map((r) => r.value).filter((t) => t.lineage?.parent === id);
    await until("three agents", () => children().length === 3, 8000);
    expect(client.data.threads.get(id)!.value.orchestrator).toBe(true);
    const first = runsOf(client.data, id)[0];
    expect(first.prompt).toBe(goal);
    for (const c of children()) {
      expect(c).toMatchObject({ orchestrator: false, lineage: { relation: "subagent", root: id, from_run: first.id } });
      expect(c.worktree?.branch).toMatch(/^roll\//);
    }
    // Spread over the machine's providers, each a task node in the plan's run that names its thread.
    expect(new Set(children().map((c) => c.provider))).toEqual(new Set(["claude", "codex"]));
    const nodes = () => [...(client.data.nodes.get(id)?.values() ?? [])].map((n) => n.value);
    await until("the task nodes", () => nodes().filter((n) => n.body.type === "subagent" && n.body.child).length === 3);
    expect(nodes().map((n) => n.body.type)).toEqual(expect.arrayContaining(["reasoning", "tool", "plan", "message", "subagent"]));

    // The tests agent asks before running them; its node waits with it, and the answer goes on.
    const asks = () => pendingRequests(client.data).filter((q) => children().some((c) => c.id === q.thread));
    await until("the tests agent to ask", () => asks().length === 1, 8000);
    const waiting = nodes().find((n) => n.body.type === "subagent" && n.body.child === asks()[0].thread);
    expect(waiting?.status).toBe("waiting");
    await client.run("mch_demo_ada", { type: "request.respond", request: asks()[0].id, response: { type: "decision", decision: "accept" } });

    // Each result wakes the orchestrator; the last wake sums it all up.
    const done = () => children().every((c) => c.status === "idle") && client.data.threads.get(id)!.value.status === "idle";
    await until("everything done", () => done() && nodes().some((n) => n.body.type === "message" && n.body.text.startsWith("All done")), 15000);
    const runs = runsOf(client.data, id);
    expect(runs.length).toBeGreaterThanOrEqual(2);
    expect(runs.slice(1).map((r) => r.prompt).join("\n")).toMatch(/Delegated task nod_\w+ \(Build: .*\) \[completed\]/);
    expect(nodes().filter((n) => n.body.type === "subagent").every((n) => n.status === "completed" && n.body.type === "subagent" && !!n.body.result)).toBe(true);
    release();
  });
});
