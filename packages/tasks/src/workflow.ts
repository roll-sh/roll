import { type Duration, type TimeInput, parseDuration, toUnixMs } from "./duration.js";
import { NonRetriableError, StepError, serializeError, type SerializedError } from "./errors.js";
import type { BackoffOptions, EventPayload } from "./types.js";

// ---------------------------------------------------------------------------------------------
// Function configuration
// ---------------------------------------------------------------------------------------------

export type TriggerInput = { event: string; if?: string } | { cron: string; tz?: string };

export interface ConcurrencyInput {
  limit: number;
  /** Expression over `event`; each value gets its own limit, e.g. `"event.data.user_id"`. */
  key?: string;
}

export interface KeyedRateInput {
  limit: number;
  period: Duration;
  key?: string;
}

export interface CancelOnInput {
  event: string;
  /** Field that must be equal in both events, e.g. `"data.order_id"`. */
  match?: string;
  /** Expression over `event` (the run's) and `async` (the incoming one). */
  if?: string;
  /** Stop listening after this long. */
  timeout?: Duration;
}

export interface FunctionConfig {
  /** Letters, digits, `-`, `_`, `.`. */
  id: string;
  name?: string;
  /** Shorthand for a single trigger. */
  trigger?: TriggerInput;
  /** Events (`prefix/*` patterns work) or crons that start runs. None: invoke only. */
  triggers?: TriggerInput[];
  /** Retries per step and per call. Default 3. */
  retries?: number;
  /** Delay between those retries. Default: the server's (exponential from 2s). */
  backoff?: BackoffOptions;
  /** Runs (with steps in flight) at once. */
  concurrency?: number | ConcurrencyInput | (number | ConcurrencyInput)[];
  /** Hard limit: events over it are dropped. */
  rateLimit?: KeyedRateInput;
  /** Soft limit: runs over it wait. */
  throttle?: KeyedRateInput;
  /** Start one run with the last event after `period` without new ones. */
  debounce?: { period: Duration; key?: string; timeout?: Duration };
  /** Start runs with up to `maxSize` events (`events` in the handler). */
  batch?: { maxSize: number; timeout: Duration; key?: string };
  /** Expression over `event`: one run per value per 24h. */
  idempotency?: string;
  cancelOn?: CancelOnInput | CancelOnInput[];
  /** `run`: expression over `event` giving seconds to run ahead (+) or behind (−) others. */
  priority?: { run: string };
  /** Cancel runs that haven't started / finished this long after they were queued. */
  timeouts?: { start?: Duration; finish?: Duration };
  /** One run at a time (per key): `skip` new runs or `cancel` the old one. */
  singleton?: { mode: "skip" | "cancel"; key?: string };
}

/** The definition roll stores (snake_case wire format). */
export interface FunctionDefinition {
  id: string;
  name?: string;
  triggers: { event?: string; cron?: string; tz?: string; if?: string }[];
  retries?: number;
  backoff?: { strategy: "exponential" | "linear" | "fixed"; base_ms: number; max_ms: number; jitter?: number };
  concurrency?: { limit: number; key?: string }[];
  rate_limit?: { limit: number; period_ms: number; key?: string };
  throttle?: { limit: number; period_ms: number; key?: string };
  debounce?: { period_ms: number; key?: string; timeout_ms?: number };
  batch?: { max_size: number; timeout_ms: number; key?: string };
  idempotency?: string;
  cancel_on?: { event: string; match?: string; if?: string; timeout_ms?: number }[];
  priority?: string;
  timeouts?: { start_ms?: number; finish_ms?: number };
  singleton?: { mode: "skip" | "cancel"; key?: string };
}

const ID = /^[A-Za-z0-9_.-]{1,128}$/;

function keyed(r: KeyedRateInput, what: string) {
  if (!(r.limit > 0)) throw new TypeError(`${what} needs a positive limit`);
  return strip({ limit: r.limit, period_ms: parseDuration(r.period), key: r.key });
}

function strip<T extends object>(o: T): T {
  for (const k of Object.keys(o) as (keyof T)[]) if (o[k] === undefined) delete o[k];
  return o;
}

/** Convert a camelCase {@link FunctionConfig} to the definition roll stores. */
export function toFunctionDefinition(c: FunctionConfig): FunctionDefinition {
  if (!c || typeof c.id !== "string" || !ID.test(c.id)) {
    throw new TypeError(`function ids use letters, digits, - _ . (max 128): ${JSON.stringify(c?.id)}`);
  }
  const triggers = [...(c.trigger ? [c.trigger] : []), ...(c.triggers ?? [])].map((t) => {
    const ev = (t as { event?: string }).event;
    const cron = (t as { cron?: string }).cron;
    if ((ev === undefined) === (cron === undefined)) throw new TypeError(`${c.id}: each trigger needs an event or a cron`);
    return ev !== undefined ? strip({ event: ev, if: (t as { if?: string }).if }) : strip({ cron, tz: (t as { tz?: string }).tz });
  });
  const def: FunctionDefinition = { id: c.id, name: c.name, triggers };
  if (c.retries !== undefined) {
    if (!Number.isInteger(c.retries) || c.retries < 0) throw new TypeError(`${c.id}: retries must be a non-negative integer`);
    def.retries = c.retries;
  }
  if (c.backoff) {
    const b = strip({
      strategy: c.backoff.strategy ?? "exponential",
      base_ms: c.backoff.base === undefined ? 2_000 : parseDuration(c.backoff.base),
      max_ms: c.backoff.max === undefined ? 3_600_000 : parseDuration(c.backoff.max),
      jitter: c.backoff.jitter,
    });
    if (b.max_ms < b.base_ms) throw new TypeError(`${c.id}: backoff max is below its base`);
    if (b.jitter !== undefined && !(b.jitter >= 0 && b.jitter <= 1)) throw new TypeError(`${c.id}: backoff jitter is between 0 and 1`);
    def.backoff = b;
  }
  if (c.concurrency !== undefined) {
    const list = Array.isArray(c.concurrency) ? c.concurrency : [c.concurrency];
    def.concurrency = list.map((x) => (typeof x === "number" ? { limit: x } : strip({ limit: x.limit, key: x.key })));
    for (const x of def.concurrency) if (!(x.limit > 0)) throw new TypeError(`${c.id}: concurrency limit must be positive`);
  }
  if (c.rateLimit) def.rate_limit = keyed(c.rateLimit, `${c.id}: rateLimit`);
  if (c.throttle) def.throttle = keyed(c.throttle, `${c.id}: throttle`);
  if (c.debounce) {
    def.debounce = strip({ period_ms: parseDuration(c.debounce.period), key: c.debounce.key, timeout_ms: c.debounce.timeout === undefined ? undefined : parseDuration(c.debounce.timeout) });
  }
  if (c.batch) def.batch = strip({ max_size: c.batch.maxSize, timeout_ms: parseDuration(c.batch.timeout), key: c.batch.key });
  if (c.idempotency !== undefined) def.idempotency = c.idempotency;
  if (c.cancelOn !== undefined) {
    const list = Array.isArray(c.cancelOn) ? c.cancelOn : [c.cancelOn];
    def.cancel_on = list.map((x) => strip({ event: x.event, match: x.match, if: x.if, timeout_ms: x.timeout === undefined ? undefined : parseDuration(x.timeout) }));
  }
  if (c.priority?.run !== undefined) def.priority = c.priority.run;
  if (c.timeouts) {
    def.timeouts = strip({
      start_ms: c.timeouts.start === undefined ? undefined : parseDuration(c.timeouts.start),
      finish_ms: c.timeouts.finish === undefined ? undefined : parseDuration(c.timeouts.finish),
    });
  }
  if (c.singleton) {
    if (c.singleton.mode !== "skip" && c.singleton.mode !== "cancel") throw new TypeError(`${c.id}: singleton mode is "skip" or "cancel"`);
    def.singleton = strip({ mode: c.singleton.mode, key: c.singleton.key });
  }
  return strip(def);
}

// ---------------------------------------------------------------------------------------------
// Handler API
// ---------------------------------------------------------------------------------------------

/** An event as functions see it. */
export interface ReceivedEvent<D = any> {
  id: string;
  name: string;
  data: D;
  user: unknown;
  ts: number;
}

/** A step id, or an id with a display name. */
export type StepId = string | { id: string; name?: string };

export interface WaitForEventOptions {
  /** Event name to wait for. */
  event: string;
  /** Give up (resolve `null`) after this long. */
  timeout: Duration;
  /** Field that must be equal in the run's event and the awaited one, e.g. `"data.order_id"`. */
  match?: string;
  /** Expression over `event` (the run's) and `async` (the awaited one). */
  if?: string;
}

export interface InvokeStepOptions {
  /** The function to run (id or the value `createFunction` returned). */
  function: string | { id: string };
  /** Becomes `event.data` of the child run. */
  data?: unknown;
  /** Fail the step (StepError `InvokeTimeout`) if the child takes longer. */
  timeout?: Duration;
}

export interface StepTools {
  /** Run `fn` once; its result is memoised and returned on every later call. Failures are retried. */
  run<T>(id: StepId, fn: () => T | Promise<T>): Promise<Awaited<T>>;
  /** Pause the run (no compute while sleeping). */
  sleep(id: StepId, duration: Duration): Promise<void>;
  sleepUntil(id: StepId, time: TimeInput): Promise<void>;
  /** The first matching event, or `null` after the timeout. */
  waitForEvent<D = any>(id: StepId, opts: WaitForEventOptions): Promise<ReceivedEvent<D> | null>;
  /** Run another function and get its output; throws a {@link StepError} if it fails. */
  invoke<T = unknown>(id: StepId, opts: InvokeStepOptions): Promise<T>;
  /** Send events (each sent exactly once, even if the step is reported again). */
  sendEvent(id: StepId, events: EventPayload | EventPayload[]): Promise<{ ids: string[] }>;
}

export interface FunctionContext<D = any> {
  /** The triggering event (the first one for batches). */
  event: ReceivedEvent<D>;
  /** Every triggering event (batches). */
  events: ReceivedEvent<D>[];
  runId: string;
  functionId: string;
  /** Retries of the current call (0 on the first try). */
  attempt: number;
  step: StepTools;
}

export type Handler<D = any, O = unknown> = (ctx: FunctionContext<D>) => O | Promise<O>;

export interface WorkflowFunction<D = any, O = unknown> {
  readonly id: string;
  readonly config: FunctionConfig;
  readonly handler: Handler<D, O>;
  /** The definition sent to roll. */
  readonly definition: FunctionDefinition;
}

/**
 * Define a durable function.
 *
 * ```ts
 * const welcome = createFunction({ id: "welcome", trigger: { event: "user.created" } }, async ({ event, step }) => {
 *   await step.run("send-email", () => sendEmail(event.data.email));
 *   await step.sleep("wait", "3d");
 *   ...
 * });
 * ```
 */
export function createFunction<D = any, O = unknown>(config: FunctionConfig, handler: Handler<D, O>): WorkflowFunction<D, O> {
  if (typeof handler !== "function") throw new TypeError("createFunction needs a handler");
  const definition = toFunctionDefinition(config);
  return Object.freeze({ id: config.id, config, handler, definition });
}

// ---------------------------------------------------------------------------------------------
// Protocol
// ---------------------------------------------------------------------------------------------

export type MemoisedStep = { data: unknown; error?: undefined } | { error: unknown; data?: undefined };

/** What roll sends on each call. */
export interface CallRequest {
  function_id: string;
  run_id: string;
  attempt?: number;
  event: unknown;
  events?: unknown[];
  steps?: Record<string, MemoisedStep>;
  ctx?: Record<string, unknown>;
}

export type StepOp =
  | { op: "run"; id: string; name: string; data: unknown }
  | { op: "run"; id: string; name: string; error: SerializedError; retriable: boolean }
  | { op: "sleep"; id: string; name: string; until?: number; duration_ms?: number }
  | { op: "wait_for_event"; id: string; name: string; event: string; match?: string; if?: string; timeout_ms: number }
  | { op: "invoke"; id: string; name: string; function: string; data: unknown; timeout_ms?: number }
  | { op: "send_event"; id: string; name: string; events: { name: string; data: unknown; id?: string; user?: unknown; ts?: number }[] };

/** What the SDK answers. */
export type CallReply =
  | { status: "done"; output: unknown }
  | { status: "error"; error: SerializedError; retriable: boolean }
  | { status: "steps"; steps: StepOp[] };

type Discovered = { op: StepOp; fn?: () => unknown };

const never = <T>() => new Promise<T>(() => {});
const tick = () => new Promise<void>((r) => setTimeout(r, 0));

/** JSON round trip, so a step's first result looks like its memoised replays. */
function jsonify(v: unknown): unknown {
  if (v === undefined) return null;
  const s = JSON.stringify(v);
  return s === undefined ? null : JSON.parse(s);
}

/**
 * Run one call of a function: replay memoised steps, stop at the first new ones, execute new
 * `run` steps (concurrently) and report what the run reached. Pure apart from the user's code.
 */
export async function executeFunction(fn: WorkflowFunction, req: CallRequest): Promise<CallReply> {
  const memo = req.steps ?? {};
  const seen = new Map<string, number>();
  const discovered: Discovered[] = [];
  let closed = false;
  let notify: (() => void) | undefined;

  const resolveId = (sid: StepId): { id: string; name: string } => {
    const base = typeof sid === "string" ? sid : sid?.id;
    if (typeof base !== "string" || base === "") throw new TypeError("steps need a non-empty id");
    const n = seen.get(base) ?? 0;
    seen.set(base, n + 1);
    return { id: n === 0 ? base : `${base}:${n}`, name: (typeof sid === "object" && sid.name) || base };
  };

  // A memoised step settles at once; a new one is recorded and never settles in this call.
  const step = <T>(sid: StepId, make: (id: string, name: string) => Discovered, map?: (data: unknown) => T): Promise<T> => {
    const { id, name } = resolveId(sid);
    if (Object.prototype.hasOwnProperty.call(memo, id)) {
      const m = memo[id]!;
      if (m.error !== undefined && m.error !== null) return Promise.reject(new StepError(id, m.error));
      return Promise.resolve(map ? map(m.data) : (m.data as T));
    }
    if (!closed) {
      discovered.push(make(id, name));
      notify?.();
    }
    return never<T>();
  };

  const tools: StepTools = {
    run: (sid, run) => {
      if (typeof run !== "function") throw new TypeError("step.run needs a function");
      return step(sid, (id, name) => ({ op: { op: "run", id, name, data: null }, fn: run })) as never;
    },
    sleep: (sid, duration) => {
      const ms = parseDuration(duration);
      return step(sid, (id, name) => ({ op: { op: "sleep", id, name, duration_ms: ms } }), () => undefined);
    },
    sleepUntil: (sid, time) => {
      const until = toUnixMs(time);
      return step(sid, (id, name) => ({ op: { op: "sleep", id, name, until } }), () => undefined);
    },
    waitForEvent: (sid, o) => {
      if (!o || typeof o.event !== "string") throw new TypeError("step.waitForEvent needs an event name");
      const timeout_ms = parseDuration(o.timeout);
      return step(
        sid,
        (id, name) => ({ op: strip({ op: "wait_for_event" as const, id, name, event: o.event, match: o.match, if: o.if, timeout_ms }) }),
        (data) => (data ?? null) as never,
      );
    },
    invoke: (sid, o) => {
      const fid = typeof o?.function === "string" ? o.function : o?.function?.id;
      if (!fid) throw new TypeError("step.invoke needs a function");
      const timeout_ms = o.timeout === undefined ? undefined : parseDuration(o.timeout);
      return step(sid, (id, name) => ({ op: strip({ op: "invoke" as const, id, name, function: fid, data: jsonify(o.data), timeout_ms }) }));
    },
    sendEvent: (sid, events) => {
      const list = (Array.isArray(events) ? events : [events]).map((e) => {
        if (!e || typeof e.name !== "string" || !e.name) throw new TypeError("events need a name");
        return strip({ name: e.name, data: jsonify(e.data), id: e.id, user: e.user, ts: e.ts === undefined ? undefined : toUnixMs(e.ts) });
      });
      return step(sid, (id, name) => ({ op: { op: "send_event", id, name, events: list } }), (data) => (data ?? { ids: [] }) as { ids: string[] });
    },
  };

  const events = (req.events && req.events.length ? req.events : [req.event]) as ReceivedEvent[];
  const ctx: FunctionContext = {
    event: (req.event ?? events[0]) as ReceivedEvent,
    events,
    runId: req.run_id,
    functionId: req.function_id,
    attempt: req.attempt ?? 0,
    step: tools,
  };

  let settled: { ok: true; value: unknown } | { ok: false; error: unknown } | undefined;
  const done = (async () => fn.handler(ctx))().then(
    (value) => {
      settled = { ok: true, value };
    },
    (error) => {
      settled = { ok: false, error };
    },
  );

  // Wait until the handler settles, or it found new steps and has gone quiet (it's blocked on them).
  for (;;) {
    if (settled) break;
    if (discovered.length === 0) {
      await Promise.race([done, new Promise<void>((r) => (notify = r))]);
      notify = undefined;
      continue;
    }
    const n = discovered.length;
    await tick();
    await tick();
    if (settled || discovered.length === n) break;
  }
  closed = true;

  if (discovered.length > 0) {
    const steps = await Promise.all(
      discovered.map(async ({ op, fn: run }): Promise<StepOp> => {
        if (op.op !== "run" || !run) return op;
        try {
          return { op: "run", id: op.id, name: op.name, data: jsonify(await run()) };
        } catch (e) {
          return { op: "run", id: op.id, name: op.name, error: serializeError(e), retriable: !(e instanceof NonRetriableError) };
        }
      }),
    );
    return { status: "steps", steps };
  }
  if (settled!.ok) return { status: "done", output: jsonify(settled!.value) };
  const error = settled!.error;
  // A failed step already used its retries: retrying the whole call would replay the same error.
  const retriable = !(error instanceof NonRetriableError || error instanceof StepError);
  return { status: "error", error: serializeError(error), retriable };
}
