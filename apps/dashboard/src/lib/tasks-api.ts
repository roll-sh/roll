/**
 * Client for the roll tasks API (crates/tasks/src/api.rs), served by rolld under `/v1/tasks`.
 * JSON is snake_case and every time is unix milliseconds.
 */

import { ROLL_API } from "./roll-api";

export const TASKS_API = `${ROLL_API}/v1/tasks`;
const TOKEN = process.env.NEXT_PUBLIC_ROLL_TASKS_TOKEN || undefined;

// --- model (crates/tasks/src/model.rs) ---

export type Ms = number;

export type MsgState = "pending" | "active" | "delivered" | "failed" | "canceled";

export type Target =
  | { type: "http"; url: string; method: string; headers?: [string, string][] }
  | { type: "pull" }
  | { type: "tick"; schedule_id: string; at: Ms }
  | { type: "run"; run_id: string }
  | { type: "timer"; run_id: string; step_id: string; kind: string }
  | { type: "flush"; function_id: string; key: string };

export interface Backoff {
  strategy: "exponential" | "linear" | "fixed";
  base_ms: number;
  max_ms: number;
  jitter: number;
}

export interface Rate {
  limit: number;
  period_ms: number;
}

export interface Flow {
  key: string;
  parallelism?: number;
  rate?: Rate;
}

export interface Outcome {
  at: Ms;
  status?: number;
  error?: string;
  body?: string;
  duration_ms: number;
}

export interface Message {
  id: string;
  target: Target;
  queue?: string;
  lane: string;
  state: MsgState;
  due_at: Ms;
  created_at: Ms;
  updated_at: Ms;
  attempts: number;
  retries: number;
  backoff: Backoff;
  timeout_ms: number;
  callback?: string;
  failure_callback?: string;
  dedup_id?: string;
  group?: string;
  flows?: Flow[];
  topic?: string;
  schedule_id?: string;
  run_id?: string;
  label?: string;
  content_type?: string;
  body_size: number;
  deferrals: number;
  last?: Outcome;
  reason?: string;
  expires_at?: Ms;
  version: number;
}

export interface Attempt {
  message_id: string;
  n: number;
  at: Ms;
  duration_ms: number;
  status?: number;
  error?: string;
  response?: string;
  next_at?: Ms;
}

export interface MessageDetail extends Message {
  attempts_log: Attempt[];
  body?: string;
  body_base64?: string;
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
  timeout_ms?: number;
  created_at: Ms;
  updated_at: Ms;
  stats?: QueueStats;
}

export interface TopicEndpoint {
  url: string;
  name?: string;
}

export interface Topic {
  name: string;
  endpoints: TopicEndpoint[];
  created_at: Ms;
  updated_at: Ms;
}

export type ScheduleTarget =
  | { type: "publish"; request: PublishRequest }
  | { type: "event"; name: string; data?: unknown }
  | { type: "function"; function_id: string };

export interface Schedule {
  id: string;
  cron: string;
  tz?: string;
  target: ScheduleTarget;
  paused: boolean;
  next_at?: Ms;
  last_at?: Ms;
  last_message_id?: string;
  tick_id?: string;
  label?: string;
  created_at: Ms;
  updated_at: Ms;
  /** Next fire times; only for unpaused schedules. */
  upcoming?: Ms[];
}

export interface Trigger {
  event?: string;
  cron?: string;
  tz?: string;
  if?: string;
}

export interface KeyedRate {
  limit: number;
  period_ms: number;
  key?: string;
}

export interface FunctionDef {
  id: string;
  name?: string;
  triggers: Trigger[];
  retries?: number;
  concurrency?: { limit: number; key?: string }[];
  rate_limit?: KeyedRate;
  throttle?: KeyedRate;
  debounce?: { period_ms: number; key?: string; timeout_ms?: number };
  batch?: { max_size: number; timeout_ms: number; key?: string };
  idempotency?: string;
  cancel_on?: { event: string; match?: string; if?: string; timeout_ms?: number }[];
  priority?: string;
  timeouts?: { start_ms?: number; finish_ms?: number };
  singleton?: { mode: "skip" | "cancel"; key?: string };
  paused: boolean;
  /** Added by the API. */
  app_id?: string;
  url?: string;
  stats?: QueueStats;
}

export interface App {
  id: string;
  url: string;
  functions: FunctionDef[];
  sdk?: string;
  synced_at: Ms;
  created_at: Ms;
}

export type RunStatus = "queued" | "running" | "waiting" | "completed" | "failed" | "canceled";

export interface PendingOp {
  step_id: string;
  op: string;
  until?: Ms;
  target?: string;
  key?: string;
  child_run?: string;
}

export interface TaskEventShape {
  id?: string;
  name?: string;
  data?: unknown;
  user?: unknown;
  ts?: Ms;
}

export interface Run {
  id: string;
  function_id: string;
  app_id: string;
  status: RunStatus;
  events: TaskEventShape[];
  output?: unknown;
  error?: unknown;
  created_at: Ms;
  started_at?: Ms;
  ended_at?: Ms;
  invocation?: string;
  dirty: boolean;
  pending?: PendingOp[];
  step_attempts?: Record<string, number>;
  parent?: [string, string];
  invocations: number;
  version: number;
  expires_at?: Ms;
}

export interface Step {
  run_id: string;
  id: string;
  name?: string;
  op: string;
  data?: unknown;
  error?: unknown;
  at: Ms;
  attempts: number;
}

export interface RunDetail extends Run {
  steps: Step[];
  /** Invocation, timer and call messages of the run. */
  messages: Message[];
}

export interface TaskEvent {
  id: string;
  name: string;
  dedup_id?: string;
  data: unknown;
  user?: unknown;
  ts: Ms;
  runs?: string[];
  expires_at?: Ms;
}

export interface SigningKeys {
  current: string;
  next: string;
  rotated_at: Ms;
}

export interface ApiKey {
  id: string;
  name: string;
  hash: string;
  prefix: string;
  created_at: Ms;
  last_used_at?: Ms;
}

export interface WorkerNode {
  id: string;
  seen_at: Ms;
  started_at: Ms;
  inflight: number;
  lanes: number;
}

export interface UsageCounters {
  published: number;
  deliveries: number;
  delivered: number;
  failed_attempts: number;
  dead: number;
  bytes_in: number;
  bytes_out: number;
  events: number;
  runs: number;
  steps: number;
  storage_writes: number;
}

export interface Cost {
  /** The AWS layout priced: the log store in S3 or DynamoDB. */
  layout: "log" | "dynamodb";
  /** What carries API calls to a log store cell. */
  front_door: "lambda" | "nlb" | null;
  /** A tenant of a shared cell: the cell's fixed cost is the platform's. */
  hosted: boolean;
  items: { name: string; usd: number }[];
  total_usd: number;
  usd_per_million_messages: number;
  /** Always-on compute (nodes or workers), whatever the traffic. */
  fixed_usd_per_month: number;
  fixed: string;
}

export interface UsageReport {
  /** Days are unix days (ms / 86 400 000). */
  period: { days: number; start_day: number; end_day: number };
  total: UsageCounters;
  /** Only days with activity. */
  daily: { day: number; usage: UsageCounters }[];
  cost: Cost;
  projected_month_usd: number;
}

export interface Stats {
  nodes: WorkerNode[];
  queues: { name: string; paused: boolean; stats: QueueStats }[];
  dlq: number;
  dlq_more: boolean;
  schedules: number;
  functions: number;
}

export interface TasksHealth {
  ok: boolean;
  version: string;
  store: string;
  node: string;
  auth: boolean;
}

export interface Page<T> {
  items: T[];
  cursor: string | null;
}

// --- requests (crates/tasks/src/engine.rs, schedules.rs) ---

/** Durations accept milliseconds or strings like "30s", "5m", "1h". */
export type Duration = number | string;

export interface PublishRequest {
  url?: string;
  topic?: string;
  queue?: string;
  method?: string;
  headers?: Record<string, string>;
  body?: unknown;
  body_base64?: string;
  content_type?: string;
  delay?: Duration;
  not_before?: number | string;
  retries?: number;
  backoff?: Partial<Backoff>;
  timeout?: Duration;
  callback?: string;
  failure_callback?: string;
  dedup_id?: string;
  content_dedup?: boolean;
  flow?: { key: string; parallelism?: number; rate?: string | Rate };
  group?: string;
  priority?: number;
  label?: string;
}

export interface Published {
  message_id: string;
  messages: { message_id: string; url?: string; deduplicated?: boolean }[];
}

/** `null` clears an optional setting; a missing key leaves it unchanged. */
export interface QueueUpdate {
  parallelism?: number;
  rate?: string | Rate | null;
  paused?: boolean;
  partitions?: number;
  retries?: number | null;
  timeout?: Duration | null;
}

export interface ScheduleRequest {
  id?: string;
  cron: string;
  tz?: string;
  publish?: PublishRequest;
  event?: { name: string; data?: unknown };
  function?: string;
  paused?: boolean;
  label?: string;
}

export interface MessageFilter {
  state?: MsgState | "all";
  queue?: string;
  topic?: string;
  schedule_id?: string;
  run_id?: string;
  label?: string;
  url?: string;
  cursor?: string;
  limit?: number;
  /** Include internal messages (schedule ticks, timers, run calls). */
  all?: boolean;
}

// --- transport ---

export class TasksApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

function qs(params: object) {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || v === null || v === "" || v === false) continue;
    p.set(k, String(v));
  }
  const s = p.toString();
  return s ? `?${s}` : "";
}

const enc = encodeURIComponent;

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers["content-type"] = "application/json";
  if (TOKEN) headers.authorization = `Bearer ${TOKEN}`;
  const res = await fetch(`${TASKS_API}${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store",
  });
  const text = await res.text();
  let data: unknown = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = text;
  }
  if (!res.ok) {
    // Errors are `{error}`; axum's body rejections (bad JSON, wrong types) are plain text.
    const msg =
      (typeof data === "string" && data.trim()) ||
      (data as { error?: string } | null)?.error ||
      `${res.status} ${res.statusText}`;
    throw new TasksApiError(msg, res.status);
  }
  return data as T;
}

export const tasks = {
  health: () => call<TasksHealth>("GET", "/health"),
  stats: () => call<Stats>("GET", "/stats"),
  usage: (days = 14) => call<UsageReport>("GET", `/usage?days=${days}`),

  // messages
  publish: (req: PublishRequest) => call<Published>("POST", "/publish", req),
  messages: (f: MessageFilter = {}) => call<Page<Message>>("GET", `/messages${qs(f)}`),
  message: (id: string) => call<MessageDetail>("GET", `/messages/${enc(id)}`),
  cancelMessage: (id: string) => call<Message>("DELETE", `/messages/${enc(id)}`),
  purgeMessage: (id: string) => call<null>("DELETE", `/messages/${enc(id)}?purge=true`),
  retryMessage: (id: string) => call<Message>("POST", `/messages/${enc(id)}/retry`),
  cancelMessages: (body: { ids?: string[]; queue?: string; url?: string; topic?: string; label?: string; all?: boolean }) =>
    call<{ canceled: number }>("POST", "/messages/cancel", body),

  // dead-letter queue
  dlq: (f: Omit<MessageFilter, "state"> = {}) => call<Page<Message>>("GET", `/dlq${qs(f)}`),
  retryDlq: (body: { ids?: string[]; queue?: string; all?: boolean }) => call<{ retried: number }>("POST", "/dlq/retry", body),
  deleteDlq: (id: string) => call<null>("DELETE", `/dlq/${enc(id)}`),

  // queues
  queues: () => call<Queue[]>("GET", "/queues"),
  queue: (name: string) => call<Queue>("GET", `/queues/${enc(name)}`),
  putQueue: (name: string, update: QueueUpdate = {}) => call<Queue>("PUT", `/queues/${enc(name)}`, update),
  deleteQueue: (name: string, force = false) =>
    call<{ canceled: number }>("DELETE", `/queues/${enc(name)}${force ? "?force=true" : ""}`),
  pauseQueue: (name: string) => call<Queue>("POST", `/queues/${enc(name)}/pause`),
  resumeQueue: (name: string) => call<Queue>("POST", `/queues/${enc(name)}/resume`),

  // topics
  topics: () => call<Topic[]>("GET", "/topics"),
  putTopic: (name: string, endpoints: (string | TopicEndpoint)[]) =>
    call<Topic>("PUT", `/topics/${enc(name)}`, { endpoints }),
  addEndpoints: (name: string, endpoints: (string | TopicEndpoint)[]) =>
    call<Topic>("POST", `/topics/${enc(name)}/endpoints`, { endpoints }),
  removeEndpoints: (name: string, urls: string[]) => call<Topic>("DELETE", `/topics/${enc(name)}/endpoints`, { urls }),
  deleteTopic: (name: string) => call<null>("DELETE", `/topics/${enc(name)}`),

  // schedules
  schedules: () => call<Schedule[]>("GET", "/schedules"),
  createSchedule: (req: ScheduleRequest) => call<Schedule>("POST", "/schedules", req),
  updateSchedule: (id: string, u: Partial<ScheduleRequest>) => call<Schedule>("PATCH", `/schedules/${enc(id)}`, u),
  deleteSchedule: (id: string) => call<null>("DELETE", `/schedules/${enc(id)}`),
  pauseSchedule: (id: string) => call<Schedule>("POST", `/schedules/${enc(id)}/pause`),
  resumeSchedule: (id: string) => call<Schedule>("POST", `/schedules/${enc(id)}/resume`),
  triggerSchedule: (id: string) => call<{ fired: unknown }>("POST", `/schedules/${enc(id)}/trigger`),

  // events
  events: (f: { name?: string; cursor?: string; limit?: number } = {}) => call<Page<TaskEvent>>("GET", `/events${qs(f)}`),
  event: (id: string) => call<TaskEvent>("GET", `/events/${enc(id)}`),
  sendEvent: (name: string, data: unknown, id?: string) => call<{ ids: string[] }>("POST", "/events", { name, data, id }),

  // apps, functions, runs
  apps: () => call<App[]>("GET", "/apps"),
  syncApp: (url: string) => call<App>("POST", "/apps", { url }),
  deleteApp: (id: string) => call<null>("DELETE", `/apps/${enc(id)}`),
  functions: () => call<FunctionDef[]>("GET", "/functions"),
  fn: (id: string) => call<FunctionDef>("GET", `/functions/${enc(id)}`),
  invoke: (id: string, data: unknown) => call<{ run_id: string }>("POST", `/functions/${enc(id)}/invoke`, { data }),
  pauseFunction: (id: string) => call<FunctionDef>("POST", `/functions/${enc(id)}/pause`),
  resumeFunction: (id: string) => call<FunctionDef>("POST", `/functions/${enc(id)}/resume`),
  runs: (f: { function?: string; status?: RunStatus | "all"; cursor?: string; limit?: number } = {}) =>
    call<Page<Run>>("GET", `/runs${qs(f)}`),
  run: (id: string) => call<RunDetail>("GET", `/runs/${enc(id)}`),
  cancelRun: (id: string) => call<Run>("POST", `/runs/${enc(id)}/cancel`),
  rerun: (id: string) => call<Run>("POST", `/runs/${enc(id)}/rerun`),

  // keys
  signingKeys: () => call<SigningKeys>("GET", "/keys"),
  rotateSigningKeys: () => call<SigningKeys>("POST", "/keys/rotate"),
  apiKeys: () => call<ApiKey[]>("GET", "/api-keys"),
  createApiKey: (name: string) => call<ApiKey & { token: string }>("POST", "/api-keys", { name }),
  deleteApiKey: (id: string) => call<null>("DELETE", `/api-keys/${enc(id)}`),
};

// --- formatting ---

/**
 * "12s ago" for the past, "in 5m" for the future. `now` is a re-render tick (useNow); the real
 * clock wins when the tick is older than freshly polled data.
 */
export function rel(ms: Ms | null | undefined, now = Date.now()) {
  if (ms == null) return "—";
  const d = Math.round((ms - Math.max(now, Date.now())) / 1000);
  const s = Math.abs(d);
  const v =
    s < 1 ? "now" : s < 60 ? `${s}s` : s < 3600 ? `${Math.floor(s / 60)}m` : s < 86400 ? `${Math.floor(s / 3600)}h` : `${Math.floor(s / 86400)}d`;
  if (v === "now") return "just now";
  return d < 0 ? `${v} ago` : `in ${v}`;
}

export function formatTime(ms: Ms | null | undefined) {
  if (ms == null) return "—";
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

/** 1500 → "1.5s", 90000 → "1.5m". */
export function formatDuration(ms: number | null | undefined) {
  if (ms == null) return "—";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = ms / 1000;
  if (s < 60) return `${+s.toFixed(s < 10 ? 2 : 1)}s`;
  if (s < 3600) return `${+(s / 60).toFixed(1)}m`;
  if (s < 86400) return `${+(s / 3600).toFixed(1)}h`;
  return `${+(s / 86400).toFixed(1)}d`;
}

const PERIODS: [number, string][] = [
  [86_400_000, "d"],
  [3_600_000, "h"],
  [60_000, "m"],
  [1000, "s"],
];

/** {limit: 10, period_ms: 1000} → "10/s"; {limit: 100, period_ms: 120000} → "100/2m". */
export function formatRate(r: Rate | null | undefined) {
  if (!r) return "—";
  for (const [ms, unit] of PERIODS) {
    if (r.period_ms % ms === 0) {
      const n = r.period_ms / ms;
      return `${r.limit}/${n === 1 ? "" : n}${unit}`;
    }
  }
  return `${r.limit}/${r.period_ms}ms`;
}

export function formatCount(n: number) {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${+(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${+(n / 1_000_000).toFixed(1)}M`;
}

/** Parse a string as JSON and pretty-print it; otherwise return it unchanged. */
export function prettyBody(s: string | undefined | null): { text: string; json: boolean } {
  if (s == null || s === "") return { text: "", json: false };
  try {
    return { text: JSON.stringify(JSON.parse(s), null, 2), json: true };
  } catch {
    return { text: s, json: false };
  }
}

export function prettyJson(v: unknown) {
  if (v === undefined) return "";
  return typeof v === "string" ? v : JSON.stringify(v, null, 2);
}

/** Where a message goes, in one line. */
export function targetSummary(m: Pick<Message, "target" | "topic" | "queue">) {
  const t = m.target;
  switch (t.type) {
    case "http":
      return t.url;
    case "pull":
      return `pull · ${m.queue ?? "queue"}`;
    case "tick":
      return `tick · ${t.schedule_id}`;
    case "run":
      return `run · ${t.run_id}`;
    case "timer":
      return `${t.kind} · ${t.run_id}`;
    case "flush":
      return `batch · ${t.function_id}`;
  }
}

const DAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

function hhmm(h: string, m: string) {
  return `${h.padStart(2, "0")}:${m.padStart(2, "0")}`;
}

/** A plain-English reading of common cron patterns; null when it isn't obvious. */
export function cronHint(cron: string): string | null {
  const c = cron.trim();
  const every = c.match(/^@every\s+(.+)$/i);
  if (every) return `every ${every[1]}`;
  const macros: Record<string, string> = {
    "@yearly": "once a year, Jan 1 at 00:00",
    "@annually": "once a year, Jan 1 at 00:00",
    "@monthly": "on the 1st of every month at 00:00",
    "@weekly": "every Sunday at 00:00",
    "@daily": "every day at 00:00",
    "@midnight": "every day at 00:00",
    "@hourly": "every hour, on the hour",
  };
  if (macros[c.toLowerCase()]) return macros[c.toLowerCase()];
  let f = c.split(/\s+/);
  let prefix = "";
  if (f.length === 6) {
    const [sec, ...rest] = f;
    if (sec === "*") return "every second";
    const ns = sec.match(/^\*\/(\d+)$/);
    if (ns && rest.every((x) => x === "*")) return `every ${ns[1]} seconds`;
    if (sec !== "0") prefix = `at second ${sec}, `;
    f = rest;
  }
  if (f.length !== 5) return null;
  const [min, hour, dom, mon, dow] = f;
  const num = /^\d+$/;
  const step = (x: string) => x.match(/^\*\/(\d+)$/)?.[1];
  const anyDay = dom === "*" && mon === "*" && dow === "*";
  let out: string | null = null;
  if (min === "*" && hour === "*" && anyDay) out = "every minute";
  else if (step(min) && hour === "*" && anyDay) out = `every ${step(min)} minutes`;
  else if (num.test(min) && hour === "*" && anyDay) out = min === "0" ? "every hour, on the hour" : `every hour at :${min.padStart(2, "0")}`;
  else if (num.test(min) && step(hour) && anyDay) out = `every ${step(hour)} hours at :${min.padStart(2, "0")}`;
  else if (num.test(min) && num.test(hour)) {
    const at = hhmm(hour, min);
    if (anyDay) out = `every day at ${at}`;
    else if (dom === "*" && mon === "*" && dow === "1-5") out = `weekdays at ${at}`;
    else if (dom === "*" && mon === "*" && num.test(dow)) out = `every ${DAYS[Number(dow) % 7]} at ${at}`;
    else if (num.test(dom) && mon === "*" && dow === "*") out = `on day ${dom} of every month at ${at}`;
  }
  return out ? prefix + out : null;
}
