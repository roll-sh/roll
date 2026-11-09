import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { setTimeout as delay } from "node:timers/promises";
import {
  type CallReply,
  type CallRequest,
  type FunctionContext,
  type MemoisedStep,
  NonRetriableError,
  StepError,
  createFunction,
  executeFunction,
} from "../src/index.js";

const event = { id: "evt_1", name: "order.created", data: { order: 7 }, user: null, ts: 1 };

function call(handler: (ctx: FunctionContext) => unknown, steps: Record<string, MemoisedStep> = {}, extra: Partial<CallRequest> = {}): Promise<CallReply> {
  const fn = createFunction({ id: "fn" }, handler);
  return executeFunction(fn, { function_id: "fn", run_id: "run_1", attempt: 0, event, events: [event], steps, ...extra });
}

function steps(r: CallReply) {
  assert.equal(r.status, "steps", JSON.stringify(r));
  return (r as Extract<CallReply, { status: "steps" }>).steps;
}

describe("step execution", () => {
  it("passes the event and run to the handler", async () => {
    let seen: FunctionContext | undefined;
    const r = await call((ctx) => {
      seen = ctx;
      return { order: ctx.event.data.order };
    }, {}, { attempt: 2 });
    assert.deepEqual(r, { status: "done", output: { order: 7 } });
    assert.equal(seen!.runId, "run_1");
    assert.equal(seen!.functionId, "fn");
    assert.equal(seen!.attempt, 2);
    assert.deepEqual(seen!.events, [event]);
  });

  it("returns null for undefined output and normalises JSON", async () => {
    assert.deepEqual(await call(() => undefined), { status: "done", output: null });
    assert.deepEqual(await call(() => ({ at: new Date(0), skip: undefined })), { status: "done", output: { at: "1970-01-01T00:00:00.000Z" } });
  });

  it("runs the first new step and stops there", async () => {
    const ran: string[] = [];
    const r = await call(async ({ step }) => {
      const a = await step.run("a", () => {
        ran.push("a");
        return 1;
      });
      const b = await step.run("b", () => {
        ran.push("b");
        return 2;
      });
      return a + b;
    });
    assert.deepEqual(steps(r), [{ op: "run", id: "a", name: "a", data: 1 }]);
    assert.deepEqual(ran, ["a"]);
  });

  it("replays memoised steps without running them", async () => {
    const ran: string[] = [];
    const handler = async ({ step }: FunctionContext) => {
      const a = await step.run("a", () => {
        ran.push("a");
        return 1;
      });
      const b = await step.run("b", () => {
        ran.push("b");
        return 2;
      });
      return a + b;
    };
    const r = await call(handler, { a: { data: 1 } });
    assert.deepEqual(steps(r), [{ op: "run", id: "b", name: "b", data: 2 }]);
    assert.deepEqual(ran, ["b"]);
    assert.deepEqual(await call(handler, { a: { data: 1 }, b: { data: 2 } }), { status: "done", output: 3 });
    assert.deepEqual(ran, ["b"]);
  });

  it("reports parallel steps together and runs them concurrently", async () => {
    const started = Date.now();
    const r = await call(async ({ step }) => {
      const parts = await Promise.all([
        step.run("a", async () => (await delay(100), 1)),
        step.run("b", async () => (await delay(100), 2)),
        step.run("c", async () => (await delay(100), 3)),
      ]);
      return parts.reduce((x, y) => x + y);
    });
    assert.deepEqual(steps(r).map((s) => [s.id, (s as { data: unknown }).data]), [["a", 1], ["b", 2], ["c", 3]]);
    assert.ok(Date.now() - started < 250, `took ${Date.now() - started}ms`);
  });

  it("reports only the parallel steps not memoised yet", async () => {
    const handler = async ({ step }: FunctionContext) => {
      const [a, b] = await Promise.all([step.run("a", () => 1), step.run("b", () => 2)]);
      const s = await step.run("sum", () => a + b);
      return s;
    };
    assert.deepEqual(steps(await call(handler, { a: { data: 1 } })), [{ op: "run", id: "b", name: "b", data: 2 }]);
    assert.deepEqual(steps(await call(handler, { a: { data: 1 }, b: { data: 2 } })), [{ op: "run", id: "sum", name: "sum", data: 3 }]);
  });

  it("finds steps after non-step async work", async () => {
    const r = await call(async ({ step }) => {
      await delay(30);
      await step.run("late", () => "x");
    });
    assert.deepEqual(steps(r), [{ op: "run", id: "late", name: "late", data: "x" }]);
  });

  it("numbers repeated ids deterministically", async () => {
    const handler = async ({ step }: FunctionContext) => {
      const out = [];
      for (let i = 0; i < 3; i++) out.push(await step.run("item", () => i));
      return out;
    };
    assert.deepEqual(steps(await call(handler)), [{ op: "run", id: "item", name: "item", data: 0 }]);
    assert.deepEqual(steps(await call(handler, { item: { data: 0 } })), [{ op: "run", id: "item:1", name: "item", data: 1 }]);
    assert.deepEqual(steps(await call(handler, { item: { data: 0 }, "item:1": { data: 1 } })), [{ op: "run", id: "item:2", name: "item", data: 2 }]);
    assert.deepEqual(await call(handler, { item: { data: 0 }, "item:1": { data: 1 }, "item:2": { data: 2 } }), { status: "done", output: [0, 1, 2] });
    // Parallel duplicates too.
    const par = await call(({ step }) => Promise.all([step.run("p", () => "a"), step.run("p", () => "b")]));
    assert.deepEqual(steps(par).map((s) => s.id), ["p", "p:1"]);
  });

  it("uses display names", async () => {
    const r = await call(({ step }) => step.run({ id: "charge", name: "Charge the card" }, () => 1));
    assert.deepEqual(steps(r), [{ op: "run", id: "charge", name: "Charge the card", data: 1 }]);
  });

  it("stores undefined step results as null", async () => {
    const r = await call(({ step }) => step.run("nothing", () => undefined));
    assert.deepEqual(steps(r), [{ op: "run", id: "nothing", name: "nothing", data: null }]);
  });

  it("reports step failures as retriable unless NonRetriableError", async () => {
    const r = await call(({ step }) =>
      Promise.all([
        step.run("flaky", () => {
          throw new Error("boom");
        }),
        step.run("fatal", () => {
          throw new NonRetriableError("bad input");
        }),
      ]),
    );
    const [flaky, fatal] = steps(r) as { error: { name: string; message: string; stack?: string }; retriable: boolean }[];
    assert.equal(flaky!.error.name, "Error");
    assert.equal(flaky!.error.message, "boom");
    assert.ok(flaky!.error.stack?.includes("boom"));
    assert.equal(flaky!.retriable, true);
    assert.equal(fatal!.error.name, "NonRetriableError");
    assert.equal(fatal!.retriable, false);
  });

  it("throws a memoised error as a StepError the handler can catch", async () => {
    const r = await call(async ({ step }) => {
      try {
        await step.run("unstable", () => "never runs");
        return "ok";
      } catch (e) {
        assert.ok(e instanceof StepError);
        return { stepId: e.stepId, name: e.error.name, message: e.message };
      }
    }, { unstable: { error: { name: "TypeError", message: "boom" } } });
    assert.deepEqual(r, { status: "done", output: { stepId: "unstable", name: "TypeError", message: "boom" } });
  });

  it("fails the run, without retries, on an uncaught StepError", async () => {
    const r = await call(({ step }) => step.run("unstable", () => 1), { unstable: { error: { name: "Error", message: "boom" } } });
    assert.equal(r.status, "error");
    const e = r as Extract<CallReply, { status: "error" }>;
    assert.equal(e.retriable, false);
    assert.equal(e.error.message, "boom");
    assert.equal(e.error.step, "unstable");
  });

  it("reports function errors; NonRetriableError isn't retried", async () => {
    const r = await call(() => {
      throw new Error("kaput");
    });
    assert.equal(r.status, "error");
    assert.equal((r as { retriable: boolean }).retriable, true);
    assert.equal((r as { error: { message: string } }).error.message, "kaput");
    const n = await call(async () => {
      throw new NonRetriableError("fatal");
    });
    assert.deepEqual({ ...(n as { error: object }).error, stack: undefined }, { name: "NonRetriableError", message: "fatal", stack: undefined });
    assert.equal((n as { retriable: boolean }).retriable, false);
    const s = await call(() => Promise.reject("a string"));
    assert.deepEqual(s, { status: "error", error: { name: "Error", message: "a string" }, retriable: true });
  });

  it("reports a step before an error that happened alongside it", async () => {
    const r = await call(({ step }) => Promise.all([step.run("a", () => 1), Promise.reject(new Error("x"))]));
    assert.deepEqual(steps(r), [{ op: "run", id: "a", name: "a", data: 1 }]);
  });

  it("sleeps for a duration or until a time", async () => {
    const r = await call(async ({ step }) => {
      await Promise.all([step.sleep("nap", "30s"), step.sleepUntil("alarm", new Date(1_900_000_000_000))]);
    });
    assert.deepEqual(steps(r), [
      { op: "sleep", id: "nap", name: "nap", duration_ms: 30_000 },
      { op: "sleep", id: "alarm", name: "alarm", until: 1_900_000_000_000 },
    ]);
    const done = await call(async ({ step }) => {
      const v = await step.sleep("nap", "30s");
      return { v: v === undefined };
    }, { nap: { data: null } });
    assert.deepEqual(done, { status: "done", output: { v: true } });
  });

  it("waits for events: the event, or null on timeout", async () => {
    const handler = async ({ step }: FunctionContext) => {
      const paid = await step.waitForEvent("paid", { event: "invoice.paid", match: "data.invoice", timeout: "1h" });
      const signed = await step.waitForEvent("signed", { event: "deal.signed", if: "async.data.amount > 5", timeout: 1000 });
      return { paid: paid?.data ?? null, signed };
    };
    assert.deepEqual(steps(await call(handler)), [
      { op: "wait_for_event", id: "paid", name: "paid", event: "invoice.paid", match: "data.invoice", timeout_ms: 3_600_000 },
    ]);
    const paid = { id: "evt_2", name: "invoice.paid", data: { invoice: "A" }, user: null, ts: 2 };
    assert.deepEqual(steps(await call(handler, { paid: { data: paid } })), [
      { op: "wait_for_event", id: "signed", name: "signed", event: "deal.signed", if: "async.data.amount > 5", timeout_ms: 1000 },
    ]);
    assert.deepEqual(await call(handler, { paid: { data: paid }, signed: { data: null } }), {
      status: "done",
      output: { paid: { invoice: "A" }, signed: null },
    });
  });

  it("invokes functions: their output, or a StepError", async () => {
    const child = createFunction({ id: "child" }, () => 1);
    const handler = async ({ step }: FunctionContext) => {
      const a = await step.invoke<number>("a", { function: child, data: { n: 1 }, timeout: "1m" });
      try {
        await step.invoke("b", { function: "other" });
      } catch (e) {
        return { a, b: (e as StepError).error };
      }
    };
    assert.deepEqual(steps(await call(handler)), [{ op: "invoke", id: "a", name: "a", function: "child", data: { n: 1 }, timeout_ms: 60_000 }]);
    assert.deepEqual(steps(await call(handler, { a: { data: 42 } })), [{ op: "invoke", id: "b", name: "b", function: "other", data: null }]);
    assert.deepEqual(await call(handler, { a: { data: 42 }, b: { error: { name: "InvokeTimeout", message: "too slow" } } }), {
      status: "done",
      output: { a: 42, b: { name: "InvokeTimeout", message: "too slow" } },
    });
  });

  it("sends events", async () => {
    const handler = async ({ step }: FunctionContext) =>
      step.sendEvent("emit", [{ name: "a", data: { x: 1 } }, { name: "b", id: "dedup", ts: new Date(5) }]);
    assert.deepEqual(steps(await call(handler)), [
      { op: "send_event", id: "emit", name: "emit", events: [{ name: "a", data: { x: 1 } }, { name: "b", data: null, id: "dedup", ts: 5 }] },
    ]);
    assert.deepEqual(await call(handler, { emit: { data: { ids: ["evt_a", "evt_b"] } } }), { status: "done", output: { ids: ["evt_a", "evt_b"] } });
  });

  it("reports mixed step kinds found together", async () => {
    const r = await call(({ step }) => Promise.all([step.run("work", () => "w"), step.sleep("pause", 1000), step.sendEvent("tell", { name: "t" })]));
    assert.deepEqual(steps(r).map((s) => s.op), ["run", "sleep", "send_event"]);
  });

  it("reports steps a handler started but didn't await", async () => {
    const r = await call(({ step }) => {
      void step.run("background", () => 1);
      return "returned";
    });
    assert.deepEqual(steps(r), [{ op: "run", id: "background", name: "background", data: 1 }]);
  });

  it("ignores steps found after the call was answered", async () => {
    let late: Promise<unknown> | undefined;
    const r = await call(async ({ step }) => {
      late = delay(50).then(() => step.run("late", () => 2));
      await step.run("first", () => 1);
    });
    assert.deepEqual(steps(r).map((s) => s.id), ["first"]);
    await delay(80);
    assert.ok(late);
  });

  it("rejects steps without ids", async () => {
    const r = await call(({ step }) => step.run("", () => 1));
    assert.equal(r.status, "error");
  });
});
