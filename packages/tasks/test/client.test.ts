import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { Tasks, TasksError, createFunction } from "../src/index.js";

interface Captured {
  method: string;
  url: URL;
  headers: Record<string, string>;
  body: unknown;
}

function fake(respond: (c: Captured) => { status?: number; body?: unknown } = () => ({ body: {} })) {
  const calls: Captured[] = [];
  const f = (async (input: URL | RequestInfo, init?: RequestInit) => {
    const c: Captured = {
      method: init?.method ?? "GET",
      url: new URL(String(input)),
      headers: Object.fromEntries(Object.entries((init?.headers as Record<string, string>) ?? {})),
      body: init?.body ? JSON.parse(String(init.body)) : undefined,
    };
    calls.push(c);
    const r = respond(c);
    const text = r.body === undefined ? "" : typeof r.body === "string" ? r.body : JSON.stringify(r.body);
    return new Response(text, { status: r.status ?? 200 });
  }) as typeof fetch;
  return { calls, fetch: f };
}

describe("client", () => {
  it("maps publish options to the wire format", async () => {
    const { calls, fetch } = fake(() => ({ status: 201, body: { message_id: "msg_1", messages: [{ message_id: "msg_1", url: "https://x.test" }] } }));
    const tasks = new Tasks({ baseUrl: "http://api.test/", token: "secret", fetch });
    const out = await tasks.publish({
      url: "https://x.test",
      body: { hello: "world" },
      headers: { "x-a": "1" },
      method: "PUT",
      delay: "5m",
      notBefore: new Date(1_800_000_000_000),
      retries: 2,
      backoff: { strategy: "fixed", base: "100ms", max: "1s", jitter: 0 },
      timeout: "10s",
      callback: "https://cb.test/ok",
      failureCallback: "https://cb.test/fail",
      dedupId: "d1",
      contentDedup: true,
      flow: { key: "acct", parallelism: 2, rate: { limit: 5, period: "1s" } },
      queue: "q",
      group: "g",
      priority: 10,
      label: "l",
      contentType: "application/vnd.x+json",
    });
    assert.deepEqual(out, { messageId: "msg_1", messages: [{ messageId: "msg_1", url: "https://x.test" }] });
    const c = calls[0]!;
    assert.equal(c.method, "POST");
    assert.equal(c.url.href, "http://api.test/v1/tasks/publish");
    assert.equal(c.headers.authorization, "Bearer secret");
    assert.equal(c.headers["content-type"], "application/json");
    assert.deepEqual(c.body, {
      url: "https://x.test",
      queue: "q",
      method: "PUT",
      headers: { "x-a": "1" },
      body: { hello: "world" },
      content_type: "application/vnd.x+json",
      delay: 300_000,
      not_before: 1_800_000_000_000,
      retries: 2,
      backoff: { strategy: "fixed", base_ms: 100, max_ms: 1000, jitter: 0 },
      timeout: 10_000,
      callback: "https://cb.test/ok",
      failure_callback: "https://cb.test/fail",
      dedup_id: "d1",
      content_dedup: true,
      flow: { key: "acct", parallelism: 2, rate: { limit: 5, period: 1000 } },
      group: "g",
      priority: 10,
      label: "l",
    });
  });

  it("sends text as is and bytes as base64", async () => {
    const { calls, fetch } = fake(() => ({ body: { message_id: "m", messages: [] } }));
    const tasks = new Tasks({ baseUrl: "http://api.test", fetch });
    await tasks.publish({ topic: "t", body: "plain text" });
    await tasks.publish({ topic: "t", body: new Uint8Array([0, 1, 2, 255]) });
    await tasks.publish({ topic: "t", body: new Uint8Array([104, 105]).buffer });
    await tasks.publish({ topic: "t", flow: { key: "k", rate: "10/s" } });
    assert.deepEqual(calls.map((c) => c.body), [
      { topic: "t", body: "plain text" },
      { topic: "t", body_base64: "AAEC/w==" },
      { topic: "t", body_base64: "aGk=" },
      { topic: "t", flow: { key: "k", rate: "10/s" } },
    ]);
    assert.equal(calls[0]!.headers.authorization, undefined);
  });

  it("camelCases responses but leaves user data alone", async () => {
    const run = {
      id: "run_1",
      function_id: "f",
      events: [{ id: "e", name: "n", data: { user_id: 1, nested: { some_key: true } }, user: { org_id: 2 }, ts: 1 }],
      output: { total_cents: 5 },
      error: { name: "E", message: "m", extra_info: 1 },
      step_attempts: { "charge_card:1": 2 },
      steps: [{ run_id: "run_1", id: "a_b", op: "run", data: { snake_case: 1 }, at: 1, attempts: 1 }],
      pending: [{ step_id: "s", op: "sleep", child_run: "c" }],
    };
    const { fetch } = fake(() => ({ body: run }));
    const got = (await new Tasks({ baseUrl: "http://api.test", fetch }).runs.get("run_1")) as unknown as Record<string, unknown>;
    assert.deepEqual(got, {
      id: "run_1",
      functionId: "f",
      events: [{ id: "e", name: "n", data: { user_id: 1, nested: { some_key: true } }, user: { org_id: 2 }, ts: 1 }],
      output: { total_cents: 5 },
      error: { name: "E", message: "m", extra_info: 1 },
      stepAttempts: { "charge_card:1": 2 },
      steps: [{ runId: "run_1", id: "a_b", op: "run", data: { snake_case: 1 }, at: 1, attempts: 1 }],
      pending: [{ stepId: "s", op: "sleep", childRun: "c" }],
    });
  });

  it("throws TasksError with the server's message", async () => {
    const { fetch } = fake((c) =>
      c.url.pathname.endsWith("/conflict") ? { status: 409, body: { error: "message m is already delivered" } } : { status: 502, body: "bad gateway" },
    );
    const tasks = new Tasks({ baseUrl: "http://api.test", fetch });
    await assert.rejects(tasks.request("GET", "/conflict"), (e: unknown) => {
      assert.ok(e instanceof TasksError);
      assert.equal(e.status, 409);
      assert.equal(e.message, "message m is already delivered");
      assert.deepEqual(e.body, { error: "message m is already delivered" });
      return true;
    });
    await assert.rejects(tasks.health(), (e: unknown) => e instanceof TasksError && e.status === 502 && e.message === "bad gateway");
  });

  it("reads its defaults from the environment", async () => {
    process.env.ROLL_TASKS_URL = "http://env.test:1234/v1/tasks";
    process.env.ROLL_TASKS_TOKEN = "from-env";
    try {
      const { calls, fetch } = fake();
      const tasks = new Tasks({ fetch });
      assert.equal(tasks.baseUrl, "http://env.test:1234");
      await tasks.stats();
      assert.equal(calls[0]!.url.href, "http://env.test:1234/v1/tasks/stats");
      assert.equal(calls[0]!.headers.authorization, "Bearer from-env");
    } finally {
      delete process.env.ROLL_TASKS_URL;
      delete process.env.ROLL_TASKS_TOKEN;
    }
    assert.equal(new Tasks({ fetch: fake().fetch }).baseUrl, "http://127.0.0.1:7070");
  });

  it("builds queries, paths and bodies for every resource", async () => {
    const { calls, fetch } = fake((c) => ({ body: c.url.pathname.endsWith("/pull") ? { messages: [] } : c.url.pathname.endsWith("/extend") ? { lease_until: 5 } : {} }));
    const t = new Tasks({ baseUrl: "http://api.test", fetch });
    const fn = createFunction({ id: "echo" }, () => 1);
    await t.messages.list({ state: "dlq", scheduleId: "s1", runId: "r", limit: 5, all: true });
    await t.messages.get("m/1");
    await t.messages.cancel("m1");
    await t.messages.cancelMany({ label: "bulk", scheduleId: "s" });
    await t.messages.cancelMany({ ids: ["a"] });
    await t.messages.retry("m1");
    await t.messages.purge("m1");
    await t.dlq.list({ queue: "q" });
    await t.dlq.retry("m1");
    await t.dlq.retryAll();
    await t.dlq.retryAll({ queue: "q" });
    await t.dlq.delete("m1");
    await t.queues.upsert("q", { parallelism: 0, rate: null, retries: null, timeout: "9s", partitions: 2 });
    await t.queues.delete("q", { force: true });
    assert.deepEqual(await t.queues.pull("q", { max: 3, visibility: "1m", wait: "2s" }), []);
    await t.queues.ack("q", ["r1", "r2"]);
    await t.queues.nack("q", "r1", { delay: "1s", error: "no" });
    assert.equal(await t.queues.extend("q", "r1", "30s"), 5);
    await t.topics.upsert("t", ["https://a.test", { url: "https://b.test", name: "b" }]);
    await t.topics.removeEndpoints("t", ["b"]);
    await t.schedules.create({ cron: "@every 1m", function: fn });
    await t.schedules.update("s", { tz: null, label: null, event: { name: "e" } });
    await t.events.send({ name: "e", ts: new Date(9) });
    await t.events.send([{ name: "a" }, { name: "b", id: "x" }]);
    await t.events.list({ name: "e", limit: 2 });
    await t.functions.invoke(fn, { data: { x: 1 }, wait: "5s", id: "once" });
    await t.functions.cancelRuns("echo", { if: "event.data.x == 1" });
    await t.runs.list({ function: "echo", status: "completed" });
    await t.apps.sync({ id: "app", url: "https://app.test/api", functions: [fn, { id: "raw", triggers: [] }] });
    await t.usage(7);
    const seen = calls.map((c) => `${c.method} ${c.url.pathname}${c.url.search} ${c.body === undefined ? "" : JSON.stringify(c.body)}`.trim());
    assert.deepEqual(seen, [
      "GET /v1/tasks/messages?state=dlq&schedule_id=s1&run_id=r&limit=5&all=true",
      "GET /v1/tasks/messages/m%2F1",
      "DELETE /v1/tasks/messages/m1",
      'POST /v1/tasks/messages/cancel {"label":"bulk","schedule_id":"s"}',
      'POST /v1/tasks/messages/cancel {"ids":["a"]}',
      "POST /v1/tasks/messages/m1/retry",
      "DELETE /v1/tasks/messages/m1?purge=true",
      "GET /v1/tasks/dlq?queue=q",
      'POST /v1/tasks/dlq/retry {"ids":["m1"]}',
      'POST /v1/tasks/dlq/retry {"all":true}',
      'POST /v1/tasks/dlq/retry {"queue":"q"}',
      "DELETE /v1/tasks/dlq/m1",
      'PUT /v1/tasks/queues/q {"parallelism":0,"rate":null,"partitions":2,"retries":null,"timeout":9000}',
      "DELETE /v1/tasks/queues/q?force=true",
      'POST /v1/tasks/queues/q/pull {"max":3,"visibility":60000,"wait":2000}',
      'POST /v1/tasks/queues/q/ack {"receipts":["r1","r2"]}',
      'POST /v1/tasks/queues/q/nack {"receipt":"r1","delay":1000,"error":"no"}',
      'POST /v1/tasks/queues/q/extend {"receipt":"r1","visibility":30000}',
      'PUT /v1/tasks/topics/t {"endpoints":["https://a.test",{"url":"https://b.test","name":"b"}]}',
      'DELETE /v1/tasks/topics/t/endpoints {"urls":["b"]}',
      'POST /v1/tasks/schedules {"cron":"@every 1m","function":"echo"}',
      'PATCH /v1/tasks/schedules/s {"tz":null,"event":{"name":"e"},"label":null}',
      'POST /v1/tasks/events {"name":"e","ts":9}',
      'POST /v1/tasks/events [{"name":"a"},{"name":"b","id":"x"}]',
      "GET /v1/tasks/events?name=e&limit=2",
      'POST /v1/tasks/functions/echo/invoke {"data":{"x":1},"id":"once","wait":5000}',
      'POST /v1/tasks/functions/echo/cancel {"if":"event.data.x == 1"}',
      "GET /v1/tasks/runs?function=echo&status=completed",
      `POST /v1/tasks/apps {"url":"https://app.test/api","id":"app","functions":[{"id":"echo","triggers":[]},{"id":"raw","triggers":[]}],"sdk":"roll-tasks-js/0.1.0"}`,
      "GET /v1/tasks/usage?days=7",
    ]);
  });

  it("keeps waiting for runs past the server's limit", async () => {
    let n = 0;
    const { calls, fetch } = fake(() => ({ body: { id: "r", status: ++n < 3 ? "running" : "completed" } }));
    const run = await new Tasks({ baseUrl: "http://api.test", fetch }).runs.wait("r", { timeout: "10m" });
    assert.equal(run.status, "completed");
    assert.equal(calls.length, 3);
    assert.equal(calls[0]!.url.searchParams.get("timeout"), "300000");
  });
});
