import { type Duration, durationMs, parseDuration, toUnixMs } from "./duration.js";
import { NonRetriableError, TasksError } from "./errors.js";
import type {
  ApiKey,
  App,
  BackoffOptions,
  CancelManyOptions,
  EventPayload,
  EventQuery,
  FunctionInfo,
  Health,
  InvokeOptions,
  Message,
  MessageDetail,
  MessageQuery,
  NackResult,
  Page,
  PublishOptions,
  PublishResult,
  PulledMessage,
  PullOptions,
  Queue,
  QueueOptions,
  RateInput,
  Run,
  RunDetail,
  RunQuery,
  Schedule,
  ScheduleOptions,
  ScheduleUpdate,
  SigningKeys,
  Stats,
  StoredEvent,
  Topic,
  TopicEndpointInput,
  UsageReport,
} from "./types.js";
import { base64Encode, camelize, compact, env, isBinary, sleep, toBytes } from "./util.js";
import { SDK } from "./serve.js";
import type { FunctionDefinition, WorkflowFunction } from "./workflow.js";

export interface TasksOptions {
  /** Default: `ROLL_TASKS_URL`, else `http://127.0.0.1:7070` (the local roll daemon). */
  baseUrl?: string;
  /** Bearer token. Default: `ROLL_TASKS_TOKEN`. */
  token?: string;
  /** Custom fetch (tests, proxies). Default: global `fetch`. */
  fetch?: typeof fetch;
  /** Extra headers on every request. */
  headers?: Record<string, string>;
}

type Query = Record<string, string | number | boolean | undefined | null>;

interface RequestOptions {
  query?: Query;
  body?: unknown;
  signal?: AbortSignal;
  /** Return the JSON as is (no camelCase conversion). */
  raw?: boolean;
}

const enc = encodeURIComponent;

function rate(r: RateInput | null | undefined): unknown {
  if (r === undefined || r === null || typeof r === "string") return r;
  return compact({ limit: r.limit, period: r.period === undefined ? undefined : parseDuration(r.period) });
}

function backoff(b: BackoffOptions | undefined) {
  if (!b) return undefined;
  return compact({
    strategy: b.strategy ?? "exponential",
    base_ms: b.base === undefined ? 2_000 : parseDuration(b.base),
    max_ms: b.max === undefined ? 3_600_000 : parseDuration(b.max),
    jitter: b.jitter,
  });
}

/** {@link PublishOptions} → the server's PublishRequest. */
export function toPublishRequest(o: PublishOptions): Record<string, unknown> {
  if (!o || typeof o !== "object") throw new TypeError("publish needs options");
  let body: unknown;
  let body_base64: string | undefined;
  if (isBinary(o.body)) body_base64 = base64Encode(toBytes(o.body));
  else body = o.body;
  return compact({
    url: o.url,
    topic: o.topic,
    queue: o.queue,
    method: o.method,
    headers: o.headers && Object.keys(o.headers).length ? o.headers : undefined,
    body,
    body_base64,
    content_type: o.contentType,
    delay: durationMs(o.delay),
    not_before: o.notBefore === undefined ? undefined : toUnixMs(o.notBefore),
    retries: o.retries,
    backoff: backoff(o.backoff),
    timeout: durationMs(o.timeout),
    callback: o.callback,
    failure_callback: o.failureCallback,
    dedup_id: o.dedupId,
    content_dedup: o.contentDedup || undefined,
    flow: o.flow ? compact({ key: o.flow.key, parallelism: o.flow.parallelism, rate: rate(o.flow.rate) }) : undefined,
    group: o.group,
    priority: o.priority,
    label: o.label,
  });
}

function eventIn(e: EventPayload): Record<string, unknown> {
  if (!e || typeof e.name !== "string") throw new TypeError("events need a name");
  return compact({ name: e.name, data: e.data, id: e.id, user: e.user, ts: e.ts === undefined ? undefined : toUnixMs(e.ts) });
}

const fnId = (f: string | { id: string }) => (typeof f === "string" ? f : f.id);

export interface ConsumeContext {
  /** Aborted when the consumer stops. */
  signal: AbortSignal;
  /** Push the lease out now (it's extended automatically while the handler runs). */
  extend(visibility?: Duration): Promise<number>;
}

export interface ConsumedMessage extends PulledMessage {
  /** The body parsed as JSON. */
  json<T = unknown>(): T;
  /** The body as bytes (text or base64 bodies). */
  bytes(): Uint8Array;
}

export interface ConsumeOptions {
  /** Messages handled at once. Default 1. */
  concurrency?: number;
  /** Lease length; renewed at half of it while the handler runs. Default 30s. */
  visibility?: Duration;
  /** Long-poll for this long per pull. Default 20s (the most the server allows). */
  wait?: Duration;
  /** Stop pulling; resolves once messages in flight are settled. */
  signal?: AbortSignal;
  /** Delay before a failed message is retried. Default: its backoff. */
  nackDelay?: Duration;
  /**
   * Errors from pulling, acking or extending. Handler errors nack the message instead; a
   * {@link NonRetriableError} dead-letters it at once.
   */
  onError?: (error: unknown) => void;
}

/**
 * The roll tasks API.
 *
 * ```ts
 * const tasks = new Tasks(); // ROLL_TASKS_URL, ROLL_TASKS_TOKEN
 * await tasks.publish({ url: "https://example.com/hook", body: { hello: "world" }, delay: "5m" });
 * ```
 */
export class Tasks {
  readonly baseUrl: string;
  private readonly token: string | undefined;
  private readonly fetchImpl: typeof fetch;
  private readonly extraHeaders: Record<string, string>;

  constructor(opts: TasksOptions = {}) {
    let base = (opts.baseUrl ?? env("ROLL_TASKS_URL") ?? "http://127.0.0.1:7070").replace(/\/+$/, "");
    base = base.replace(/\/v1\/tasks$/, "");
    this.baseUrl = base;
    this.token = opts.token ?? env("ROLL_TASKS_TOKEN");
    const f = opts.fetch ?? (globalThis as { fetch?: typeof fetch }).fetch;
    if (!f) throw new TypeError("no fetch: pass one in the options");
    this.fetchImpl = f;
    this.extraHeaders = opts.headers ?? {};
  }

  /** Call any endpoint under `/v1/tasks`. Responses are converted to camelCase unless `raw`. */
  async request<T = unknown>(method: string, path: string, opts: RequestOptions = {}): Promise<T> {
    const url = new URL(`${this.baseUrl}/v1/tasks${path}`);
    for (const [k, v] of Object.entries(opts.query ?? {})) if (v !== undefined && v !== null) url.searchParams.set(k, String(v));
    const headers: Record<string, string> = { accept: "application/json", "user-agent": SDK, ...this.extraHeaders };
    if (this.token) headers.authorization = `Bearer ${this.token}`;
    let body: string | undefined;
    if (opts.body !== undefined) {
      headers["content-type"] = "application/json";
      body = JSON.stringify(opts.body);
    }
    const res = await this.fetchImpl(url, { method, headers, body, signal: opts.signal });
    const text = await res.text();
    let parsed: unknown;
    if (text) {
      try {
        parsed = JSON.parse(text);
      } catch {
        parsed = text;
      }
    }
    if (!res.ok) {
      const msg =
        parsed && typeof parsed === "object" && typeof (parsed as { error?: unknown }).error === "string"
          ? (parsed as { error: string }).error
          : text || res.statusText || `HTTP ${res.status}`;
      throw new TasksError(res.status, msg, parsed);
    }
    return (opts.raw ? parsed : camelize(parsed)) as T;
  }

  // --- publishing ---

  /** Publish a message to a URL, a topic, or a queue (for pull consumers). */
  publish(opts: PublishOptions): Promise<PublishResult> {
    return this.request("POST", "/publish", { body: toPublishRequest(opts) });
  }

  /** Publish up to 1000 messages; each result is a publish result or `{ error }`. */
  publishBatch(messages: PublishOptions[]): Promise<(PublishResult | { error: string })[]> {
    return this.request("POST", "/batch", { body: messages.map(toPublishRequest) });
  }

  readonly messages = {
    list: (q: MessageQuery = {}): Promise<Page<Message>> =>
      this.request("GET", "/messages", {
        query: { state: q.state, queue: q.queue, topic: q.topic, schedule_id: q.scheduleId, run_id: q.runId, label: q.label, url: q.url, cursor: q.cursor, limit: q.limit, all: q.all },
      }),
    /** The message, its attempts and its body. */
    get: (id: string): Promise<MessageDetail> => this.request("GET", `/messages/${enc(id)}`),
    /** Cancel an unfinished message. */
    cancel: (id: string): Promise<Message> => this.request("DELETE", `/messages/${enc(id)}`),
    /** Cancel by ids, or every unfinished message matching a filter (`all: true` for no filter). */
    cancelMany: (o: CancelManyOptions): Promise<{ canceled: number }> => {
      const body =
        "ids" in o
          ? { ids: o.ids }
          : compact({ queue: o.queue, url: o.url, topic: o.topic, label: o.label, schedule_id: o.scheduleId, all: o.all });
      return this.request("POST", "/messages/cancel", { body });
    },
    /** Send a failed or canceled message again. */
    retry: (id: string): Promise<Message> => this.request("POST", `/messages/${enc(id)}/retry`),
    /** Delete a finished message for good. */
    purge: async (id: string): Promise<void> => {
      await this.request("DELETE", `/messages/${enc(id)}`, { query: { purge: true } });
    },
  };

  /** The dead-letter queue: messages whose retries ran out. */
  readonly dlq = {
    list: (q: Omit<MessageQuery, "state"> = {}): Promise<Page<Message>> =>
      this.request("GET", "/dlq", { query: { queue: q.queue, topic: q.topic, schedule_id: q.scheduleId, label: q.label, url: q.url, cursor: q.cursor, limit: q.limit } }),
    retry: (ids: string | string[]): Promise<{ retried: number }> =>
      this.request("POST", "/dlq/retry", { body: { ids: Array.isArray(ids) ? ids : [ids] } }),
    /** Retry everything in the DLQ, or only one queue's. */
    retryAll: (o: { queue?: string } = {}): Promise<{ retried: number }> =>
      this.request("POST", "/dlq/retry", { body: o.queue ? { queue: o.queue } : { all: true } }),
    delete: async (id: string): Promise<void> => {
      await this.request("DELETE", `/dlq/${enc(id)}`);
    },
  };

  readonly queues = {
    list: (): Promise<Queue[]> => this.request("GET", "/queues"),
    get: (name: string): Promise<Queue> => this.request("GET", `/queues/${enc(name)}`),
    /** Create or update a queue. */
    upsert: (name: string, o: QueueOptions = {}): Promise<Queue> =>
      this.request("PUT", `/queues/${enc(name)}`, {
        body: compact({
          parallelism: o.parallelism,
          rate: rate(o.rate),
          paused: o.paused,
          partitions: o.partitions,
          retries: o.retries,
          timeout: o.timeout === null ? null : durationMs(o.timeout),
        }),
      }),
    pause: (name: string): Promise<Queue> => this.request("POST", `/queues/${enc(name)}/pause`),
    resume: (name: string): Promise<Queue> => this.request("POST", `/queues/${enc(name)}/resume`),
    /** Delete a queue; `force` cancels its unfinished messages. */
    delete: (name: string, o: { force?: boolean } = {}): Promise<{ canceled: number }> =>
      this.request("DELETE", `/queues/${enc(name)}`, { query: { force: o.force || undefined } }),
    /** Lease messages published to the queue without a url or topic. */
    pull: async (name: string, o: PullOptions = {}): Promise<PulledMessage[]> => {
      const r = await this.request<{ messages: PulledMessage[] }>("POST", `/queues/${enc(name)}/pull`, {
        body: compact({ max: o.max, visibility: durationMs(o.visibility), wait: durationMs(o.wait) }),
        signal: o.signal,
      });
      return r.messages;
    },
    /** Settle leased messages as done. */
    ack: (name: string, receipts: string | string[]): Promise<{ acked: number; errors: { receipt: string; error: string }[] }> =>
      this.request("POST", `/queues/${enc(name)}/ack`, { body: Array.isArray(receipts) ? { receipts } : { receipt: receipts } }),
    /**
     * Give a message back: retried after `delay` (default: its backoff), or dead-lettered when
     * it's out of retries or `dead` is set.
     */
    nack: (name: string, receipt: string, o: { delay?: Duration; error?: string; dead?: boolean } = {}): Promise<NackResult> =>
      this.request("POST", `/queues/${enc(name)}/nack`, {
        body: compact({ receipt, delay: durationMs(o.delay), error: o.error, dead: o.dead || undefined }),
      }),
    /** Keep the lease for `visibility` from now. Returns the new lease end (unix ms). */
    extend: async (name: string, receipt: string, visibility: Duration): Promise<number> => {
      const r = await this.request<{ leaseUntil: number }>("POST", `/queues/${enc(name)}/extend`, { body: { receipt, visibility: parseDuration(visibility) } });
      return r.leaseUntil;
    },
    /** See {@link Tasks.consume}. */
    consume: (name: string, handler: (msg: ConsumedMessage, ctx: ConsumeContext) => unknown, o: ConsumeOptions = {}): Promise<void> =>
      this.consume(name, handler, o),
  };

  readonly topics = {
    list: (): Promise<Topic[]> => this.request("GET", "/topics"),
    get: (name: string): Promise<Topic> => this.request("GET", `/topics/${enc(name)}`),
    /** Create a topic or replace its endpoints. */
    upsert: (name: string, endpoints: (string | TopicEndpointInput)[]): Promise<Topic> =>
      this.request("PUT", `/topics/${enc(name)}`, { body: { endpoints } }),
    addEndpoints: (name: string, endpoints: (string | TopicEndpointInput)[]): Promise<Topic> =>
      this.request("POST", `/topics/${enc(name)}/endpoints`, { body: { endpoints } }),
    /** Remove endpoints by URL or name. */
    removeEndpoints: (name: string, urlsOrNames: string[]): Promise<Topic> =>
      this.request("DELETE", `/topics/${enc(name)}/endpoints`, { body: { urls: urlsOrNames } }),
    delete: async (name: string): Promise<void> => {
      await this.request("DELETE", `/topics/${enc(name)}`);
    },
  };

  readonly schedules = {
    create: (o: ScheduleOptions): Promise<Schedule> =>
      this.request("POST", "/schedules", {
        body: compact({
          id: o.id,
          cron: o.cron,
          tz: o.tz,
          publish: o.publish ? toPublishRequest(o.publish) : undefined,
          event: o.event ? compact({ name: o.event.name, data: o.event.data }) : undefined,
          function: o.function === undefined ? undefined : fnId(o.function),
          paused: o.paused,
          label: o.label,
        }),
      }),
    list: (): Promise<Schedule[]> => this.request("GET", "/schedules"),
    get: (id: string): Promise<Schedule> => this.request("GET", `/schedules/${enc(id)}`),
    update: (id: string, u: ScheduleUpdate): Promise<Schedule> =>
      this.request("PATCH", `/schedules/${enc(id)}`, {
        body: compact({
          cron: u.cron,
          tz: u.tz,
          publish: u.publish ? toPublishRequest(u.publish) : undefined,
          event: u.event ? compact({ name: u.event.name, data: u.event.data }) : undefined,
          function: u.function === undefined ? undefined : fnId(u.function),
          paused: u.paused,
          label: u.label,
        }),
      }),
    pause: (id: string): Promise<Schedule> => this.request("POST", `/schedules/${enc(id)}/pause`),
    resume: (id: string): Promise<Schedule> => this.request("POST", `/schedules/${enc(id)}/resume`),
    /** Fire now, outside the cadence. `fired` holds message, event or run ids. */
    trigger: (id: string): Promise<{ fired: string[] }> => this.request("POST", `/schedules/${enc(id)}/trigger`),
    delete: async (id: string): Promise<void> => {
      await this.request("DELETE", `/schedules/${enc(id)}`);
    },
  };

  readonly events = {
    /** Send one event or many (up to 1000). Matching functions start runs. */
    send: (events: EventPayload | EventPayload[]): Promise<{ ids: string[] }> =>
      this.request("POST", "/events", { body: Array.isArray(events) ? events.map(eventIn) : eventIn(events) }),
    list: (q: EventQuery = {}): Promise<Page<StoredEvent>> => this.request("GET", "/events", { query: { name: q.name, cursor: q.cursor, limit: q.limit } }),
    get: (id: string): Promise<StoredEvent> => this.request("GET", `/events/${enc(id)}`),
  };

  readonly functions = {
    list: (): Promise<FunctionInfo[]> => this.request("GET", "/functions"),
    get: (id: string | { id: string }): Promise<FunctionInfo> => this.request("GET", `/functions/${enc(fnId(id))}`),
    /** Start a run. With `wait`, resolves with the run once it ends (or the wait runs out). */
    invoke: ((id: string | { id: string }, o: InvokeOptions = {}) =>
      this.request("POST", `/functions/${enc(fnId(id))}/invoke`, {
        body: compact({ data: o.data, id: o.id, user: o.user, wait: durationMs(o.wait) }),
      })) as {
      <O = unknown>(id: string | { id: string }, o: InvokeOptions & { wait: Duration }): Promise<Run<O>>;
      (id: string | { id: string }, o?: InvokeOptions): Promise<{ runId: string }>;
    },
    pause: (id: string | { id: string }): Promise<FunctionInfo> => this.request("POST", `/functions/${enc(fnId(id))}/pause`),
    resume: (id: string | { id: string }): Promise<FunctionInfo> => this.request("POST", `/functions/${enc(fnId(id))}/resume`),
    /** Cancel unfinished runs, optionally only those whose event matches `if`. */
    cancelRuns: (id: string | { id: string }, o: { if?: string } = {}): Promise<{ canceled: number }> =>
      this.request("POST", `/functions/${enc(fnId(id))}/cancel`, { body: compact({ if: o.if }) }),
  };

  readonly runs = {
    list: (q: RunQuery = {}): Promise<Page<Run>> => this.request("GET", "/runs", { query: { function: q.function, status: q.status, cursor: q.cursor, limit: q.limit } }),
    /** The run with its memoised steps and calls. */
    get: <O = unknown>(id: string): Promise<RunDetail<O>> => this.request("GET", `/runs/${enc(id)}`),
    cancel: (id: string): Promise<Run> => this.request("POST", `/runs/${enc(id)}/cancel`),
    /** Start a new run with the same events. */
    rerun: (id: string): Promise<Run> => this.request("POST", `/runs/${enc(id)}/rerun`),
    /** Wait for a run to end, up to `timeout` (default 30s). Returns the run either way. */
    wait: async <O = unknown>(id: string, o: { timeout?: Duration; signal?: AbortSignal } = {}): Promise<Run<O>> => {
      const deadline = Date.now() + parseDuration(o.timeout ?? 30_000);
      for (;;) {
        const left = Math.max(0, deadline - Date.now());
        // The server waits up to 5 minutes per request.
        const run = await this.request<Run<O>>("GET", `/runs/${enc(id)}/wait`, { query: { timeout: Math.min(left, 300_000) }, signal: o.signal });
        if (["completed", "failed", "canceled"].includes(run.status) || Date.now() >= deadline) return run;
      }
    },
  };

  readonly apps = {
    /**
     * Register an app. With only `url`, roll fetches the functions from it (`GET url`); or push
     * them with `id` and `functions`.
     */
    sync: (o: { url: string; id?: string; functions?: (FunctionDefinition | WorkflowFunction<any, any>)[] }): Promise<App> =>
      this.request("POST", "/apps", {
        body: compact({
          url: o.url,
          id: o.id,
          functions: o.functions?.map((f) => ("definition" in f && "handler" in f ? f.definition : f)),
          sdk: o.functions ? SDK : undefined,
        }),
      }),
    list: (): Promise<App[]> => this.request("GET", "/apps"),
    /** Remove an app: its functions go away and their unfinished runs are canceled. */
    delete: async (id: string): Promise<void> => {
      await this.request("DELETE", `/apps/${enc(id)}`);
    },
  };

  /** The `whsec_…` secrets roll signs requests with. */
  readonly signingKeys = {
    get: (): Promise<SigningKeys> => this.request("GET", "/keys"),
    /** `next` becomes `current` and a new `next` is made. */
    rotate: (): Promise<SigningKeys> => this.request("POST", "/keys/rotate"),
  };

  readonly apiKeys = {
    list: (): Promise<ApiKey[]> => this.request("GET", "/api-keys"),
    /** The token is only returned here. */
    create: (name: string): Promise<ApiKey & { token: string }> => this.request("POST", "/api-keys", { body: { name } }),
    delete: async (id: string): Promise<void> => {
      await this.request("DELETE", `/api-keys/${enc(id)}`);
    },
  };

  usage(days?: number): Promise<UsageReport> {
    return this.request("GET", "/usage", { query: { days } });
  }

  stats(): Promise<Stats> {
    return this.request("GET", "/stats");
  }

  health(): Promise<Health> {
    return this.request("GET", "/health");
  }

  /**
   * Work a pull queue: long-poll, run `handler` for each message (up to `concurrency` at once),
   * extend the lease while it runs, ack when it returns and nack when it throws. Resolves when
   * `signal` aborts and the messages in flight are settled.
   */
  async consume(queue: string, handler: (msg: ConsumedMessage, ctx: ConsumeContext) => unknown, o: ConsumeOptions = {}): Promise<void> {
    const concurrency = Math.max(1, Math.floor(o.concurrency ?? 1));
    const visibility = parseDuration(o.visibility ?? "30s");
    const wait = Math.min(parseDuration(o.wait ?? "20s"), 20_000);
    const nackDelay = durationMs(o.nackDelay);
    const onError = o.onError ?? (() => {});
    const stop = new AbortController();
    const abort = () => stop.abort();
    if (o.signal?.aborted) return;
    o.signal?.addEventListener("abort", abort, { once: true });
    const stopped = new Promise<void>((r) => stop.signal.addEventListener("abort", () => r(), { once: true }));
    const active = new Set<Promise<void>>();

    const work = async (m: PulledMessage) => {
      const extend = (v?: Duration) => this.queues.extend(queue, m.receipt, v ?? visibility);
      const timer = setInterval(() => extend().catch(onError), Math.max(500, Math.floor(visibility / 2)));
      try {
        const bytes = () => (m.bodyBase64 !== undefined ? Uint8Array.from(atob(m.bodyBase64), (c) => c.charCodeAt(0)) : new TextEncoder().encode(m.body ?? ""));
        const msg: ConsumedMessage = { ...m, json: <T>() => JSON.parse(m.body ?? new TextDecoder().decode(bytes())) as T, bytes };
        let failed: unknown;
        let ok = false;
        try {
          await handler(msg, { signal: stop.signal, extend });
          ok = true;
        } catch (e) {
          failed = e;
        }
        clearInterval(timer);
        if (ok) await this.queues.ack(queue, m.receipt);
        else
          await this.queues.nack(queue, m.receipt, {
            delay: nackDelay,
            error: failed instanceof Error ? failed.message : String(failed),
            // Retrying won't help: straight to the dead-letter queue.
            dead: failed instanceof NonRetriableError,
          });
      } catch (e) {
        onError(e);
      } finally {
        clearInterval(timer);
      }
    };

    try {
      while (!stop.signal.aborted) {
        if (active.size >= concurrency) {
          await Promise.race([...active, stopped]);
          continue;
        }
        let msgs: PulledMessage[];
        try {
          msgs = await this.queues.pull(queue, { max: Math.min(100, concurrency - active.size), visibility, wait, signal: stop.signal });
        } catch (e) {
          if (stop.signal.aborted) break;
          onError(e);
          await sleep(1000, stop.signal);
          continue;
        }
        for (const m of msgs) {
          const p: Promise<void> = work(m).finally(() => active.delete(p));
          active.add(p);
        }
      }
    } finally {
      o.signal?.removeEventListener("abort", abort);
      await Promise.allSettled([...active]);
    }
  }
}
