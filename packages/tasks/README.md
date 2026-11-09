# @roll/tasks

TypeScript SDK for **roll tasks**: durable HTTP messages, queues, schedules, events and workflows.

- Zero runtime dependencies. Uses global `fetch` and Web Crypto, so it runs on Node 18+, Bun, Deno, Cloudflare Workers and Vercel / Next.js route handlers.
- ESM only, with types.

```ts
import { Tasks } from "@roll/tasks";

const tasks = new Tasks(); // ROLL_TASKS_URL (default http://127.0.0.1:7070), ROLL_TASKS_TOKEN
```

You write options in camelCase with human durations (`"500ms"`, `"30s"`, `"5m"`, `"2h"`, `"7d"` or a number of milliseconds), and the SDK converts them to the API's snake_case. Responses come back in camelCase. User payloads are returned exactly as stored: `data`, `output`, `error`, `user`, `headers` and `body`. API errors throw `TasksError` with `status` and `message`.

## Messages

```ts
await tasks.publish({
  url: "https://example.com/hooks/welcome",
  body: { userId: 42 },        // strings as is, Uint8Array as bytes, anything else as JSON
  delay: "10m",                // or notBefore: new Date(...)
  retries: 5,
  backoff: { strategy: "exponential", base: "2s", max: "1h" },
  timeout: "30s",
  dedupId: "welcome-42",       // or contentDedup: true
  callback: "https://example.com/hooks/delivered",
  failureCallback: "https://example.com/hooks/failed",
  flow: { key: "acct_1", parallelism: 2, rate: "10/s" },
  label: "welcome",
});

await tasks.publish({ topic: "orders", body: { id: 7 } });              // fan out
await tasks.publish({ url, queue: "emails", group: "user-42" });          // FIFO per group
await tasks.publishBatch([{ url, body: 1 }, { url, body: 2 }]);

await tasks.messages.list({ state: "pending", label: "welcome" });
await tasks.messages.get(id);          // with attemptsLog and body
await tasks.messages.cancel(id);
await tasks.messages.cancelMany({ label: "welcome" });
await tasks.messages.retry(id);        // failed or canceled ones
await tasks.messages.purge(id);        // delete a finished one

await tasks.dlq.list();
await tasks.dlq.retry([id]);
await tasks.dlq.retryAll({ queue: "emails" });
await tasks.dlq.delete(id);
```

### Receiving

Every delivery is signed with [Standard Webhooks](https://www.standardwebhooks.com/), using both the current and the next key. Verify the raw body:

```ts
import { Receiver, deliveryInfo } from "@roll/tasks";

const receiver = new Receiver(); // ROLL_TASKS_SIGNING_KEY (+ _NEXT), or { secrets: [current, next] }

export async function POST(req: Request) {
  const body = await receiver.verifyRequest(req); // throws SignatureError
  const { messageId, retried, queue, topic, scheduleId, label } = deliveryInfo(req.headers);
  // ...
  return new Response("ok");
}
```

You can also call `verifySignature({ secrets, headers, body, toleranceSeconds = 300 })` directly. `tasks.signingKeys.get()` returns the secrets, and `tasks.signingKeys.rotate()` rotates them.

How your response steers retries:

- **2xx** means delivered. Anything else is retried with backoff.
- **489** stops retries: the message goes straight to the dead-letter queue. The `roll-non-retryable: true` header does the same.
- `Retry-After` (seconds or an HTTP date) sets the delay before the next attempt.

## Queues

```ts
await tasks.queues.upsert("emails", { parallelism: 5, rate: "100/1m", retries: 3, timeout: "20s" });
await tasks.queues.pause("emails");
await tasks.queues.resume("emails");
await tasks.queues.delete("emails", { force: true });
```

**Pull consumers** are for workers that can't take HTTP requests. Publish with only a `queue`:

```ts
await tasks.publish({ queue: "jobs", body: { n: 1 } });

const stop = new AbortController();
await tasks.consume("jobs", async (msg, { signal }) => {
  await work(msg.json());     // ack on return, nack on throw; the lease is extended while it runs
}, { concurrency: 4, visibility: "30s", signal: stop.signal });
```

For manual control, use `queues.pull(name, { max, visibility, wait })`, `queues.ack(name, receipt | receipts)`, `queues.nack(name, receipt, { delay, error })` and `queues.extend(name, receipt, visibility)`.

## Topics, schedules, events

```ts
await tasks.topics.upsert("orders", ["https://a.example/hook", { url: "https://b.example/hook", name: "billing" }]);
await tasks.topics.addEndpoints("orders", ["https://c.example/hook"]);
await tasks.topics.removeEndpoints("orders", ["billing"]);

await tasks.schedules.create({ id: "nightly", cron: "0 3 * * *", tz: "Europe/Budapest", publish: { url, body: "go" } });
await tasks.schedules.create({ cron: "@every 30s", event: { name: "tick" } });
await tasks.schedules.create({ cron: "*/5 * * * *", function: "cleanup" });
// update, pause, resume, trigger (fire now), list, get, delete

await tasks.events.send({ name: "user.created", data: { id: 42 } });
await tasks.events.send([{ name: "a" }, { name: "b", id: "dedup-key" }]);
await tasks.events.list({ name: "user.created" });
```

## Durable workflows

Define functions, serve them over HTTP, and register the URL with roll.

```ts
import { createFunction, serve, NonRetriableError } from "@roll/tasks";

export const onboarding = createFunction(
  {
    id: "onboarding",
    trigger: { event: "user.created" },          // or triggers: [{ event, if }, { cron, tz }]
    retries: 3,
    concurrency: { limit: 10, key: "event.data.plan" },
    cancelOn: { event: "user.deleted", match: "data.id" },
  },
  async ({ event, step }) => {
    const user = await step.run("load-user", () => db.users.find(event.data.id));
    await step.run("send-welcome", () => email.send(user.email, "Welcome!"));

    await step.sleep("wait-a-day", "1d");

    const activated = await step.waitForEvent("activation", {
      event: "user.activated",
      match: "data.id",
      timeout: "7d",
    }); // the event, or null on timeout

    if (!activated) {
      const score = await step.invoke("score", { function: "lead-score", data: { id: user.id } });
      await step.sendEvent("nudge", { name: "user.nudge", data: { id: user.id, score } });
    }
    return { activated: !!activated };
  },
);

// Next.js: app/api/roll/route.ts
const handler = serve({ appId: "my-app", functions: [onboarding] });
export { handler as GET, handler as POST };
```

On Node, use `http.createServer(toNodeHandler(handler))`, which works with Express too. Then register the app once (on deploy):

```ts
await tasks.apps.sync({ url: "https://my-app.example/api/roll" });
```

How steps run:

- The function runs from the top on every call. Finished steps return their recorded results instead of running again, so put side effects inside `step.run`.
- `Promise.all([step.run(...), step.run(...)])` runs the steps in parallel and reports them in one call.
- Step ids must be unique and stable. Repeating an id in a loop is allowed: repeats become `id:1`, `id:2`, … in order.
- A step that throws is retried with backoff, using the function's `retries`. Once its retries run out, `step.run` throws a `StepError` you can catch.
- Throw `NonRetriableError` to skip retries, from a step or from the whole function.
- `step.invoke` throws a `StepError` if the child run fails or times out.
- Results go through JSON, and `undefined` becomes `null`.

Calls from roll are verified against `signingKeys` (default: `ROLL_TASKS_SIGNING_KEY` and `ROLL_TASKS_SIGNING_KEY_NEXT`). Unsigned or forged calls get a 401. Pass `verify: false` for local development without keys.

The function config also takes:

- `rateLimit`, `throttle`, `debounce`, `batch`
- `idempotency`
- `priority: { run }`
- `timeouts: { start, finish }`
- `singleton: { mode: "skip" | "cancel", key }`

### Managing functions and runs

```ts
await tasks.functions.invoke(onboarding, { data: { id: 1 } });            // { runId }
const run = await tasks.functions.invoke("echo", { data: 1, wait: "10s" }); // the finished run
await tasks.functions.pause("onboarding"); // runs queue up until resume
await tasks.functions.cancelRuns("onboarding", { if: "event.data.plan == 'free'" });

await tasks.runs.list({ function: "onboarding", status: "failed" });
await tasks.runs.get(runId);        // with steps
await tasks.runs.wait(runId, { timeout: "1m" });
await tasks.runs.cancel(runId);
await tasks.runs.rerun(runId);
```

There are also `apps.list/delete`, `apiKeys.list/create/delete`, `usage(days)`, `stats()` and `health()`. `tasks.request(method, path, { query, body })` calls any other endpoint.

## Development

```sh
pnpm --filter @roll/tasks build      # tsc → dist
pnpm --filter @roll/tasks typecheck
pnpm --filter @roll/tasks test       # unit tests + end to end against the real server
```

The end-to-end suite builds and spawns `roll-tasks` itself, with `cargo build -p tasks --no-default-features --bin roll-tasks`. Set `ROLL_TASKS_BIN` to use a prebuilt binary instead.
