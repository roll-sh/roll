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
    await until("two machines", () => client.data.machines.size === 2);
    expect(client.data.projects.size).toBe(3);
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

  it("rents sandboxes that boot into machines, and follows a published change to landed", async () => {
    demo = new DemoOwner(0.01, false);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    const services = demo.services();
    const list = await services.sandboxes.list("demo");
    expect(list.sizes[0]).toMatchObject({ name: "standard", hourly_cents: 3 });
    expect(list.sandboxes.map((s) => s.machine)).toEqual(["mch_demo_box"]);

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
});
