/**
 * roll code automations: the wire types of `agent_proto::automation` (crates/agent-proto/src/automation.rs),
 * their decoders, and the `/v1/code/agents/{owner}/automations…` client (docs/code/http-api.md,
 * "Automations"). The fixtures in crates/agent-proto/fixtures/automation*.json are parsed by
 * `automations-api.test.ts`, so a change on either side that breaks the other fails a test.
 *
 * Fields Rust leaves out when they're empty, false or null are optional here, so a record
 * decodes and serializes back the same.
 */
import type {
  ApprovalAction,
  Decision,
  InteractionMode,
  MachineId,
  Millis,
  ModelOptions,
  ProviderId,
  RuntimeMode,
  SendMode,
  ThreadId,
} from "./agents-api";
import { array, bool, decode, int, json, literal, map, nullable, obj, optional, str, strOrBool, tagged, withDefault, type Decoder } from "./decode";

export type AutomationId = string;
export type AutomationRunId = string;
export type AutomationEventId = string;

// ── definitions ──

export type AgentEvent = "thread.created" | "thread.done" | "thread.failed" | "thread.waiting" | "thread.settled" | "request.opened";
export const AGENT_EVENTS: readonly AgentEvent[] = [
  "thread.created",
  "thread.done",
  "thread.failed",
  "thread.waiting",
  "thread.settled",
  "request.opened",
];

/** The roll code events a `code` trigger can listen for (the webhook events). */
export const CODE_EVENTS = ["landed", "change", "review", "check", "queue", "work", "preview", "push", "thread", "budget"] as const;

export type Trigger =
  /** Only by hand: the dashboard's button, `roll agent automation run`, the MCP tool. */
  | { type: "manual" }
  /** A cron expression (5 fields, or 6 with seconds) in an IANA time zone. */
  | { type: "schedule"; cron: string; timezone: string }
  /** Every so many seconds (at least 60). */
  | { type: "interval"; every_s: number }
  /** `POST` to the automation's own URL, signed with its secret. */
  | { type: "webhook" }
  /** roll code events of the owner's repos; empty lists: all of them. */
  | { type: "code"; events: string[]; repos: string[] }
  | { type: "agent"; events: AgentEvent[] }
  /** An integration's events (`linear`, `slack`, …), with its config (JSON Schema in the catalog). */
  | { type: "integration"; source: string; events: string[]; config: unknown };

export type FilterOp = "equals" | "contains" | "any_of" | "has_label" | "glob" | "exists" | "gt" | "lt";

/** A condition on the run's context: `field` is a dotted path (`event.data.change.title`). */
export interface Filter {
  field: string;
  op: FilterOp;
  /** What `op` compares with (`null` for `exists`). */
  value: unknown;
  not?: boolean;
}

export type OnError = "fail" | "continue";

export interface Retry {
  attempts: number;
  backoff_s: number;
}

export interface Step {
  /** `[a-z0-9_]`, 1 to 32 characters: later steps read its output as `steps.<id>.…`. */
  id: string;
  name?: string;
  /** `agent.start`, `webhook`, `linear.comment`, … */
  kind: string;
  /** The kind's config (built-in kinds: the `*Config` types below); its strings are templates. */
  config: unknown;
  when: Filter[];
  on_error?: OnError;
  retry: Retry | null;
  timeout_s: number | null;
}

export type Concurrency = "parallel" | "skip" | "queue" | "cancel_previous";

export interface RateLimit {
  max: number;
  per_s: number;
}

export type Target =
  | { type: "machine"; machine: string }
  /** A new sandbox, deleted when the run ends unless `keep`. */
  | { type: "sandbox"; size: string; keep?: boolean }
  | { type: "any_online" }
  | { type: "project_machine" };

export type ProjectRef = { type: "id"; id: string } | { type: "repo"; repo: string; path: string | null };

export interface AgentDefaults {
  target: Target | null;
  project: ProjectRef | null;
  provider: ProviderId | null;
  model: string | null;
  effort: string | null;
  /** The model's other options by id (agent-proto `ModelOptions`). */
  options: ModelOptions | null;
  runtime_mode: RuntimeMode | null;
  interaction_mode: InteractionMode | null;
  worktree: boolean | null;
}

/** What the owner writes; `POST …/automations` takes one, `PUT …/{id}` replaces it whole. */
export interface AutomationSpec {
  name: string;
  description?: string;
  enabled: boolean;
  trigger: Trigger;
  filters: Filter[];
  steps: Step[];
  defaults?: AgentDefaults;
  concurrency: Concurrency;
  /** Unset: 100 runs an hour. */
  rate_limit: RateLimit | null;
  /** Runs act as `agent:<name>` instead of as the owner. */
  run_as: string | null;
  /** The owner's agent tokens may run it by hand. */
  allow_agents?: boolean;
}

export type RunState = "queued" | "running" | "waiting" | "succeeded" | "failed" | "cancelled" | "skipped";
export const isRunFinal = (s: RunState) => s === "succeeded" || s === "failed" || s === "cancelled" || s === "skipped";

export interface RunBrief {
  id: AutomationRunId;
  status: RunState;
  at: Millis;
}

export interface WebhookIngress {
  url: string;
  /** The secret's last four characters. */
  secret_hint: string;
}

export interface Automation extends AutomationSpec {
  id: AutomationId;
  revision: number;
  cursor: number;
  created_at: Millis;
  updated_at: Millis;
  created_by: string;
  /** Names only: values never leave the server. */
  secrets: string[];
  webhook: WebhookIngress | null;
  next_at: Millis | null;
  last_run: RunBrief | null;
}

export interface AutomationSaved {
  automation: Automation;
  /** A new inbound webhook secret (`rlh_…`), only in the answer that made it. */
  webhook_secret: string | null;
}

// ── the built-in steps' configs ──

export const STEP_KINDS = [
  "agent.start",
  "agent.wait",
  "agent.message",
  "agent.respond",
  "agent.publish",
  "agent.script",
  "agent.settle",
  "webhook",
  "notify",
  "delay",
] as const;
export type BuiltInStepKind = (typeof STEP_KINDS)[number];

export interface AgentStartConfig {
  target: Target | null;
  project: ProjectRef | null;
  provider: ProviderId | null;
  model: string | null;
  effort: string | null;
  /** The model's other options by id (agent-proto `ModelOptions`). */
  options: ModelOptions | null;
  runtime_mode: RuntimeMode | null;
  interaction_mode: InteractionMode | null;
  worktree: boolean | null;
  title: string | null;
  prompt: string;
  orchestrator?: boolean;
}

export type WaitFor = "done" | "failed" | "waiting" | "settled";

export interface AgentWaitConfig {
  /** A thread id template; unset: the run's latest thread. */
  thread: string | null;
  until: WaitFor[];
  timeout_s: number | null;
  on_timeout?: OnError;
}

export interface AgentMessageConfig {
  thread: string | null;
  text: string;
  mode: SendMode;
}

export interface RespondRule {
  action: ApprovalAction | null;
  command: string | null;
  files: string | null;
  tool: string | null;
  decision: Decision;
}

export interface AgentRespondConfig {
  thread: string | null;
  rules: RespondRule[];
  otherwise: Decision | null;
  answers?: Record<string, string>;
  first_option?: boolean;
  message: string | null;
}

export interface AgentPublishConfig {
  thread: string | null;
  message: string | null;
  land?: boolean;
}

export interface AgentScriptConfig {
  script: string;
  thread: string | null;
  project: ProjectRef | null;
  target: Target | null;
}

export type SettleAction = "settle" | "archive" | "unsettle";

export interface AgentSettleConfig {
  thread: string | null;
  action: SettleAction;
}

export interface WebhookConfig {
  url: string;
  method: string;
  /** Values may use `{{secrets.<name>}}`. */
  headers?: Record<string, string>;
  /** A template, or JSON whose strings are templates; unset: the event. */
  body: unknown;
  /** One of the automation's secrets: the body is signed with it (`X-Roll-Signature-256`). */
  secret: string | null;
  ok_statuses: number[];
}

export type NoticeLevel = "info" | "success" | "warning" | "error";

export interface NotifyConfig {
  title: string;
  body?: string;
  level: NoticeLevel;
  link: string | null;
}

export interface DelayConfig {
  seconds: number;
}

// ── events and runs ──

export interface AutomationEvent {
  id: AutomationEventId;
  source: string;
  kind: string;
  /** The idempotency key: `<source>:<the event's own id>`. */
  key: string;
  at: Millis;
  data: unknown;
  automation: AutomationId | null;
}

export interface EventBrief {
  id: AutomationEventId;
  source: string;
  kind: string;
  summary: string;
  at: Millis;
}

export type StepState = "pending" | "running" | "waiting" | "retrying" | "succeeded" | "failed" | "skipped" | "cancelled";
export type LogLevel = "info" | "warn" | "error";

export interface LogLine {
  at: Millis;
  level: LogLevel;
  text: string;
}

export interface StepRun {
  id: string;
  kind: string;
  status: StepState;
  attempt: number;
  started_at: Millis | null;
  finished_at: Millis | null;
  next_at: Millis | null;
  output: unknown;
  error: string | null;
  log: LogLine[];
}

/** A thread a run started: the canvas tags it with the automation. */
export interface RunThread {
  machine: MachineId;
  thread: ThreadId;
  step: string;
}

export interface AutomationRun {
  id: AutomationRunId;
  automation: AutomationId;
  name: string;
  status: RunState;
  event: EventBrief;
  steps: StepRun[];
  threads: RunThread[];
  cursor: number;
  created_at: Millis;
  started_at: Millis | null;
  finished_at: Millis | null;
  reason: string | null;
  /** When it was last retried: its time counts from here; the stopped attempt stays in its steps' logs. */
  retried_at?: Millis | null;
}

export interface RunDetail {
  run: AutomationRun;
  event: AutomationEvent;
}

export interface RunNow {
  /** `event.data.input` in templates. */
  input: unknown;
  /**
   * The event's data as its trigger would send it (an object): a run by hand of an automation a
   * code, agent or integration event starts. Its fields go into `event.data` beside `by` and `input`.
   */
  data?: Record<string, unknown> | null;
  /** The same key is the same run. */
  key: string | null;
}

/** A `notify` step's notice (the `notice` stream event: ephemeral, a toast). */
export interface Notice {
  id: string;
  title: string;
  body?: string;
  level: NoticeLevel;
  link: string | null;
  automation: AutomationId | null;
  run: AutomationRunId | null;
  /** The agent event its run started from: says when what it told (waiting on you) is over. */
  about?: NoticeAbout | null;
  at: Millis;
}

export interface NoticeAbout {
  event: AgentEvent;
  thread: ThreadId;
  request: string | null;
}

// ── the catalog and dry runs ──

export interface EventInfo {
  kind: string;
  title: string;
  fields: string[];
  /** A realistic `event.data`, for "Test" and autocomplete. */
  sample: unknown;
}

export interface TriggerInfo {
  source: string;
  title: string;
  description: string;
  events: EventInfo[];
  /** JSON Schema of an integration trigger's config. */
  config: unknown;
  needs_setup?: boolean;
}

export interface StepInfo {
  kind: string;
  title: string;
  description: string;
  /** JSON Schema of its config. */
  config: unknown;
  output: string[];
  integration: string | null;
  needs_setup?: boolean;
}

export interface Catalog {
  triggers: TriggerInfo[];
  steps: StepInfo[];
}

export interface TestEvent {
  source: string;
  kind: string;
  data: unknown;
}

export interface TestRequest {
  automation: AutomationSpec;
  event: TestEvent;
}

export interface TestStep {
  id: string;
  runs: boolean;
  config: unknown;
  error: string | null;
}

export interface TestResult {
  matched: boolean;
  trigger: boolean;
  filters: boolean[];
  steps: TestStep[];
}

// ── decoders ──

const strs = withDefault(array(str), () => []);
const opt = nullable;
const decisionD = literal("accept", "accept_for_session", "decline", "cancel");
const runtimeModeD = literal("full_access", "auto_edit", "ask");
const interactionModeD = literal("default", "plan");
const onErrorD = optional(literal("fail", "continue"));

export const triggerD: Decoder<Trigger> = tagged("type", {
  manual: obj<object>({}),
  schedule: obj<{ cron: string; timezone: string }>({
    cron: str,
    timezone: withDefault(str, () => "UTC"),
  }),
  interval: obj<{ every_s: number }>({ every_s: int }),
  webhook: obj<object>({}),
  code: obj<{ events: string[]; repos: string[] }>({
    events: strs,
    repos: strs,
  }),
  agent: obj<{ events: AgentEvent[] }>({
    events: array(literal(...AGENT_EVENTS)),
  }),
  integration: obj<{ source: string; events: string[]; config: unknown }>({
    source: str,
    events: strs,
    config: json,
  }),
} satisfies Record<Trigger["type"], Decoder<object>>);

export const filterD = obj<Filter>({
  field: str,
  op: literal("equals", "contains", "any_of", "has_label", "glob", "exists", "gt", "lt"),
  value: json,
  not: optional(bool),
});
const filtersD = withDefault(array(filterD), () => []);

export const stepD = obj<Step>({
  id: str,
  name: optional(str),
  kind: str,
  config: json,
  when: filtersD,
  on_error: onErrorD,
  retry: opt(obj<Retry>({ attempts: int, backoff_s: int })),
  timeout_s: opt(int),
});

export const targetD: Decoder<Target> = tagged("type", {
  machine: obj<{ machine: string }>({ machine: str }),
  sandbox: obj<{ size: string; keep?: boolean }>({
    size: withDefault(str, () => "standard"),
    keep: optional(bool),
  }),
  any_online: obj<object>({}),
  project_machine: obj<object>({}),
} satisfies Record<Target["type"], Decoder<object>>);

export const projectRefD: Decoder<ProjectRef> = tagged("type", {
  id: obj<{ id: string }>({ id: str }),
  repo: obj<{ repo: string; path: string | null }>({
    repo: str,
    path: opt(str),
  }),
} satisfies Record<ProjectRef["type"], Decoder<object>>);

const agentFields = {
  target: opt(targetD),
  project: opt(projectRefD),
  provider: opt(str),
  model: opt(str),
  effort: opt(str),
  options: opt(map(strOrBool)),
  runtime_mode: opt(runtimeModeD),
  interaction_mode: opt(interactionModeD),
  worktree: opt(bool),
};
export const agentDefaultsD = obj<AgentDefaults>(agentFields);

const specFields = {
  name: str,
  description: optional(str),
  enabled: withDefault(bool, () => true),
  trigger: triggerD,
  filters: filtersD,
  steps: array(stepD),
  defaults: optional(agentDefaultsD),
  concurrency: withDefault(literal("parallel", "skip", "queue", "cancel_previous"), () => "parallel" as const),
  rate_limit: opt(obj<RateLimit>({ max: int, per_s: int })),
  run_as: opt(str),
  allow_agents: optional(bool),
};
export const automationSpecD = obj<AutomationSpec>(specFields);

const runStateD = literal("queued", "running", "waiting", "succeeded", "failed", "cancelled", "skipped");

export const automationD = obj<Automation>({
  ...specFields,
  id: str,
  revision: int,
  cursor: int,
  created_at: int,
  updated_at: int,
  created_by: str,
  secrets: strs,
  webhook: opt(obj<WebhookIngress>({ url: str, secret_hint: str })),
  next_at: opt(int),
  last_run: opt(obj<RunBrief>({ id: str, status: runStateD, at: int })),
});

export const automationSavedD = obj<AutomationSaved>({
  automation: automationD,
  webhook_secret: opt(str),
});

export const agentStartD = obj<AgentStartConfig>({
  ...agentFields,
  title: opt(str),
  prompt: str,
  orchestrator: optional(bool),
});
export const agentWaitD = obj<AgentWaitConfig>({
  thread: opt(str),
  until: withDefault(array(literal("done", "failed", "waiting", "settled")), () => []),
  timeout_s: opt(int),
  on_timeout: onErrorD,
});
export const agentMessageD = obj<AgentMessageConfig>({
  thread: opt(str),
  text: str,
  mode: withDefault(literal("queue", "steer"), () => "queue" as const),
});
export const agentRespondD = obj<AgentRespondConfig>({
  thread: opt(str),
  rules: withDefault(
    array(
      obj<RespondRule>({
        action: opt(literal("command", "file_change", "permission", "tool", "plan")),
        command: opt(str),
        files: opt(str),
        tool: opt(str),
        decision: decisionD,
      }),
    ),
    () => [],
  ),
  otherwise: opt(decisionD),
  answers: optional(map(str)),
  first_option: optional(bool),
  message: opt(str),
});
export const agentPublishD = obj<AgentPublishConfig>({
  thread: opt(str),
  message: opt(str),
  land: optional(bool),
});
export const agentScriptD = obj<AgentScriptConfig>({
  script: str,
  thread: opt(str),
  project: opt(projectRefD),
  target: opt(targetD),
});
export const agentSettleD = obj<AgentSettleConfig>({
  thread: opt(str),
  action: withDefault(literal("settle", "archive", "unsettle"), () => "settle" as const),
});
export const webhookConfigD = obj<WebhookConfig>({
  url: str,
  method: withDefault(str, () => "POST"),
  headers: optional(map(str)),
  body: json,
  secret: opt(str),
  ok_statuses: withDefault(array(int), () => []),
});
const noticeLevelD = withDefault(literal("info", "success", "warning", "error"), () => "info" as const);
export const notifyConfigD = obj<NotifyConfig>({
  title: str,
  body: optional(str),
  level: noticeLevelD,
  link: opt(str),
});
export const delayConfigD = obj<DelayConfig>({ seconds: int });

/** The decoder of each built-in step kind's config. */
export const STEP_CONFIGS: Record<BuiltInStepKind, Decoder<unknown>> = {
  "agent.start": agentStartD,
  "agent.wait": agentWaitD,
  "agent.message": agentMessageD,
  "agent.respond": agentRespondD,
  "agent.publish": agentPublishD,
  "agent.script": agentScriptD,
  "agent.settle": agentSettleD,
  webhook: webhookConfigD,
  notify: notifyConfigD,
  delay: delayConfigD,
};

export const automationEventD = obj<AutomationEvent>({
  id: str,
  source: str,
  kind: str,
  key: str,
  at: int,
  data: json,
  automation: opt(str),
});

const stepRunD = obj<StepRun>({
  id: str,
  kind: str,
  status: literal("pending", "running", "waiting", "retrying", "succeeded", "failed", "skipped", "cancelled"),
  attempt: withDefault(int, () => 0),
  started_at: opt(int),
  finished_at: opt(int),
  next_at: opt(int),
  output: json,
  error: opt(str),
  log: withDefault(
    array(
      obj<LogLine>({
        at: int,
        level: withDefault(literal("info", "warn", "error"), () => "info" as const),
        text: str,
      }),
    ),
    () => [],
  ),
});

export const automationRunD = obj<AutomationRun>({
  id: str,
  automation: str,
  name: str,
  status: runStateD,
  event: obj<EventBrief>({
    id: str,
    source: str,
    kind: str,
    summary: str,
    at: int,
  }),
  steps: array(stepRunD),
  threads: withDefault(array(obj<RunThread>({ machine: str, thread: str, step: str })), () => []),
  cursor: int,
  created_at: int,
  started_at: opt(int),
  finished_at: opt(int),
  reason: opt(str),
  retried_at: opt(int),
});

export const runDetailD = obj<RunDetail>({
  run: automationRunD,
  event: automationEventD,
});
export const runNowD = obj<RunNow>({ input: json, data: opt(json) as never, key: opt(str) });

export const noticeD = obj<Notice>({
  id: str,
  title: str,
  body: optional(str),
  level: noticeLevelD,
  link: opt(str),
  automation: opt(str),
  run: opt(str),
  about: optional(obj<NoticeAbout>({ event: literal(...AGENT_EVENTS), thread: str as Decoder<ThreadId>, request: opt(str) })),
  at: int,
});

const eventInfoD = obj<EventInfo>({ kind: str, title: str, fields: strs, sample: json });
export const catalogD = obj<Catalog>({
  triggers: array(
    obj<TriggerInfo>({
      source: str,
      title: str,
      description: str,
      events: withDefault(array(eventInfoD), () => []),
      config: json,
      needs_setup: optional(bool),
    }),
  ),
  steps: array(
    obj<StepInfo>({
      kind: str,
      title: str,
      description: str,
      config: json,
      output: strs,
      integration: opt(str),
      needs_setup: optional(bool),
    }),
  ),
});

export const testRequestD = obj<TestRequest>({
  automation: automationSpecD,
  event: obj<TestEvent>({ source: str, kind: str, data: json }),
});
export const testResultD = obj<TestResult>({
  matched: bool,
  trigger: bool,
  filters: array(bool),
  steps: array(obj<TestStep>({ id: str, runs: bool, config: json, error: opt(str) })),
});

// ── the client ──

/** How the client reaches the API: `createAgentsApi`'s request function (it throws `ApiError`s). */
export type Call = (method: string, path: string, body?: unknown) => Promise<unknown>;

const enc = encodeURIComponent;

/** Query options of the run lists. */
export interface RunsQuery {
  /** Runs made before this one (paging, newest first). */
  before?: AutomationRunId;
  limit?: number;
  status?: RunState;
}

function runsQuery(q: RunsQuery & { automation?: AutomationId } = {}): string {
  return query(q);
}

function query(q: object): string {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== null) p.set(k, String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
}

/** `/v1/code/agents/{owner}/automations…`. */
export function createAutomationsApi(call: Call) {
  const base = (owner: string) => `/v1/code/agents/${enc(owner)}/automations`;
  return {
    list: async (owner: string) => decode(array(automationD), await call("GET", base(owner))),
    get: async (owner: string, id: AutomationId) => decode(automationD, await call("GET", `${base(owner)}/${enc(id)}`)),
    create: async (owner: string, spec: AutomationSpec) => decode(automationSavedD, await call("POST", base(owner), spec)),
    /** Replace the definition; `revision` (the one the edit started from) makes a stale save a 409. */
    update: async (owner: string, id: AutomationId, spec: AutomationSpec, revision?: number) =>
      decode(automationSavedD, await call("PUT", `${base(owner)}/${enc(id)}`, revision === undefined ? spec : { ...spec, revision })),
    remove: async (owner: string, id: AutomationId) => {
      await call("DELETE", `${base(owner)}/${enc(id)}`);
    },
    enable: async (owner: string, id: AutomationId) => decode(automationD, await call("POST", `${base(owner)}/${enc(id)}/enable`)),
    disable: async (owner: string, id: AutomationId) => decode(automationD, await call("POST", `${base(owner)}/${enc(id)}/disable`)),
    /** Run it now; the same `key` is the same run. */
    run: async (owner: string, id: AutomationId, req: Partial<RunNow> = {}) =>
      decode(
        automationRunD,
        await call("POST", `${base(owner)}/${enc(id)}/run`, {
          input: req.input ?? null,
          ...(req.data ? { data: req.data } : {}),
          key: req.key ?? null,
        }),
      ),
    runsOf: async (owner: string, id: AutomationId, q: RunsQuery = {}) =>
      decode(array(automationRunD), await call("GET", `${base(owner)}/${enc(id)}/runs${runsQuery(q)}`)),
    /** Every run of the owner's automations, newest first. */
    runs: async (owner: string, q: RunsQuery & { automation?: AutomationId } = {}) =>
      decode(array(automationRunD), await call("GET", `${base(owner)}/runs${runsQuery(q)}`)),
    runDetail: async (owner: string, run: AutomationRunId) => decode(runDetailD, await call("GET", `${base(owner)}/runs/${enc(run)}`)),
    /** The notify steps' notices, newest first: kept for a dashboard that wasn't open (`before`: a notice's `at`). */
    notices: async (owner: string, q: { before?: number; limit?: number } = {}) => decode(array(noticeD), await call("GET", `${base(owner)}/notices${query(q)}`)),
    cancel: async (owner: string, run: AutomationRunId) =>
      decode(automationRunD, await call("POST", `${base(owner)}/runs/${enc(run)}/cancel`)),
    /** Run a failed or cancelled run again from the step that stopped it. */
    retry: async (owner: string, run: AutomationRunId) =>
      decode(automationRunD, await call("POST", `${base(owner)}/runs/${enc(run)}/retry`)),
    /** Set one of its secrets (sealed on the server; never readable again). */
    setSecret: async (owner: string, id: AutomationId, name: string, value: string) => {
      await call("PUT", `${base(owner)}/${enc(id)}/secrets/${enc(name)}`, {
        value,
      });
    },
    removeSecret: async (owner: string, id: AutomationId, name: string) => {
      await call("DELETE", `${base(owner)}/${enc(id)}/secrets/${enc(name)}`);
    },
    /** A new inbound webhook secret (the old one stops working); shown once. */
    rotateWebhookSecret: async (owner: string, id: AutomationId) =>
      decode(automationSavedD, await call("POST", `${base(owner)}/${enc(id)}/webhook/secret`)),
    catalog: async (owner: string) => decode(catalogD, await call("GET", `${base(owner)}/catalog`)),
    /** What an automation would do with an event, without doing it. */
    test: async (owner: string, req: TestRequest) => decode(testResultD, await call("POST", `${base(owner)}/test`, req)),
  };
}

export type AutomationsApi = ReturnType<typeof createAutomationsApi>;
