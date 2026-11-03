import { afterEach, describe, expect, it } from "vitest";
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
});
