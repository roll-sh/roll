import type { Duration, TimeInput } from "./duration.js";

// ---------------------------------------------------------------------------------------------
// Inputs (camelCase; converted to the snake_case wire format)
// ---------------------------------------------------------------------------------------------

export type HttpMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS";

/** `"10/s"`, `"100/1m"`, or `{ limit, period }`. */
export type RateInput = string | { limit: number; period?: Duration };

export interface BackoffOptions {
  /** Default `"exponential"`. */
  strategy?: "exponential" | "linear" | "fixed";
  /** First retry delay. Default 2s. */
  base?: Duration;
  /** Longest delay. Default 1h. */
  max?: Duration;
  /** Fraction of each delay randomised away, 0–1. Default 0.2. */
  jitter?: number;
}

export interface FlowOptions {
  /** Messages sharing a key share its limits. */
  key: string;
  /** Deliveries in flight at once for the key. */
  parallelism?: number;
  rate?: RateInput;
}

/** Message bodies: strings are sent as is, bytes as is, anything else as JSON. */
export type Body = string | Uint8Array | ArrayBuffer | ArrayBufferView | Record<string, unknown> | unknown[] | number | boolean | null;

export interface PublishOptions {
  /** Deliver to this URL … */
  url?: string;
  /** … or to every endpoint of a topic … */
  topic?: string;
  /** … through a queue; with neither url nor topic, the message waits for a pull consumer. */
  queue?: string;
  body?: Body;
  /** Default: `application/json` for JSON bodies. */
  contentType?: string;
  /** Default `POST`. */
  method?: HttpMethod;
  /** Headers sent with the delivery. */
  headers?: Record<string, string>;
  delay?: Duration;
  /** Not before this time. */
  notBefore?: TimeInput;
  /** Retries after the first attempt. */
  retries?: number;
  backoff?: BackoffOptions;
  /** Per-attempt timeout. */
  timeout?: Duration;
  /** URL that gets a message when the delivery succeeds. */
  callback?: string;
  /** URL that gets a message when the delivery fails for good. */
  failureCallback?: string;
  /** A second publish with the same id within 10 minutes is dropped. */
  dedupId?: string;
  /** Deduplicate on destination, body and headers. */
  contentDedup?: boolean;
  flow?: FlowOptions;
  /** FIFO key within the queue: one at a time, in publish order. Needs a queue. */
  group?: string;
  /** Seconds to jump ahead of (+) or fall behind (−) other due messages. */
  priority?: number;
  label?: string;
}

export interface MessageQuery {
  state?: MessageState | "dlq" | "all";
  queue?: string;
  topic?: string;
  scheduleId?: string;
  runId?: string;
  label?: string;
  url?: string;
  cursor?: string;
  limit?: number;
  /** Include internal messages (schedule ticks, timers, run calls). */
  all?: boolean;
}

export type CancelManyOptions =
  | { ids: string[] }
  | { queue?: string; url?: string; topic?: string; label?: string; scheduleId?: string; all?: boolean };

export interface QueueOptions {
  /** Deliveries in flight at once across every worker; 0 removes the limit. */
  parallelism?: number;
  /** `null` removes the limit. */
  rate?: RateInput | null;
  paused?: boolean;
  /** Due-index partitions (can only grow). */
  partitions?: number;
  /** Default retries for messages in the queue; `null` clears. */
  retries?: number | null;
  /** Default per-attempt timeout; `null` clears. */
  timeout?: Duration | null;
}

export interface PullOptions {
  /** Messages to lease, 1–100. Default 1. */
  max?: number;
  /** Lease length. Default 30s. */
  visibility?: Duration;
  /** Long-poll up to this long (max 20s) when nothing is ready. Default 0. */
  wait?: Duration;
  signal?: AbortSignal;
}

export interface TopicEndpointInput {
  url: string;
  name?: string;
}

export interface ScheduleOptions {
  id?: string;
  /** Cron (5 or 6 fields, `@daily` …) or `@every 30s`. */
  cron: string;
  /** IANA time zone. Default UTC. */
  tz?: string;
  /** Publish this on every fire … */
  publish?: PublishOptions;
  /** … or send an event … */
  event?: { name: string; data?: unknown };
  /** … or start a run of a function. */
  function?: string | { id: string };
  paused?: boolean;
  label?: string;
}

export interface ScheduleUpdate {
  cron?: string;
  tz?: string | null;
  publish?: PublishOptions;
  event?: { name: string; data?: unknown };
  function?: string | { id: string };
  paused?: boolean;
  label?: string | null;
}

export interface EventPayload<D = unknown> {
  name: string;
  data?: D;
  /** Deduplication id: a repeat within 24h is ignored. */
  id?: string;
  ts?: TimeInput;
  user?: unknown;
}

export interface EventQuery {
  name?: string;
  cursor?: string;
  limit?: number;
}

export interface RunQuery {
  function?: string;
  status?: RunStatus | "all";
  cursor?: string;
  limit?: number;
}

export interface InvokeOptions {
  data?: unknown;
  /** Deduplication id: invoking again with it returns the same run. */
  id?: string;
  user?: unknown;
  /** Wait up to this long for the run to finish and return it. */
  wait?: Duration;
}

// ---------------------------------------------------------------------------------------------
// Outputs (camelCase views of the server's records; user payloads are left untouched)
// ---------------------------------------------------------------------------------------------

export interface Page<T> {
  items: T[];
  /** Pass to the next call; absent on the last page. */
  cursor?: string | null;
}

export interface Published {
  messageId: string;
  url?: string;
  /** True when a previous publish with the same dedup key won. */
  deduplicated?: boolean;
}

export interface PublishResult {
  /** The first message (topics publish one per endpoint). */
  messageId: string;
  messages: Published[];
}

export type MessageState = "pending" | "active" | "delivered" | "failed" | "canceled";

export type MessageTarget =
  | { type: "http"; url: string; method: string; headers?: [string, string][] }
  | { type: "pull" }
  | { type: "tick"; scheduleId: string; at: number }
  | { type: "run"; runId: string }
  | { type: "timer"; runId: string; stepId: string; kind: string }
  | { type: "flush"; functionId: string; key: string };

export interface Backoff {
  strategy: "exponential" | "linear" | "fixed";
  baseMs: number;
  maxMs: number;
  jitter: number;
}

export interface Rate {
  limit: number;
  periodMs: number;
}

export interface Outcome {
  at: number;
  status?: number;
  error?: string;
  body?: string;
  durationMs: number;
}

export interface Message {
  id: string;
  target: MessageTarget;
  queue?: string;
  lane: string;
  state: MessageState;
  dueAt: number;
  createdAt: number;
  updatedAt: number;
  attempts: number;
  retries: number;
  backoff: Backoff;
  timeoutMs: number;
  callback?: string;
  failureCallback?: string;
  dedupId?: string;
  group?: string;
  flows?: { key: string; parallelism?: number; rate?: Rate }[];
  topic?: string;
  scheduleId?: string;
  runId?: string;
  label?: string;
  contentType?: string;
  bodySize: number;
  lease?: string;
  deferrals: number;
  last?: Outcome;
  reason?: string;
  expiresAt?: number;
  version: number;
}

export interface Attempt {
  messageId: string;
  n: number;
  at: number;
  durationMs: number;
  status?: number;
  error?: string;
  response?: string;
  nextAt?: number;
}

export interface MessageDetail extends Message {
  attemptsLog: Attempt[];
  /** UTF-8 bodies. */
  body?: string;
  /** Other bodies. */
  bodyBase64?: string;
}

export interface QueueStats {
  ready: number;
  scheduled: number;
  active: number;
  inflight: number;
}

export interface Queue {
  name: string;
  parallelism?: number;
  rate?: Rate;
  paused: boolean;
  partitions: number;
  retries?: number;
  timeoutMs?: number;
  createdAt: number;
  updatedAt: number;
  stats?: QueueStats;
}

export interface PulledMessage {
  id: string;
  /** Pass to ack / nack / extend. */
  receipt: string;
  body?: string;
  bodyBase64?: string;
  contentType?: string;
  attempts: number;
  retries: number;
  createdAt: number;
  leaseUntil: number;
  label?: string;
  group?: string;
}

export interface NackResult {
  state: MessageState;
  dueAt: number;
  attempts: number;
}

export interface Topic {
  name: string;
  endpoints: { url: string; name?: string }[];
  createdAt: number;
  updatedAt: number;
}

export type ScheduleTarget =
  | { type: "publish"; request: Record<string, unknown> }
  | { type: "event"; name: string; data: unknown }
  | { type: "function"; functionId: string };

export interface Schedule {
  id: string;
  cron: string;
  tz?: string;
  target: ScheduleTarget;
  paused: boolean;
  nextAt?: number;
  lastAt?: number;
  lastMessageId?: string;
  tickId?: string;
  label?: string;
  createdAt: number;
  updatedAt: number;
  /** The next fire times (unpaused schedules). */
  upcoming?: number[];
}

export interface StoredEvent<D = unknown> {
  id: string;
  name: string;
  dedupId?: string;
  data: D;
  user?: unknown;
  ts: number;
  /** Runs the event started. */
  runs?: string[];
  expiresAt?: number;
}

export type RunStatus = "queued" | "running" | "waiting" | "completed" | "failed" | "canceled";

export interface PendingOp {
  stepId: string;
  op: string;
  until?: number;
  target?: string;
  key?: string;
  childRun?: string;
}

export interface StepRecord {
  runId: string;
  id: string;
  name?: string;
  op: string;
  data?: unknown;
  error?: unknown;
  at: number;
  attempts: number;
}

export interface Run<O = unknown> {
  id: string;
  functionId: string;
  appId: string;
  status: RunStatus;
  /** Triggering events (`{ id, name, data, user, ts }`). */
  events: { id: string; name: string; data: unknown; user: unknown; ts: number }[];
  output?: O;
  error?: { name?: string; message?: string; [k: string]: unknown };
  createdAt: number;
  startedAt?: number;
  endedAt?: number;
  invocation?: string;
  dirty: boolean;
  pending?: PendingOp[];
  stepAttempts?: Record<string, number>;
  /** `[runId, stepId]` of the run that invoked this one. */
  parent?: [string, string];
  invocations: number;
  version: number;
  expiresAt?: number;
}

export interface RunDetail<O = unknown> extends Run<O> {
  steps: StepRecord[];
  messages: Message[];
}

export interface FunctionInfo {
  id: string;
  name?: string;
  triggers: { event?: string; cron?: string; tz?: string; if?: string }[];
  retries?: number;
  concurrency?: { limit: number; key?: string }[];
  rateLimit?: { limit: number; periodMs: number; key?: string };
  throttle?: { limit: number; periodMs: number; key?: string };
  debounce?: { periodMs: number; key?: string; timeoutMs?: number };
  batch?: { maxSize: number; timeoutMs: number; key?: string };
  idempotency?: string;
  cancelOn?: { event: string; match?: string; if?: string; timeoutMs?: number }[];
  priority?: string;
  timeouts?: { startMs?: number; finishMs?: number };
  singleton?: { mode: "skip" | "cancel"; key?: string };
  paused: boolean;
  appId?: string;
  url?: string;
  stats?: QueueStats;
}

export interface App {
  id: string;
  url: string;
  functions: FunctionInfo[];
  sdk?: string;
  syncedAt: number;
  createdAt: number;
}

export interface SigningKeys {
  /** `whsec_…` */
  current: string;
  /** Becomes current on the next rotation; requests are already signed with it. */
  next: string;
  rotatedAt: number;
}

export interface ApiKey {
  id: string;
  name: string;
  hash: string;
  prefix: string;
  createdAt: number;
  lastUsedAt?: number;
}

export interface Usage {
  published: number;
  deliveries: number;
  delivered: number;
  failedAttempts: number;
  dead: number;
  bytesIn: number;
  bytesOut: number;
  events: number;
  runs: number;
  steps: number;
  /** Object-storage writes (log store commits and snapshots). */
  storageWrites: number;
}

export interface Cost {
  /** The AWS layout priced: the log store in S3 or DynamoDB. */
  layout: "log" | "dynamodb";
  /** What carries API calls to a log store cell. */
  frontDoor: "lambda" | "nlb" | null;
  /** A tenant of a shared cell: the cell's fixed cost is the platform's. */
  hosted: boolean;
  items: { name: string; usd: number }[];
  totalUsd: number;
  usdPerMillionMessages: number;
  fixedUsdPerMonth: number;
  fixed: string;
}

export interface UsageReport {
  period: { days: number; startDay: number; endDay: number };
  total: Usage;
  daily: { day: number; usage: Usage }[];
  cost: Cost;
  projectedMonthUsd: number;
}

export interface Stats {
  nodes: { id: string; seenAt: number; startedAt: number; inflight: number; lanes: number }[];
  queues: { name: string; paused: boolean; stats: QueueStats }[];
  dlq: number;
  dlqMore: boolean;
  schedules: number;
  functions: number;
}

export interface Health {
  ok: boolean;
  version: string;
  store: string;
  node: string;
  auth: boolean;
}
