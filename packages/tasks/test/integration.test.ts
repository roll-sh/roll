// End to end against the real roll-tasks server (built with cargo). Set ROLL_TASKS_BIN to use a
// prebuilt binary; the suite is skipped when it can't be built.
import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";
import { setTimeout as delay } from "node:timers/promises";
import {
  NonRetriableError,
  Receiver,
  StepError,
  Tasks,
  TasksError,
  createFunction,
  deliveryInfo,
  serve,
  toNodeHandler,
} from "../src/index.js";
import { AppServer, type Server, eventually, serverBinary, startServer } from "./harness.js";

const binary = serverBinary();
const skip = binary.skip ?? false;

let server: Server;
let app: AppServer;
let tasks: Tasks;
let receiver: Receiver;
const calls = new Map<string, number>();
const counters = new Map<string, number>();
const bump = (k: string) => counters.set(k, (counters.get(k) ?? 0) + 1);
const count = (k: string) => counters.get(k) ?? 0;

// --- functions under test ---

const memo = createFunction({ id: "memo", trigger: { event: "it.memo" } }, async ({ event, step }) => {
  const a = await step.run("a", () => (bump("memo.a"), 1));
  bump("memo.between");
  const b = await step.run("b", () => (bump("memo.b"), 2));
  return { order: event.data.order, sum: a + b };
});

const sleeper = createFunction({ id: "sleeper" }, async ({ step }) => {
  const before = await step.run("before", () => Date.now());
  await step.sleep("nap", "1s");
  const after = await step.run("after", () => Date.now());
  return { slept: after - before };
});

const waiter = createFunction({ id: "waiter", trigger: { event: "it.order.created" } }, async ({ step }) => {
  const paid = await step.waitForEvent("paid", { event: "it.order.paid", match: "data.order", timeout: "1500ms" });
  return { paid: paid ? paid.data : null };
});

const child = createFunction({ id: "child" }, async ({ event, step }) => step.run("double", () => event.data.n * 2));
const failingChild = createFunction({ id: "failing-child", retries: 0 }, () => {
  throw new NonRetriableError("child says no");
});
const parent = createFunction({ id: "parent" }, async ({ step }) => {
  const a = await step.invoke<number>("a", { function: child, data: { n: 21 } });
  try {
    await step.invoke("b", { function: failingChild, data: {} });
    return { a, b: "unexpected" };
  } catch (e) {
    assert.ok(e instanceof StepError);
    return { a, b: e.message };
  }
});

const emitter = createFunction({ id: "emitter", trigger: { event: "it.emit" } }, async ({ step }) =>
  step.sendEvent("emit", { name: "it.emitted", data: { from: "emitter" } }),
);
const emitted = createFunction({ id: "emitted", trigger: { event: "it.emitted" } }, ({ event }) => event.data);

const parallel = createFunction({ id: "parallel" }, async ({ step }) => {
  const parts = await Promise.all(
    ["x", "y", "z"].map((k, i) =>
      step.run(`part-${k}`, async () => {
        bump(`parallel.${k}`);
        await delay(50);
        return i + 1;
      }),
    ),
  );
  return parts.reduce((a, b) => a + b, 0);
});

const flaky = createFunction({ id: "flaky", retries: 1 }, async ({ step }) => {
  try {
    await step.run("unstable", () => {
      bump("flaky.unstable");
      throw new Error("boom");
    });
    return "unexpected";
  } catch (e) {
    return { handled: (e as StepError).message, name: (e as StepError).error.name };
  }
});

const fragile = createFunction({ id: "fragile", retries: 3 }, async ({ step }) => {
  try {
    await step.run("once", () => {
      bump("fragile.once");
      throw new NonRetriableError("don't retry me");
    });
  } catch (e) {
    return { caught: e instanceof StepError ? e.error.name : "?" };
  }
});

const fatal = createFunction({ id: "fatal", retries: 3 }, () => {
  throw new NonRetriableError("fatal");
});

const echo = createFunction({ id: "echo" }, ({ event }) => event.data);

const reminder = createFunction(
  { id: "reminder", trigger: { event: "it.cart.updated" }, cancelOn: { event: "it.cart.checkout", match: "data.cart" } },
  async ({ step }) => {
    await step.sleep("wait", "60s");
    return "reminded";
  },
);

let active = 0;
let maxActive = 0;
const serial = createFunction({ id: "serial", trigger: { event: "it.job" }, concurrency: 1 }, async ({ event }) => {
  active++;
  maxActive = Math.max(maxActive, active);
  await delay(80);
  active--;
  return event.data.i;
});

const cron = createFunction({ id: "cron-fn", trigger: { cron: "* * * * * *" } }, ({ event }) => event.name);

const functions = [memo, sleeper, waiter, child, failingChild, parent, emitter, emitted, parallel, flaky, fragile, fatal, echo, reminder, serial, cron];

async function runsOf(fn: string, status?: string) {
  return (await tasks.runs.list({ function: fn, status: status as never, limit: 100 })).items;
}

before(async () => {
  if (skip) return;
  server = await startServer(binary.bin!);
  tasks = new Tasks({ baseUrl: server.url });
  const keys = await tasks.signingKeys.get();
  receiver = new Receiver({ secrets: [keys.current, keys.next] });
  app = await new AppServer().start();
  const handler = serve({ appId: "it", functions, signingKeys: [keys.current, keys.next] });
  const node = toNodeHandler(async (req) => {
    const fn = new URL(req.url).searchParams.get("fnId");
    if (req.method === "POST" && fn) calls.set(fn, (calls.get(fn) ?? 0) + 1);
    return handler(req);
  });
  app.app = (req, res) => void node(req, res);
  const synced = await tasks.apps.sync({ url: `${app.base}/app` });
  assert.equal(synced.id, "it");
  assert.equal(synced.functions.length, functions.length);
}, { timeout: 600_000 });

after(async () => {
  await app?.close();
  await server?.stop();
});

describe("messages", { skip, concurrency: true }, () => {
  it("publishes and delivers signed requests", async () => {
    const r = await tasks.publish({ url: app.url("hello"), body: { hello: "world" }, label: "greeting", headers: { "x-custom": "yes" } });
    assert.match(r.messageId, /^msg_/);
    const d = (await eventually("a delivery", () => app.received("hello")[0]))!;
    await receiver.verify({ headers: d.headers, body: d.body });
    assert.deepEqual(JSON.parse(String(d.body)), { hello: "world" });
    assert.equal(d.headers["content-type"], "application/json");
    assert.equal(d.headers["x-custom"], "yes");
    const info = deliveryInfo(d.headers);
    assert.equal(info.messageId, r.messageId);
    assert.equal(info.id, r.messageId);
    assert.equal(info.label, "greeting");
    assert.equal(info.retried, 0);
    // A tampered copy doesn't verify.
    assert.equal(await receiver.isValid({ headers: d.headers, body: Buffer.from('{"hello":"mallory"}') }), false);
    const m = await eventually("delivered", async () => {
      const m = await tasks.messages.get(r.messageId);
      return m.state === "delivered" && m;
    });
    assert.equal(m.body, '{"hello":"world"}');
    assert.equal(m.attemptsLog.length, 1);
    assert.equal(m.attemptsLog[0]!.status, 200);
    assert.equal(m.target.type, "http");
  });

  it("delivers text and binary bodies byte for byte", async () => {
    const bytes = new Uint8Array([0, 1, 2, 250, 255]);
    await tasks.publish({ url: app.url("binary"), body: bytes, contentType: "application/octet-stream" });
    await tasks.publish({ url: app.url("text"), body: "just text", method: "PUT" });
    const b = await eventually("binary", () => app.received("binary")[0]);
    assert.deepEqual(new Uint8Array(b.body), bytes);
    await receiver.verify({ headers: b.headers, body: b.body });
    const t = await eventually("text", () => app.received("text")[0]);
    assert.equal(String(t.body), "just text");
    assert.equal(t.method, "PUT");
  });

  it("retries, dead-letters, and retries from the DLQ", async () => {
    let healthy = false;
    app.behaviours.set("flaky", () => ({ status: healthy ? 200 : 500 }));
    const backoff = { strategy: "fixed" as const, base: 50, max: 50, jitter: 0 };
    const r = await tasks.publish({ url: app.url("flaky"), retries: 1, backoff, body: "x" });
    const failed = await eventually("dead-lettered", async () => {
      const m = await tasks.messages.get(r.messageId);
      return m.state === "failed" && m;
    });
    assert.equal(failed.attempts, 2);
    assert.equal(app.received("flaky").length, 2);
    assert.equal(app.received("flaky")[1]!.headers["roll-retried"], "1");
    const dlq = await tasks.dlq.list({ limit: 100 });
    assert.ok(dlq.items.some((m) => m.id === r.messageId));
    healthy = true;
    assert.deepEqual(await tasks.dlq.retry(r.messageId), { retried: 1 });
    await eventually("delivered after the DLQ retry", async () => (await tasks.messages.get(r.messageId)).state === "delivered");
    assert.ok(!(await tasks.dlq.list({ limit: 100 })).items.some((m) => m.id === r.messageId));
  });

  it("stops retrying on 489 or roll-non-retryable", async () => {
    app.behaviours.set("stop489", () => ({ status: 489 }));
    app.behaviours.set("stopheader", () => ({ status: 500, headers: { "roll-non-retryable": "true" } }));
    const a = await tasks.publish({ url: app.url("stop489"), retries: 5 });
    const b = await tasks.publish({ url: app.url("stopheader"), retries: 5 });
    for (const id of [a.messageId, b.messageId]) {
      const m = await eventually("failed", async () => {
        const m = await tasks.messages.get(id);
        return m.state === "failed" && m;
      });
      assert.equal(m.attempts, 1);
    }
    await tasks.dlq.delete(a.messageId);
    await assert.rejects(tasks.messages.get(a.messageId), (e: unknown) => e instanceof TasksError && e.status === 404);
  });

  it("honours Retry-After", async () => {
    app.behaviours.set("later", (_d, n) => (n === 1 ? { status: 503, headers: { "retry-after": "1" } } : {}));
    await tasks.publish({ url: app.url("later"), retries: 2, backoff: { strategy: "fixed", base: 10, max: 10, jitter: 0 } });
    const [first, second] = await eventually("two attempts", () => app.received("later").length >= 2 && app.received("later"), 5000);
    assert.ok(second!.at - first!.at >= 900, `retried after ${second!.at - first!.at}ms`);
  });

  it("delays, lists, cancels, retries and purges", async () => {
    const a = await tasks.publish({ url: app.url("delayed"), delay: "1m", label: "it-bulk" });
    const b = await tasks.publish({ url: app.url("delayed"), notBefore: new Date(Date.now() + 60_000), label: "it-bulk" });
    const listed = await tasks.messages.list({ label: "it-bulk", state: "pending" });
    assert.deepEqual(new Set(listed.items.map((m) => m.id)), new Set([a.messageId, b.messageId]));
    const canceled = await tasks.messages.cancel(a.messageId);
    assert.equal(canceled.state, "canceled");
    await assert.rejects(tasks.messages.cancel(a.messageId), (e: unknown) => e instanceof TasksError && e.status === 409);
    assert.deepEqual(await tasks.messages.cancelMany({ label: "it-bulk" }), { canceled: 1 });
    assert.deepEqual(await tasks.messages.cancelMany({ ids: [b.messageId] }), { canceled: 0 });
    // A canceled message can be sent again, now.
    const retried = await tasks.messages.retry(a.messageId);
    assert.equal(retried.state, "pending");
    await eventually("the retried delivery", () => app.received("delayed").length === 1);
    await eventually("delivered", async () => (await tasks.messages.get(a.messageId)).state === "delivered");
    await tasks.messages.purge(a.messageId);
    await assert.rejects(tasks.messages.get(a.messageId), (e: unknown) => e instanceof TasksError && e.status === 404);
    await assert.rejects(tasks.messages.cancelMany({}), (e: unknown) => e instanceof TasksError && e.status === 400);
  });

  it("deduplicates", async () => {
    const a = await tasks.publish({ url: app.url("dedup"), dedupId: "it-once", delay: "200ms" });
    const b = await tasks.publish({ url: app.url("dedup"), dedupId: "it-once" });
    assert.equal(b.messageId, a.messageId);
    assert.equal(b.messages[0]!.deduplicated, true);
    const c = await tasks.publish({ url: app.url("dedup"), body: { x: 1 }, contentDedup: true });
    const d = await tasks.publish({ url: app.url("dedup"), body: { x: 1 }, contentDedup: true });
    assert.equal(d.messageId, c.messageId);
    await eventually("two deliveries", () => app.received("dedup").length === 2);
  });

  it("publishes batches and reports errors per message", async () => {
    const out = await tasks.publishBatch([{ url: app.url("batch"), body: 1 }, { url: app.url("batch"), body: 2 }, { body: "nowhere" }]);
    assert.equal(out.length, 3);
    assert.match((out[0] as { messageId: string }).messageId, /^msg_/);
    assert.match((out[2] as { error: string }).error, /url, a topic, or a queue/);
    await eventually("both deliveries", () => app.received("batch").length === 2);
  });

  it("sends success callbacks", async () => {
    const r = await tasks.publish({ url: app.url("cb-source"), body: "payload", callback: app.url("cb") });
    const cb = await eventually("the callback", () => app.received("cb")[0]);
    assert.equal(cb.headers["roll-callback-for"], r.messageId);
    const body = JSON.parse(String(cb.body));
    assert.equal(body.message_id, r.messageId);
    assert.equal(body.state, "delivered");
    assert.equal(Buffer.from(body.source_body, "base64").toString(), "payload");
  });

  it("fans out through topics", async () => {
    const t = await tasks.topics.upsert("it-orders", [app.url("topic-a"), { url: app.url("topic-b"), name: "billing" }]);
    assert.equal(t.endpoints.length, 2);
    assert.equal((await tasks.topics.addEndpoints("it-orders", [app.url("topic-c"), app.url("topic-a")])).endpoints.length, 3);
    const r = await tasks.publish({ topic: "it-orders", body: { id: 7 } });
    assert.equal(r.messages.length, 3);
    for (const n of ["topic-a", "topic-b", "topic-c"]) {
      const d = await eventually(n, () => app.received(n)[0]);
      assert.equal(d.headers["roll-topic"], "it-orders");
      await receiver.verify({ headers: d.headers, body: d.body });
    }
    assert.equal((await tasks.topics.removeEndpoints("it-orders", ["billing"])).endpoints.length, 2);
    assert.ok((await tasks.topics.list()).some((x) => x.name === "it-orders"));
    assert.equal((await tasks.topics.get("it-orders")).endpoints.length, 2);
    await tasks.topics.delete("it-orders");
    await assert.rejects(tasks.topics.get("it-orders"), (e: unknown) => e instanceof TasksError && e.status === 404);
  });

  it("manages queues", async () => {
    const q = await tasks.queues.upsert("it-admin", { parallelism: 3, rate: "10/s", retries: 7, timeout: "9s" });
    assert.deepEqual([q.parallelism, q.rate, q.retries, q.timeoutMs], [3, { limit: 10, periodMs: 1000 }, 7, 9000]);
    const cleared = await tasks.queues.upsert("it-admin", { parallelism: 0, rate: null, timeout: null });
    assert.equal(cleared.parallelism, undefined);
    assert.equal(cleared.rate, undefined);
    assert.equal(cleared.timeoutMs, undefined);
    const got = await tasks.queues.get("it-admin");
    assert.deepEqual(got.stats, { ready: 0, scheduled: 0, active: 0, inflight: 0 });
    assert.ok((await tasks.queues.list()).some((x) => x.name === "it-admin"));
    assert.equal((await tasks.queues.pause("it-admin")).paused, true);
    // Paused queues hold their messages.
    await tasks.publish({ url: app.url("queued"), queue: "it-admin" });
    await delay(300);
    assert.equal(app.received("queued").length, 0);
    assert.equal((await tasks.queues.resume("it-admin")).paused, false);
    await eventually("the held message", () => app.received("queued").length === 1);
    assert.deepEqual(await tasks.queues.delete("it-admin"), { canceled: 0 });
    await assert.rejects(tasks.queues.get("it-admin"), (e: unknown) => e instanceof TasksError && e.status === 404);
  });

  it("pulls, acks, nacks and extends by hand", async () => {
    await tasks.publish({ queue: "it-manual", body: "one" });
    await tasks.publish({ queue: "it-manual", body: { n: 2 } });
    const got = await tasks.queues.pull("it-manual", { max: 10, visibility: "2s" });
    assert.deepEqual(got.map((m) => m.body).sort(), ["one", '{"n":2}']);
    assert.ok(got.every((m) => m.receipt && m.attempts === 1 && m.leaseUntil > Date.now()));
    assert.deepEqual(await tasks.queues.pull("it-manual"), []);
    assert.deepEqual(await tasks.queues.ack("it-manual", got[0]!.receipt), { acked: 1, errors: [] });
    await assert.rejects(tasks.queues.ack("it-manual", got[0]!.receipt), (e: unknown) => e instanceof TasksError && e.status === 409);
    const lease = await tasks.queues.extend("it-manual", got[1]!.receipt, "5s");
    assert.ok(lease > Date.now() + 4000);
    const n = await tasks.queues.nack("it-manual", got[1]!.receipt, { delay: 0, error: "try again" });
    assert.equal(n.state, "pending");
    const again = await tasks.queues.pull("it-manual", { wait: "2s" });
    assert.equal(again.length, 1);
    assert.equal(again[0]!.attempts, 2);
    await tasks.queues.ack("it-manual", [again[0]!.receipt]);

    // Dead on arrival: straight to the dead-letter queue, retries left or not.
    const poison = await tasks.publish({ queue: "it-manual", body: "poison", retries: 5 });
    const [p] = await tasks.queues.pull("it-manual");
    const dead = await tasks.queues.nack("it-manual", p!.receipt, { error: "unparseable", dead: true });
    assert.equal(dead.state, "failed");
    assert.equal((await tasks.messages.get(poison.messageId)).reason, "unparseable");
  });

  it("consumes a queue with concurrency, retries and lease extension", async () => {
    for (let n = 1; n <= 6; n++) await tasks.publish({ queue: "it-jobs", body: { n } });
    const done = new Set<number>();
    const attempts = new Map<number, number>();
    let inFlight = 0;
    let peak = 0;
    const stop = new AbortController();
    const errors: unknown[] = [];
    const consumer = tasks.consume(
      "it-jobs",
      async (msg) => {
        const { n } = msg.json<{ n: number }>();
        attempts.set(n, (attempts.get(n) ?? 0) + 1);
        inFlight++;
        peak = Math.max(peak, inFlight);
        try {
          // Longer than the lease: only works because the consumer extends it.
          await delay(n === 6 ? 1600 : 50);
          if (n === 3 && attempts.get(n) === 1) throw new Error("first try fails");
          done.add(n);
          if (done.size === 6) stop.abort();
        } finally {
          inFlight--;
        }
      },
      { concurrency: 3, visibility: "1s", wait: "1s", nackDelay: 0, signal: stop.signal, onError: (e) => errors.push(e) },
    );
    await Promise.race([consumer, delay(10_000).then(() => stop.abort())]);
    await consumer;
    assert.equal(done.size, 6, `handled ${[...done]}; errors ${errors.map(String)}`);
    assert.equal(attempts.get(3), 2);
    assert.equal(attempts.get(6), 1, "the lease was extended");
    assert.ok(peak <= 3 && peak >= 2, `peak ${peak}`);
    assert.deepEqual(errors, []);
    assert.equal((await tasks.queues.get("it-jobs")).stats!.ready, 0);
  });

  it("dead-letters a consumed message on NonRetriableError", async () => {
    const { messageId } = await tasks.publish({ queue: "it-poison", body: "x", retries: 5 });
    const stop = new AbortController();
    let calls = 0;
    const consumer = tasks.consume(
      "it-poison",
      () => {
        calls++;
        stop.abort();
        throw new NonRetriableError("can't parse");
      },
      { wait: "1s", signal: stop.signal },
    );
    await Promise.race([consumer, delay(5_000).then(() => stop.abort())]);
    await consumer;
    assert.equal(calls, 1);
    const m = await tasks.messages.get(messageId);
    assert.equal(m.state, "failed");
    assert.equal(m.reason, "can't parse");
  });

  it("fires schedules", async () => {
    const s = await tasks.schedules.create({ id: "it-heartbeat", cron: "* * * * * *", publish: { url: app.url("tick"), body: { beat: true } }, label: "hb" });
    assert.equal(s.id, "it-heartbeat");
    assert.equal(s.upcoming?.length, 5);
    const d = await eventually("a scheduled delivery", () => app.received("tick")[0], 4000);
    assert.equal(d.headers["roll-schedule-id"], "it-heartbeat");
    assert.equal(d.headers["roll-label"], "hb");
    await receiver.verify({ headers: d.headers, body: d.body });
    const paused = await tasks.schedules.pause("it-heartbeat");
    assert.equal(paused.paused, true);
    assert.equal(paused.upcoming, undefined);
    const fired = await tasks.schedules.trigger("it-heartbeat");
    assert.equal(fired.fired.length, 1);
    const updated = await tasks.schedules.update("it-heartbeat", { cron: "@every 1h", label: null });
    assert.equal(updated.cron, "@every 1h");
    assert.equal(updated.label, undefined);
    assert.equal((await tasks.schedules.resume("it-heartbeat")).paused, false);
    assert.ok((await tasks.schedules.list()).some((x) => x.id === "it-heartbeat"));
    assert.equal((await tasks.schedules.get("it-heartbeat")).target.type, "publish");
    await tasks.schedules.delete("it-heartbeat");
    await assert.rejects(tasks.schedules.get("it-heartbeat"), (e: unknown) => e instanceof TasksError && e.status === 404);

    await tasks.schedules.create({ id: "it-sched-event", cron: "* * * * * *", event: { name: "it.sched.tick", data: { n: 1 } } });
    const ev = await eventually("a scheduled event", async () => (await tasks.events.list({ name: "it.sched.tick" })).items[0], 4000);
    assert.deepEqual(ev.data, { n: 1 });
    await tasks.schedules.delete("it-sched-event");
    await assert.rejects(tasks.schedules.create({ cron: "never", event: { name: "x" } }), (e: unknown) => e instanceof TasksError && e.status === 400);
  });

  it("sends roll-schedule-id once on scheduled deliveries", async () => {
    await tasks.schedules.create({ id: "it-once-header", cron: "* * * * * *", publish: { url: app.url("tick-once") } });
    try {
      const d = await eventually("a scheduled delivery", () => app.received("tick-once")[0], 4000);
      assert.equal(d.headers["roll-schedule-id"], "it-once-header");
    } finally {
      await tasks.schedules.delete("it-once-header");
    }
  });

  it("sends, lists and gets events", async () => {
    const { ids } = await tasks.events.send([
      { name: "it.plain", data: { a: 1 } },
      { name: "it.plain", data: { a: 2 }, id: "it-dedup" },
      { name: "it.plain", data: { a: 3 }, id: "it-dedup" },
    ]);
    assert.equal(ids.length, 3);
    assert.equal(ids[2], ids[1], "deduplicated");
    const one = await tasks.events.send({ name: "it.single", user: { id: "u1" } });
    const e = await tasks.events.get(one.ids[0]!);
    assert.deepEqual(e.user, { id: "u1" });
    assert.equal((await tasks.events.list({ name: "it.plain" })).items.length, 2);
  });
});

describe("workflows", { skip, concurrency: true }, () => {
  it("memoises steps: side effects run exactly once", async () => {
    const { ids } = await tasks.events.send({ name: "it.memo", data: { order: 7 } });
    const run = await eventually("a memo run", async () => (await runsOf("memo"))[0]);
    const done = await tasks.runs.wait(run.id, { timeout: "8s" });
    assert.equal(done.status, "completed");
    assert.deepEqual(done.output, { order: 7, sum: 3 });
    assert.equal(count("memo.a"), 1);
    assert.equal(count("memo.b"), 1);
    assert.equal(count("memo.between"), 2, "code between steps re-runs on replay");
    assert.equal(calls.get("memo"), 3, "one call per step plus the last");
    const detail = await tasks.runs.get(run.id);
    assert.deepEqual(detail.steps.map((s) => [s.id, s.data]), [["a", 1], ["b", 2]]);
    assert.deepEqual((await tasks.events.get(ids[0]!)).runs, [run.id]);
  });

  it("sleeps", async () => {
    const { runId } = await tasks.functions.invoke(sleeper);
    await eventually("the run to sleep", async () => (await tasks.runs.get(runId)).status === "waiting");
    assert.equal((await tasks.runs.get(runId)).pending?.[0]?.op, "sleep");
    const run = await tasks.runs.wait<{ slept: number }>(runId, { timeout: "8s" });
    assert.equal(run.status, "completed");
    assert.ok(run.output!.slept >= 900, `slept ${run.output!.slept}ms`);
  });

  it("waits for a matching event, or times out with null", async () => {
    await tasks.events.send([
      { name: "it.order.created", data: { order: "A" } },
      { name: "it.order.created", data: { order: "B" } },
    ]);
    await eventually("two waiting runs", async () => (await runsOf("waiter", "waiting")).length === 2);
    await tasks.events.send({ name: "it.order.paid", data: { order: "Z" } });
    await tasks.events.send({ name: "it.order.paid", data: { order: "A", amount: 10 } });
    const runs = await runsOf("waiter");
    const by = (o: string) => runs.find((r) => (r.events[0]!.data as { order: string }).order === o)!.id;
    const a = await tasks.runs.wait(by("A"), { timeout: "5s" });
    assert.deepEqual(a.output, { paid: { order: "A", amount: 10 } });
    const b = await tasks.runs.wait(by("B"), { timeout: "5s" });
    assert.equal(b.status, "completed");
    assert.deepEqual(b.output, { paid: null });
  });

  it("invokes functions and surfaces child failures", async () => {
    const run = await tasks.functions.invoke(parent, { wait: "10s" });
    assert.equal(run.status, "completed", JSON.stringify(run));
    assert.deepEqual(run.output, { a: 42, b: "child says no" });
    const kid = (await runsOf("child"))[0]!;
    assert.equal(kid.parent?.[0], run.id);
    assert.equal(kid.events[0]!.name, "roll/function.invoked");
  });

  it("sends events that start other functions, once", async () => {
    await tasks.events.send({ name: "it.emit" });
    const r = await eventually("the emitted run", async () => (await runsOf("emitted", "completed"))[0]);
    assert.deepEqual(r.output, { from: "emitter" });
    const first = (await runsOf("emitter"))[0]!;
    const done = await tasks.runs.wait<{ ids: string[] }>(first.id, { timeout: "5s" });
    assert.equal(done.output!.ids.length, 1);
    await delay(200);
    assert.equal((await runsOf("emitted")).length, 1);
  });

  it("runs parallel steps in one call", async () => {
    const run = await tasks.functions.invoke(parallel, { wait: "10s" });
    assert.equal(run.output, 6);
    assert.equal(calls.get("parallel"), 2, "three steps reported together");
    for (const k of ["x", "y", "z"]) assert.equal(count(`parallel.${k}`), 1);
  });

  it("retries a failing step, then hands the error to the function", async () => {
    const run = await tasks.functions.invoke(flaky, { wait: "15s" });
    assert.equal(run.status, "completed", JSON.stringify(run));
    assert.deepEqual(run.output, { handled: "boom", name: "Error" });
    assert.equal(count("flaky.unstable"), 2, "first try + 1 retry");
    const detail = await tasks.runs.get(run.id);
    assert.equal(detail.steps.find((s) => s.id === "unstable")?.attempts, 2);
  });

  it("doesn't retry NonRetriableError in steps or functions", async () => {
    const run = await tasks.functions.invoke(fragile, { wait: "10s" });
    assert.deepEqual(run.output, { caught: "NonRetriableError" });
    assert.equal(count("fragile.once"), 1);
    const failed = await tasks.functions.invoke(fatal, { wait: "10s" });
    assert.equal(failed.status, "failed");
    assert.equal(failed.error?.name, "NonRetriableError");
    assert.equal(failed.error?.message, "fatal");
    assert.equal(calls.get("fatal"), 1);
  });

  it("invokes with wait, reruns and lists runs", async () => {
    const run = await tasks.functions.invoke(echo, { data: { x: 1 }, wait: "10s" });
    assert.equal(run.status, "completed");
    assert.deepEqual(run.output, { x: 1 });
    const again = await tasks.runs.rerun(run.id);
    assert.notEqual(again.id, run.id);
    assert.deepEqual((await tasks.runs.wait(again.id, { timeout: "5s" })).output, { x: 1 });
    const a = await tasks.functions.invoke("echo", { data: 1, id: "it-once" });
    const b = await tasks.functions.invoke("echo", { data: 2, id: "it-once" });
    assert.equal(a.runId, b.runId);
    await tasks.runs.wait(a.runId, { timeout: "5s" });
    assert.equal((await runsOf("echo", "completed")).length, 3);
    await assert.rejects(tasks.functions.invoke("missing"), (e: unknown) => e instanceof TasksError && e.status === 404);
  });

  it("cancels runs by event, by hand and in bulk", async () => {
    await tasks.events.send([
      { name: "it.cart.updated", data: { cart: 1 } },
      { name: "it.cart.updated", data: { cart: 2 } },
    ]);
    await eventually("two sleeping runs", async () => (await runsOf("reminder", "waiting")).length === 2);
    await tasks.events.send({ name: "it.cart.checkout", data: { cart: 1 } });
    const runs = await runsOf("reminder");
    const by = (c: number) => runs.find((r) => (r.events[0]!.data as { cart: number }).cart === c)!.id;
    const one = await tasks.runs.wait(by(1), { timeout: "5s" });
    assert.equal(one.status, "canceled");
    assert.match(String(one.error?.message), /it\.cart\.checkout/);
    assert.equal((await tasks.runs.get(by(2))).status, "waiting");
    assert.equal((await tasks.runs.cancel(by(2))).status, "canceled");
    await assert.rejects(tasks.runs.cancel(by(2)), (e: unknown) => e instanceof TasksError && e.status === 409);

    await tasks.events.send([
      { name: "it.cart.updated", data: { cart: 3 } },
      { name: "it.cart.updated", data: { cart: 4 } },
    ]);
    await eventually("two new sleeping runs", async () => (await runsOf("reminder", "waiting")).length === 2);
    assert.deepEqual(await tasks.functions.cancelRuns(reminder, { if: "event.data.cart == 3" }), { canceled: 1 });
    assert.deepEqual(await tasks.functions.cancelRuns("reminder"), { canceled: 1 });
    assert.ok((await runsOf("reminder")).every((r) => r.status === "canceled"));
  });

  it("limits concurrency", async () => {
    await tasks.events.send([0, 1, 2, 3].map((i) => ({ name: "it.job", data: { i } })));
    await eventually("four runs", async () => (await runsOf("serial", "completed")).length === 4, 10_000);
    assert.equal(maxActive, 1);
  });

  it("runs cron triggers and pauses functions", async () => {
    const r = await eventually("a cron run", async () => (await runsOf("cron-fn", "completed"))[0], 5000);
    assert.equal(r.output, "roll/scheduled.timer");
    const sched = await tasks.schedules.get("fn-cron-fn-0");
    assert.deepEqual(sched.target, { type: "function", functionId: "cron-fn" });
    const paused = await tasks.functions.pause(cron);
    assert.equal(paused.paused, true);
    assert.equal((await tasks.schedules.get("fn-cron-fn-0")).paused, true);
    const f = await tasks.functions.get("cron-fn");
    assert.equal(f.paused, true);
    assert.equal(f.appId, "it");
    assert.deepEqual(f.triggers, [{ cron: "* * * * * *" }]);
    // Paused functions queue runs until resumed.
    const { runId } = await tasks.functions.invoke(cron);
    await delay(300);
    assert.equal((await tasks.runs.get(runId)).status, "queued");
    await tasks.functions.resume(cron);
    assert.equal((await tasks.runs.wait(runId, { timeout: "5s" })).status, "completed");
    await tasks.functions.pause(cron);
  });
});

describe("admin", { skip }, () => {
  it("lists functions and apps; pushes and deletes apps", async () => {
    const fns = await tasks.functions.list();
    assert.equal(fns.length, functions.length);
    const apps = await tasks.apps.list();
    assert.equal(apps.find((a) => a.id === "it")?.sdk, "roll-tasks-js/0.1.0");
    const pushed = await tasks.apps.sync({ id: "it-pushed", url: `${app.base}/app`, functions: [createFunction({ id: "pushed-fn", trigger: { event: "p" } }, () => 1)] });
    assert.equal(pushed.functions[0]!.id, "pushed-fn");
    assert.equal((await tasks.apps.list()).length, 2);
    await assert.rejects(
      tasks.apps.sync({ id: "thief", url: `${app.base}/app`, functions: [echo] }),
      (e: unknown) => e instanceof TasksError && e.status === 409,
    );
    await tasks.apps.delete("it-pushed");
    await assert.rejects(tasks.functions.get("pushed-fn"), (e: unknown) => e instanceof TasksError && e.status === 404);
  });

  it("reports health, stats and usage", async () => {
    const h = await tasks.health();
    assert.equal(h.ok, true);
    assert.equal(h.auth, false);
    const s = await tasks.stats();
    assert.equal(s.functions, functions.length);
    assert.ok(Array.isArray(s.queues) && s.nodes.length >= 1);
    const u = await tasks.usage(1);
    assert.equal(u.period.days, 1);
    assert.ok(u.total.published > 0 && u.total.runs > 0 && u.total.events > 0);
  });

  it("manages API keys and enforces tokens", async () => {
    const secured = await startServer(binary.bin!, ["--token", "admin-secret"]);
    try {
      const anon = new Tasks({ baseUrl: secured.url, token: "" });
      await assert.rejects(anon.queues.list(), (e: unknown) => e instanceof TasksError && e.status === 401 && /token/.test(e.message));
      assert.equal((await anon.health()).auth, true);
      const admin = new Tasks({ baseUrl: secured.url, token: "admin-secret" });
      const key = await admin.apiKeys.create("ci");
      assert.match(key.token, /^rt_/);
      assert.equal(key.prefix, key.token.slice(0, 7));
      assert.deepEqual((await admin.apiKeys.list()).map((k) => k.name), ["ci"]);
      const ci = new Tasks({ baseUrl: secured.url, token: key.token });
      assert.deepEqual(await ci.queues.list(), []);
      await admin.apiKeys.delete(key.id);
      await assert.rejects(ci.queues.list(), (e: unknown) => e instanceof TasksError && e.status === 401);
    } finally {
      await secured.stop();
    }
  });

  it("rotates signing keys without breaking receivers that trust the next key", async () => {
    const before = await tasks.signingKeys.get();
    const after = await tasks.signingKeys.rotate();
    assert.equal(after.current, before.next);
    assert.notEqual(after.next, before.next);
    const r = await tasks.publish({ url: app.url("rotated"), body: "after rotation" });
    const d = await eventually("a delivery", () => app.received("rotated")[0]);
    assert.equal(d.headers["webhook-id"], r.messageId);
    // The old receiver (current + next before rotation) still verifies; the new current alone does too.
    await receiver.verify({ headers: d.headers, body: d.body });
    await new Receiver({ secrets: after.current }).verify({ headers: d.headers, body: d.body });
    assert.equal(await new Receiver({ secrets: before.current }).isValid({ headers: d.headers, body: d.body }), false);
  });
});

