import { afterEach, describe, expect, it } from "vitest";
import type { NodeBody, Run } from "./agents-api";
import { AgentsClient } from "./agents-client";
import { DemoOwner } from "./agents-demo";
import { implementationPrompt, implementationThreadTitle, PLAN_IMPLEMENTATION_PREFIX, planBody, planFilename, planTitle, readyPlan } from "./agents-plan";
import { nodeTree, runsOf } from "./agents-store";
import { foldTurn, timelineItems } from "./agents-worklog";

const until = async (what: string, ok: () => boolean, ms = 6000) => {
  const t0 = Date.now();
  while (!ok()) {
    if (Date.now() - t0 > ms) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 5));
  }
};

describe("T3's proposed-plan helpers", () => {
  const plan = "# Export invoices as CSV!\n\n## Summary\n\nAdd a route.\n\n1. Route\n2. Button\n";

  it("titles the plan by its first heading, and strips it (and a Summary heading) for the card", () => {
    expect(planTitle(plan)).toBe("Export invoices as CSV!");
    expect(planTitle("1. Just steps")).toBeNull();
    expect(planBody(plan)).toBe("Add a route.\n\n1. Route\n2. Button");
  });

  it("builds Implement's message, the new thread's title and the download's name", () => {
    expect(implementationPrompt(`  ${plan}`)).toBe(`PLEASE IMPLEMENT THIS PLAN:\n${plan.trim()}`);
    expect(implementationThreadTitle(plan)).toBe("Implement Export invoices as CSV!");
    expect(implementationThreadTitle("1. Steps")).toBe("Implement plan");
    expect(planFilename(plan)).toBe("export-invoices-as-csv.md");
    expect(planFilename("no title")).toBe("plan.md");
  });
});

describe("the turn fold", () => {
  const n = (ordinal: number, body: NodeBody) => ({ node: { id: `n${ordinal}`, thread: "t", run: "r", parent: null, ordinal, status: "completed" as const, started_at: 0, completed_at: 0, body }, children: [] });
  it("folds the work but keeps a plan and a compaction where they are (T3 folds work entries only)", () => {
    const items = timelineItems([
      n(1, { type: "reasoning", text: "Look first." }),
      n(2, { type: "compaction", before_tokens: 200, after_tokens: 50, error: null }),
      n(3, { type: "plan", text: "# P" }),
      n(4, { type: "message", text: "That's the plan." }),
    ]);
    const turn = foldTurn(items, { status: "completed" } as Pick<Run, "status">);
    expect(turn.fold?.map((i) => (i.kind === "work" ? i.entries[0].node.id : i.t.node.id))).toEqual(["n1"]);
    expect(turn.rest.map((i) => (i.kind === "node" ? i.t.node.body.type : "work"))).toEqual(["compaction", "plan", "message"]);
  });
});

describe("the demo's queue, compaction and plans", () => {
  let demo: DemoOwner;
  afterEach(() => demo?.dispose());
  const start = async () => {
    demo = new DemoOwner(0.01, false);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    client.follow("test", ["thr_demo_flaky", "thr_demo_billing"]);
    await until("the demo", () => client.data.threads.size > 5);
    await until("the plan thread's nodes", () => (client.data.nodes.get("thr_demo_billing")?.size ?? 0) > 0);
    return { client, release };
  };

  it("edits, reorders and sends now; a stopped thread's queue waits until one is sent", async () => {
    const { client, release } = await start();
    const t = () => client.data.threads.get("thr_demo_flaky")!.value;
    await until("a running thread", () => t().status === "running");
    const queue = async (text: string) => (await client.run<{ type: "created"; id: string }>("mch_demo_ada", { type: "message.send", thread: "thr_demo_flaky", text, mode: "queue" }, "created")).id;
    const a = await queue("one");
    const b = await queue("two");
    await client.run("mch_demo_ada", { type: "message.edit", thread: "thr_demo_flaky", message: a, text: "one, edited" });
    await client.run("mch_demo_ada", { type: "message.reorder", thread: "thr_demo_flaky", message: b, before: a });
    await until("the new order", () => t().queued.map((q) => q.text).join() === "two,one, edited");
    expect(await client.send("mch_demo_ada", { type: "message.edit", thread: "thr_demo_flaky", message: a, text: " " })).toMatchObject({ type: "error", code: "invalid" });

    await client.run("mch_demo_ada", { type: "message.promote", thread: "thr_demo_flaky", message: b });
    await until("the steer", () => runsOf(client.data, "thr_demo_flaky")[0].steers.includes("two"));
    expect(t().queued.map((q) => q.text)).toEqual(["one, edited"]);

    await client.run("mch_demo_ada", { type: "run.interrupt", thread: "thr_demo_flaky" });
    await until("stopped", () => t().status === "idle");
    expect(t().queued).toHaveLength(1);
    await client.run("mch_demo_ada", { type: "message.promote", thread: "thr_demo_flaky", message: a });
    await until("its run", () => runsOf(client.data, "thr_demo_flaky").at(-1)?.prompt === "one, edited");
    expect(t().queued).toHaveLength(0);
    release();
  });

  it("/compact runs as a compaction that shrinks the context; steering it queues", async () => {
    const { client, release } = await start();
    const t = () => client.data.threads.get("thr_demo_billing")!.value;
    await client.run("mch_demo_ada", { type: "message.send", thread: "thr_demo_billing", text: "/compact" });
    await until("compacting", () => t().status === "running");
    await client.run("mch_demo_ada", { type: "message.send", thread: "thr_demo_billing", text: "then go on", mode: "steer" });
    expect(t().queued.map((q) => q.text)).toEqual(["then go on"]);
    const run = () => runsOf(client.data, "thr_demo_billing").find((r) => r.prompt === "/compact")!;
    await until("compacted", () => run().status === "completed");
    const body = nodeTree(client.data, "thr_demo_billing", run().id)[0].node.body;
    expect(body).toMatchObject({ type: "compaction", before_tokens: 61_900, after_tokens: Math.round(61_900 * 0.24) });
    release();
  });

  it("seeds a plan ready without an approval; Implement in build mode carries it out", async () => {
    const { client, release } = await start();
    const t = () => client.data.threads.get("thr_demo_billing")!.value;
    const ready = readyPlan(client.data, t());
    expect(ready).toMatchObject({ title: "Export invoices as CSV", approval: null });
    await client.run("mch_demo_ada", { type: "thread.configure", thread: "thr_demo_billing", interaction_mode: "default" });
    await client.run("mch_demo_ada", { type: "message.send", thread: "thr_demo_billing", text: implementationPrompt(ready!.text) });
    await until("implemented", () => runsOf(client.data, "thr_demo_billing").at(-1)?.status === "completed" && runsOf(client.data, "thr_demo_billing").length === 2, 20_000);
    const last = runsOf(client.data, "thr_demo_billing").at(-1)!;
    expect(last.prompt.startsWith(PLAN_IMPLEMENTATION_PREFIX)).toBe(true);
    expect(last.stat?.files).toBe(2);
    // Out of plan mode, the plan is no longer ready.
    expect(readyPlan(client.data, t())).toBeNull();
    release();
  });
});
