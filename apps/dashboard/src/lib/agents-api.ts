/**
 * roll code agents: the wire types and the relay API client (docs/code/agents.md; the routes are in
 * docs/code/http-api.md, "Agents").
 *
 * The types mirror `crates/agent-proto/src/lib.rs` field for field. Each has a decoder that fills
 * serde's defaults (`Option` → `null`, `Vec` → `[]`), so received records are always complete;
 * commands keep optional fields `undefined` so they serialize exactly as Rust writes them.
 * `agents-api.test.ts` decodes every file in `crates/agent-proto/fixtures/` strictly, so drift on
 * either side fails a test.
 */

import { ApiError } from "./code-api";
import { getToken, requestSignIn, withSession } from "./code-token";
import {
  array,
  bool,
  decode,
  DecodeError,
  int,
  json,
  literal,
  map,
  nullable,
  num,
  obj,
  optional,
  str,
  strOrBool,
  tagged,
  withDefault,
  type DecodeCtx,
  type Decoder,
} from "./decode";
import { createIntegrationsApi } from "./integrations-api";
import {
  automationD,
  automationRunD,
  createAutomationsApi,
  noticeD,
  type Automation,
  type AutomationId,
  type AutomationRun,
  type Notice,
} from "./automations-api";
import { createLinearApi } from "./linear-api";
import { createGatewayApi } from "./agents-gateway";
import { createLookupApi } from "./lookup-api";
import { ROLL_API } from "./roll-api";

/** Milliseconds since the Unix epoch. */
export type Millis = number;

/** `mch_…`, assigned by the relay. */
export type MachineId = string;
export type ProjectId = string;
export type ThreadId = string;
export type RunId = string;
export type NodeId = string;
export type RequestId = string;
export type CheckpointId = string;
export type TerminalId = string;
/** `codex`, `claude`, or `acp:<name>`. */
export type ProviderId = string;

// ── entities ──

export interface RepoRef {
  owner: string;
  repo: string;
}

export interface GitInfo {
  branch: string | null;
  head: string | null;
  remote: string | null;
  dirty: boolean;
}

export interface Project {
  id: ProjectId;
  name: string;
  path: string;
  repo: RepoRef | null;
  git: GitInfo | null;
  /** What the owner set (`project.update`). */
  settings: ProjectSettings;
  /** What the last pull did (auto-pull or `project.pull`). */
  pull: PullResult | null;
  /** The project's own logo, as its daemon found it (docs/code/agents.md, "Identity"). */
  logo: ProjectLogo | null;
  created_at: Millis;
  updated_at: Millis;
}

/**
 * A categorical colour for a project or a machine (`--id-<name>` tokens in globals.css). None is
 * blue (the accent means busy), orange, amber or yellow. `lib/agents-identity.ts` has the rest.
 */
export type IdentityColor = "slate" | "violet" | "purple" | "pink" | "rose" | "teal" | "cyan" | "green";
export const IDENTITY_COLORS: readonly IdentityColor[] = ["slate", "violet", "purple", "pink", "rose", "teal", "cyan", "green"];

/** How a project shows itself: its logo (else the monogram), the monogram, an emoji, or a picture in the project. */
export type ProjectIcon = { kind: "auto" } | { kind: "monogram"; text?: string } | { kind: "emoji"; emoji: string } | { kind: "file"; path: string };

/** A project's logo: where it is in the project, and the picture as a `data:image/…` URL (shown only in an `<img>`). */
export interface ProjectLogo {
  path: string;
  url: string;
}

export type MachineKind = "laptop" | "desktop" | "server" | "cloud" | "container";

/** `PATCH …/machines/{machine}`: a colour or kind to set, `null` to clear one, absent to keep it. */
export interface MachineStyle {
  color?: IdentityColor | null;
  kind?: MachineKind | null;
}

/** A machine's colour and kind as its owner set them on the relay (unset: from its id and OS). */
export interface MachineAppearance {
  color?: IdentityColor;
  kind?: MachineKind;
}

/** A project's settings (T3's per-project settings); `project.update` replaces them whole. */
export interface ProjectSettings {
  /** Fast-forward the checkout's default branch when the remote's moves, while it is clean. */
  auto_pull: boolean;
  /** What new threads in the project start with. */
  defaults: ThreadDefaults;
  /** Commands for the checkout or a thread's worktree (T3's project scripts). */
  scripts: ProjectScript[];
  /** When the daemon settles the project's threads by itself. */
  auto_settle: AutoSettle;
  /** What the daemon cleans up after settled and archived threads (unset: nothing). */
  cleanup?: Cleanup;
  /** Its colour (unset: made from its repo or name, the same on every device). */
  color?: IdentityColor;
  /** Its logo, monogram or emoji (unset: `auto`). */
  icon?: ProjectIcon;
}

/** T3's auto-settle settings, per project. */
export interface AutoSettle {
  /** Days without activity before a thread settles; `null` never (1–90). */
  after_days: number | null;
  /** Settle a thread when its change lands (a closed change always settles it). */
  on_land: boolean;
  /** Inactivity never settles a thread whose published change is still open (T3's `sidebarAutoSettleKeepPullRequests`). */
  keep_changes?: boolean;
}

export const DEFAULT_AUTO_SETTLE: AutoSettle = { after_days: 3, on_land: true };

/**
 * T3's worktree cleanup, per project (off by default, as there): the worktree of a thread settled
 * or archived `worktree_after_days` ago goes, unless it has local changes. Its branch and
 * checkpoints stay, and its next message, rollback or terminal puts it back.
 */
export interface Cleanup {
  /** 0–3650 days; unset never. */
  worktree_after_days?: number | null;
  /** Which local files keep a worktree (default `uncommitted_changes`). */
  keep?: CleanupKeep;
}

export type CleanupKeep = "uncommitted_changes" | "any_local_files";
export const CLEANUP_MAX_DAYS = 3650;

/** Why cleanup left a worktree where it was. */
export type KeptReason = "local_changes" | "busy" | "terminal" | "other_branch" | "shared" | "not_managed" | "failed";

export interface CleanupReport {
  removed: ThreadId[];
  kept: { thread: ThreadId; reason: KeptReason }[];
}

/** Unset fields fall back to the composer's last choice. */
export interface ThreadDefaults {
  provider?: ProviderId;
  model?: string;
  effort?: string;
  /** The model's other options ({@link ModelInfo.options}). */
  options?: ModelOptions;
  runtime_mode?: RuntimeMode;
  interaction_mode?: InteractionMode;
  /** New threads get their own worktree (`true`) or work in the checkout (`false`). */
  worktree?: boolean;
}

export type ScriptIcon = "play" | "test" | "lint" | "configure" | "build" | "debug";

export interface ProjectScript {
  id: string;
  name: string;
  command: string;
  icon: ScriptIcon;
  /** Runs in each new worktree, beside the agent (the setup script). */
  on_worktree: boolean;
}

export type PullSkip = "not_git" | "detached" | "other_branch" | "no_upstream" | "local_changes" | "diverged" | "busy" | "no_access";

export type PullOutcome =
  | { status: "pulled"; branch: string; from: string; to: string; commits: number }
  | { status: "up_to_date"; branch: string }
  | { status: "skipped"; reason: PullSkip; message: string }
  | { status: "failed"; message: string };

export interface PullResult {
  at: Millis;
  outcome: PullOutcome;
}

export const NO_SETTINGS: ProjectSettings = { auto_pull: false, defaults: {}, scripts: [], auto_settle: DEFAULT_AUTO_SETTLE };

export type RuntimeMode = "full_access" | "auto_edit" | "ask";
export type InteractionMode = "default" | "plan";
export type ThreadStatus = "idle" | "starting" | "running" | "waiting" | "error";
export type Relation = "fork" | "subagent";

export interface Worktree {
  path: string;
  branch: string;
  base: string;
  /** When cleanup removed its folder; the next message, rollback or terminal puts it back. */
  removed_at: Millis | null;
}

export interface Lineage {
  parent: ThreadId;
  relation: Relation;
  root: ThreadId;
  from_run: RunId | null;
}

export interface Attachment {
  name: string;
  mime: string;
  /** Base64 (images; at most 5 MiB decoded). */
  data: string;
}

export interface QueuedMessage {
  id: string;
  text: string;
  attachments: Attachment[];
  at: Millis;
  /** Delegated tasks' results waiting behind the active run (a wake); empty for a person's message. */
  delivers?: NodeId[];
}

export interface Usage {
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  cost_usd: number | null;
}

/**
 * How full a conversation's context window is (T3's `ThreadTokenUsageSnapshot`). `used_tokens`
 * is what the next request carries; it drops after a compaction. No `max_tokens`: the window is
 * unknown, so show the tokens and never a percentage.
 */
export interface ContextUsage {
  used_tokens: number;
  max_tokens: number | null;
  /** Every token the conversation's model calls processed (Codex reports it). */
  total_processed_tokens: number | null;
  /** How long the prompt cache lives after the last request (Claude: 1 h on subscriptions, 5 min on API keys; T3's `promptCacheTtlMs`). */
  cache_ttl_ms?: number | null;
  /** Where the provider compacts by itself, when a setting moved it below the window (T3's `autoCompactThreshold`). */
  auto_compact_at?: number | null;
}

export interface Thread {
  id: ThreadId;
  project: ProjectId;
  title: string;
  provider: ProviderId;
  model: string | null;
  effort: string | null;
  /** The model's other options, by id (a speed, a context window, …). */
  options: ModelOptions;
  runtime_mode: RuntimeMode;
  interaction_mode: InteractionMode;
  cwd: string;
  worktree: Worktree | null;
  status: ThreadStatus;
  lineage: Lineage | null;
  /** An orchestrator: its agent plans the goal it was given and hands the parts to subagents. */
  orchestrator: boolean;
  active_run: RunId | null;
  queued: QueuedMessage[];
  run_count: number;
  pending_requests: number;
  /** The whole thread's changes (its base checkpoint to its files now: the "Whole thread" diff's numbers, terminal edits included); a run's `stat` is only that run's. Null until the daemon first counts it, and from daemons before it. */
  diff: DiffStat | null;
  /** The newest run completed with a proposed plan (T3's `hasActionableProposedPlan`): "Plan ready" on the canvas while in plan mode. */
  plan_ready: boolean;
  /** One line about what the agent is doing right now. */
  activity: string | null;
  usage: Usage;
  /** The conversation's context window as its provider last reported it. */
  context: ContextUsage | null;
  error: string | null;
  /** The roll code change it was published as, as roll code sees it now (the relay's). */
  change: ThreadChange | null;
  /** The roll code work item it took (the relay's). */
  work: WorkRef | null;
  created_at: Millis;
  updated_at: Millis;
  archived_at: Millis | null;
  /** Settled, snoozed, seen: kept by the daemon, so every device agrees. */
  inbox: Inbox;
}

/** Where a thread sits in the inbox (T3's settle and snooze state). */
export interface Inbox {
  /** Settled, or kept active by hand; `null`: the daemon settles it by the project's rules. */
  settle: Settle | null;
  snooze: Snooze | null;
  /** When a snooze last woke by itself: "Woke" until visited after it. */
  woke_at: Millis | null;
  /** When the conversation was last opened (`thread.visit`). */
  visited_at: Millis | null;
  /** The daemon never settles it by itself. */
  manual: boolean;
}

export type SettledBy = "user" | "inactivity" | "landed" | "closed";
export type Settle = { state: "settled"; at: Millis; by: SettledBy } | { state: "active"; at: Millis };

export interface Snooze {
  until: Millis;
  at: Millis;
}

/** The inbox before `thread.settle`, as this device saw it: what undoing the settle puts back. */
export interface InboxBefore {
  /** `null`: back under the project's auto-settle rules. */
  settle: Settle | null;
  snooze: Snooze | null;
}

export const EMPTY_INBOX: Inbox = { settle: null, snooze: null, woke_at: null, visited_at: null, manual: false };

export type ChangeStatus = "checking" | "failing" | "blocked" | "ready" | "queued" | "landed" | "conflict" | "closed";

export interface ThreadChange {
  /** `owner/name`. */
  repo: string;
  number: number;
  status: ChangeStatus;
  url: string;
  /** One line on why (failing checks, the first blocker, the queue position). */
  detail: string | null;
}

export interface WorkRef {
  id: string;
  repo: string;
  number: number;
  title: string;
}

export type RunStatus =
  | "queued"
  | "starting"
  | "running"
  | "waiting"
  | "completed"
  | "interrupted"
  | "failed"
  | "cancelled"
  | "rolled_back";

export interface DiffStat {
  files: number;
  additions: number;
  deletions: number;
}

export interface Run {
  id: RunId;
  thread: ThreadId;
  ordinal: number;
  provider: ProviderId;
  model: string | null;
  status: RunStatus;
  prompt: string;
  attachments: Attachment[];
  steers: string[];
  /** When each steer came (one per steer); absent from older daemons. */
  steered_at?: Millis[];
  /** The daemon compacted the idle conversation before its prompt cache expired (T3's idle compaction). */
  idle_compaction?: boolean;
  /**
   * The daemon started it with these delegated tasks' results, waking the thread that delegated
   * them (T3's "Delegated task finished"); `prompt` is what the agent was told. Empty for a person's run.
   */
  delivers?: NodeId[];
  requested_at: Millis;
  started_at: Millis | null;
  completed_at: Millis | null;
  checkpoint: CheckpointId | null;
  stat: DiffStat | null;
  /** What `stat` counts, per file, from the same checkpoint diff (at most 200; empty from older daemons): the changed-files card shows these, so it agrees with Open diff. */
  files?: FileStat[];
  usage: Usage;
  /** The context window as the run left it. */
  context: ContextUsage | null;
  error: string | null;
}

export type NodeStatus = "running" | "waiting" | "completed" | "failed" | "interrupted";
export type TodoStatus = "pending" | "in_progress" | "completed";
export type ToolKind = "command" | "edit" | "read" | "search" | "web" | "mcp" | "other";
export type ChangeKind = "add" | "modify" | "delete" | "rename";

export interface Todo {
  text: string;
  status: TodoStatus;
}

/** One file of a checkpoint diff (the repository's path, the new one for a rename). */
export interface FileStat {
  path: string;
  change: ChangeKind;
  additions: number;
  deletions: number;
}

export interface FileChange {
  path: string;
  change: ChangeKind;
  diff: string | null;
}

export interface ToolCall {
  name: string;
  kind: ToolKind;
  title: string;
  input: unknown;
  output: string | null;
  exit_code: number | null;
  files: FileChange[];
}

export type NodeBody =
  | { type: "message"; text: string }
  | { type: "reasoning"; text: string }
  | { type: "plan"; text: string }
  | { type: "todos"; items: Todo[] }
  | ({ type: "tool" } & ToolCall)
  | SubagentBody
  /** The provider compacted the context (T3's compaction row); the counts are the window before and after. */
  | { type: "compaction"; before_tokens: number | null; after_tokens: number | null; error: string | null }
  | { type: "request"; request: RequestId }
  | { type: "system"; text: string };

/** A subagent the provider started, or a delegated roll thread (`child` set). */
export interface SubagentBody {
  type: "subagent";
  agent: string;
  prompt: string;
  child: ThreadId | null;
  result: string | null;
  /** What it's doing now, while it runs. */
  progress?: string | null;
  model?: string | null;
}

export interface Node {
  id: NodeId;
  thread: ThreadId;
  run: RunId;
  parent: NodeId | null;
  ordinal: number;
  status: NodeStatus;
  started_at: Millis;
  completed_at: Millis | null;
  body: NodeBody;
}

export type RequestStatus = "pending" | "resolved" | "cancelled";
export type ApprovalAction = "command" | "file_change" | "permission" | "tool" | "plan";
export type Decision = "accept" | "accept_for_session" | "decline" | "cancel";

export interface Approval {
  action: ApprovalAction;
  title: string;
  command: string | null;
  cwd: string | null;
  files: FileChange[];
  reason: string | null;
  tool: string | null;
  input: unknown;
  decisions: Decision[];
}

export interface QuestionOption {
  label: string;
  description: string | null;
}

export interface Question {
  id: string;
  header: string;
  question: string;
  options: QuestionOption[];
  multi: boolean;
}

export type RequestBody = ({ type: "approval" } & Approval) | { type: "questions"; questions: Question[] };

export type RequestResponse =
  | { type: "decision"; decision: Decision; message?: string }
  /** Answers keyed by question id; a multi-select answer joins labels with ", ". */
  | { type: "answers"; answers: Record<string, string> };

export interface Request {
  id: RequestId;
  thread: ThreadId;
  run: RunId;
  node: NodeId;
  status: RequestStatus;
  body: RequestBody;
  created_at: Millis;
  resolved_at: Millis | null;
  resolved_by: string | null;
  response: RequestResponse | null;
}

export interface Checkpoint {
  id: CheckpointId;
  thread: ThreadId;
  run: RunId | null;
  ordinal: number;
  commit: string;
  stat: DiffStat;
  created_at: Millis;
}

export type TerminalStatus = "running" | "exited";

export interface Terminal {
  id: TerminalId;
  project: ProjectId | null;
  thread: ThreadId | null;
  cwd: string;
  title: string;
  status: TerminalStatus;
  exit_code: number | null;
  cols: number;
  rows: number;
  created_at: Millis;
}

export type EntityKind = "project" | "thread" | "run" | "node" | "request" | "checkpoint" | "terminal";

export interface Removed {
  entity: EntityKind;
  id: string;
  thread: ThreadId | null;
}

export type Entity =
  | ({ kind: "project" } & Project)
  | ({ kind: "thread" } & Thread)
  | ({ kind: "run" } & Run)
  | ({ kind: "node" } & Node)
  | ({ kind: "request" } & Request)
  | ({ kind: "checkpoint" } & Checkpoint)
  | ({ kind: "terminal" } & Terminal)
  | ({ kind: "removed" } & Removed);

// ── machines ──

export type AuthStatus = "ready" | "missing" | "unknown";

/** What an option is for, so it shows where it belongs (agent-proto `OptionRole`). */
export type OptionRole = "reasoning" | "speed" | "context_window";

export interface OptionChoice {
  id: string;
  label: string;
  description?: string;
}

/** One thing a model can be asked for (T3's `ProviderOptionDescriptor`; agent-proto `ModelOption`). */
export type ModelOption =
  | { type: "select"; id: string; label: string; description?: string; choices: OptionChoice[]; default?: string; role?: OptionRole }
  | { type: "boolean"; id: string; label: string; description?: string; default: boolean; role?: OptionRole };

export type OptionValue = string | boolean;

/** A thread's picked options besides its effort, by option id. */
export type ModelOptions = Record<string, OptionValue>;

/** The reasoning option's id: its value is a thread's `effort`, never one of its `options`. */
export const EFFORT_OPTION = "effort";

export interface ModelInfo {
  id: string;
  name: string;
  /** What a narrow place calls it ("Opus 5.5" for "Claude Opus 5.5"). */
  short_name?: string;
  default: boolean;
  /** Its reasoning levels (role `reasoning`, the thread's `effort`) and other options. */
  options: ModelOption[];
  /** An older model, folded under "Show more". */
  legacy?: boolean;
  /** `false`: it takes text only (unset: images too, as most models do). */
  images?: boolean;
  /** One of the machine's custom models (its settings), not one the provider lists. */
  custom?: boolean;
  /** Listed by an endpoint the provider uses (its name), not by the provider itself. */
  via?: string;
  /** T3's "New" badge: a week after a current model's release. */
  badge?: "new";
}

/** A model the provider would offer after an update. */
export interface UpdateRequired {
  id: string;
  name: string;
  min_version: string;
}

export interface Capabilities {
  steer: boolean;
  fork: boolean;
  rollback: boolean;
  plan_mode: boolean;
  images: boolean;
  subagents: boolean;
  mode_switch: boolean;
  /** `/compact` compacts the context (the context meter's Compact action). */
  compact?: boolean;
}

export interface ProviderInfo {
  id: ProviderId;
  name: string;
  installed: boolean;
  version: string | null;
  auth: AuthStatus;
  detail: string | null;
  models: ModelInfo[];
  /** Models it would offer after an update ("Update Claude Code to v… to use …"). */
  update_required: UpdateRequired[];
  capabilities: Capabilities;
  /**
   * The daemon is still probing it (it says hello without waiting). The rest is the last probe's,
   * or not installed with its login unknown when there was none.
   */
  checking?: boolean;
  /**
   * How much of the account's allowance is left, as the provider's own CLI reports it (Codex,
   * Claude Code, Muse). Absent for a provider that reports none.
   */
  usage_limits?: UsageLimits;
}

/** T3's `ServerProviderUsageLimits`: windows sorted session, weekly, monthly, other. */
export interface UsageLimits {
  checked_at: Millis;
  windows: UsageWindow[];
  plan?: string;
}

export type UsageWindowKind = "session" | "weekly" | "monthly" | "other";

export interface UsageWindow {
  id: string;
  kind: UsageWindowKind;
  label: string;
  /** 0 to 100. */
  used_percent: number;
  resets_at?: Millis;
  window_minutes?: number;
}

export interface MachineInfo {
  hostname: string;
  os: string;
  arch: string;
  version: string;
  home: string;
  /** The providers on offer: turned-off ones are left out (they are in `settings`). */
  providers: ProviderInfo[];
  /** The machine's settings (`null`: a daemon from before machine settings). */
  settings: MachineConfig | null;
  /** The daemon's X25519 public key (base64url): endpoint keys are sealed to it (`agents-seal.ts`). */
  seal_key?: string;
}

// ── endpoints (docs/code/agents.md, "Endpoints"; agent-proto `endpoint.rs`) ──

/**
 * What an endpoint speaks: which variables a provider gets and how its models are listed.
 * `gateway` is roll's model gateway on another of the owner's machines, set by the relay when a
 * machine joins it (id `roll-gateway`; docs/code/gateway.md): the generic editor never edits it.
 */
export type EndpointKind = "anthropic" | "openai" | "cliproxy" | "custom" | "gateway";

/** An endpoint as the owner sets it: everything but the key. */
export interface EndpointSpec {
  /** `[a-z0-9_-]`, at most 64; the same id on several machines is one profile applied to them. */
  id: string;
  name: string;
  kind: EndpointKind;
  base_url: string;
  /** The providers that use it (a provider uses at most one endpoint on a machine). */
  providers?: ProviderId[];
  /** `custom`: the variable the base URL goes in. */
  url_env?: string;
  /** `custom`: the variable the key goes in. */
  key_env?: string;
  /** The key is read on the machine from 1Password (`op://vault/item/field`). */
  key_ref?: string;
  /** When it doesn't answer as a provider's process starts, the provider starts on its own login or keys instead of failing. */
  fallback?: boolean;
}

/** A key sealed to one daemon (`roll-seal-v1`; `agents-seal.ts`). The relay only ever sees this. */
export interface SealedSecret {
  v: number;
  epk: string;
  nonce: string;
  ct: string;
}

/** What `endpoint.set` does to the endpoint's key. */
export type KeyChange = { type: "keep" } | { type: "clear" } | { type: "set"; sealed: SealedSecret };

/** Where an endpoint's key comes from. Never any part of the key. */
export type KeyState = "none" | "sealed" | "reference";

export interface EndpointModel {
  id: string;
  name?: string;
}

/** An endpoint as its daemon reports it. */
export interface EndpointInfo extends EndpointSpec {
  key: KeyState;
  /** What its `/models` listed at the last check. */
  models?: EndpointModel[];
  checked_at?: Millis;
  /** Why the last check failed (never holds the key). */
  error?: string;
}

/** `endpoint.check`'s answer. */
export interface EndpointCheck {
  ok: boolean;
  models: number;
  ms: number;
  error?: string;
}

/** An owner's endpoint kept on the relay without a key, to apply to machines. */
export interface EndpointProfile extends EndpointSpec {
  /** Offered first, and picked, in the new-sandbox dialog. */
  sandbox_default?: boolean;
  updated_at: Millis;
}

// ── the model gateway (docs/code/gateway.md; agent-proto `gateway.rs`) ──

/** The limits `GatewaySettings::check` holds settings to, and the queries' caps (agent-proto `gateway_limits`). */
export const GATEWAY_LIMITS = {
  id: 64,
  name: 64,
  url: 2048,
  upstreams: 32,
  routes: 256,
  targets: 16,
  models: 256,
  model: 256,
  listen: 8,
  clients: 256,
  headers: 16,
  header: 1024,
  logPage: 200,
  usageRows: 5000,
  /** The members' endpoint id on every machine that uses the gateway. */
  endpointId: "roll-gateway",
} as const;

/** An API a request or an upstream speaks. */
export type WireFormat = "messages" | "chat" | "responses";

/** Who an upstream is: its URL, how it's authenticated, what it speaks. API keys and cloud credentials only. */
export type UpstreamKind = "anthropic" | "openai" | "azure_openai" | "bedrock" | "vertex" | "openrouter" | "anthropic_compatible" | "openai_compatible";
export const UPSTREAM_KINDS: readonly UpstreamKind[] = ["anthropic", "openai", "azure_openai", "bedrock", "vertex", "openrouter", "anthropic_compatible", "openai_compatible"];

/** An upstream as the owner sets it: everything but its key. */
export interface UpstreamSpec {
  /** `[a-z0-9_-]`, at most 64; its sealed key is bound to it. */
  id: string;
  name: string;
  kind: UpstreamKind;
  base_url?: string;
  /** Bedrock and Vertex. */
  region?: string;
  /** Vertex's project id. */
  project?: string;
  /** Exact, or a prefix ending in `*`; empty: what it lists. */
  models?: string[];
  /** `openai_compatible`: it speaks Responses too. */
  responses?: boolean;
  /** Sent with every request to it; never a key. */
  headers?: [string, string][];
  /** Its base URL may be a private, loopback or tailnet address. */
  allow_private?: boolean;
  /** The key is read on the gateway's machine from 1Password. */
  key_ref?: string;
  disabled?: boolean;
}

export interface RouteTarget {
  upstream: string;
  /** The model name to ask it for (unset: the client's). */
  model?: string;
}

/** Where requests for a model go, the first healthy target serving. An alias renames the model. */
export interface RouteSpec {
  /** Exact, or a prefix ending in `*`. */
  model: string;
  targets: RouteTarget[];
}

/** A model's price over the built-in list, in micro-dollars per million tokens. */
export interface ModelPrice {
  model: string;
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
}

/** The gateway as the owner sets it on its machine (`gateway.configure` replaces it whole). No secrets. */
export interface GatewaySettings {
  enabled: boolean;
  port: number;
  /** IP literals besides loopback; never an unspecified address. */
  listen?: string[];
  /** In priority order. */
  upstreams?: UpstreamSpec[];
  routes?: RouteSpec[];
  prices?: ModelPrice[];
  /** Keep bodies in the request log (64 KiB each, a day). */
  log_bodies?: boolean;
  /** Unset: on. */
  affinity?: boolean;
  /** Days of usage kept; unset: 400. */
  retention_days?: number;
}

export const GATEWAY_PORT = 8790;
export const GATEWAY_RETENTION = 400;

/** Where an upstream's key comes from (never any part of it). */
export interface UpstreamKey {
  upstream: string;
  key: KeyState;
}

/** A client key (`rgk_…`): one per machine that uses the gateway. The gateway keeps only its hash. */
export interface GatewayClient {
  /** `gwc_…`. */
  id: string;
  name: string;
  machine?: MachineId;
  created_at: Millis;
}

export type GatewayRun =
  | { state: "stopped" }
  | { state: "running"; addresses: string[]; since: Millis }
  /** It couldn't start; the daemon retries. Addresses it did bind are still served. */
  | { state: "failed"; error: string; addresses?: string[] };

/** The gateway as its daemon reports it (`MachineConfig.gateway`). */
export interface GatewayInfo {
  settings: GatewaySettings;
  keys: UpstreamKey[];
  clients: GatewayClient[];
  run: GatewayRun;
}

export type CircuitState = "closed" | "open" | "half_open";

/** An upstream's health (`gateway.status`). */
export interface UpstreamHealth {
  upstream: string;
  circuit: CircuitState;
  failures: number;
  open_until?: Millis;
  /** A 429's `retry-after`: nothing goes to it before this. */
  cooling_until?: Millis;
  last_ok_at?: Millis;
  last_error?: string;
  last_error_at?: Millis;
  requests_hour: number;
  /** Median time to the first byte over the last hour. */
  p50_ms?: number;
}

export interface ClientSeen {
  client: string;
  at: Millis;
}

export interface GatewayStatus {
  run: GatewayRun;
  upstreams: UpstreamHealth[];
  clients_seen: ClientSeen[];
  /** What `GET /v1/models` offers. */
  models: string[];
}

/** What `gateway.test` found for one upstream (it never generates). */
export interface UpstreamCheck {
  upstream: string;
  ok: boolean;
  ms: number;
  models: number;
  error?: string;
}

export type UsageBy = "model" | "machine" | "project" | "thread" | "upstream";
export type UsageBucket = "hour" | "day";

export interface UsageRow {
  /** The bucket's start (UTC). */
  at: Millis;
  /** The group's key; empty when the request didn't say. */
  key: string;
  requests: number;
  errors: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  /** Micro-dollars from list prices: an estimate, not a bill. */
  cost_micros: number;
}

export interface UsageReport {
  by: UsageBy;
  bucket: UsageBucket;
  from: Millis;
  to: Millis;
  rows: UsageRow[];
  /** More rows matched than the cap; the newest were kept. */
  truncated?: boolean;
}

/** One upstream tried for a request. */
export interface Attempt {
  upstream: string;
  model: string;
  /** 0: no answer. */
  status: number;
  ms: number;
  error?: string;
}

/** A request in the log; more than one attempt is a failover. */
export interface LogEntry {
  seq: number;
  at: Millis;
  client?: string;
  machine?: MachineId;
  project?: string;
  thread?: string;
  format: WireFormat;
  model: string;
  stream: boolean;
  status: number;
  ms: number;
  first_byte_ms?: number;
  attempts: Attempt[];
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  cost_micros: number;
  error?: string;
  /** Its bodies were kept (`gateway.log.body`). */
  has_body?: boolean;
}

export interface LogBody {
  seq: number;
  request: string;
  response: string;
  truncated?: boolean;
}

/** `gateway.client.issue`'s answer: the key sealed to the machine that asked (only ciphertext passes). */
export interface IssuedClient {
  client: GatewayClient;
  sealed: SealedSecret;
}

/** A way members reach the gateway, labelled ("Tailscale", "Docker sandboxes"). */
export interface GatewayUrl {
  label: string;
  url: string;
}

export interface GatewayMember {
  machine: MachineId;
  url: string;
  /** Empty: every provider that can. */
  providers?: ProviderId[];
  /** Its client (`gwc_…`) on the host. */
  client?: string;
  joined_at: Millis;
  /** Why joining last failed. */
  error?: string;
}

/** The owner's gateway, kept on the relay (`/v1/code/agents/{owner}/gateway`): no keys. */
export interface GatewayPlan {
  host: MachineId;
  /** The first is the default. */
  urls?: GatewayUrl[];
  members?: GatewayMember[];
  /** New sandboxes join as they first say hello. */
  new_sandboxes?: boolean;
  /** Members' providers start on their own login or keys while the gateway doesn't answer. */
  fallback?: boolean;
  /** When the owner acknowledged the notice. */
  acknowledged_at: Millis;
  updated_at: Millis;
}

/** `POST …/gateway/members`. */
export interface GatewayJoin {
  machine: MachineId;
  /** Unset: the plan's first URL. */
  url?: string;
  providers?: ProviderId[];
}

// ── machine settings (docs/code/agents.md, "Machine settings") ──

/** A machine's settings as its daemon reports them: what the owner set, and what applies otherwise. */
export interface MachineConfig {
  /** What the owner set from roll (`machine.configure` replaces it whole). */
  set: MachineSettings;
  /** What applies where `set` says nothing: the daemon's `config.toml`, else roll's defaults. */
  base: SettingsBase;
  /** Every provider the daemon has, on or off. */
  providers: ProviderBase[];
  /** The daemon's `config.toml` on the machine: its own API keys and base URLs stay there. */
  config_path: string;
  /** The endpoints set from roll, with whether each has a key (never the key). */
  endpoints?: EndpointInfo[];
  /** roll's model gateway on this machine; unset: never set up here. */
  gateway?: GatewayInfo;
  /** The agents the daemon installed from the ACP registry. Unset: a daemon that can't install them. */
  registry?: RegistryInstalled[];
}

// ── the ACP registry (agent-proto `registry.rs`) ──

/** How the registry distributes an agent: an archive for the machine, or an npm / PyPI package. */
export type RegistryKind = "binary" | "npx" | "uvx";
/** What vouches for the bytes an install runs: a listed SHA-256, an exact package version, or nothing. */
export type RegistryIntegrity = "sha256" | "registry" | "none";

/** One agent of the ACP registry as a daemon sees it (T3's `AcpRegistrySearchAgent`). */
export interface RegistryAgent {
  /** The registry's id; it installs as the provider `acp:<id>`. */
  id: string;
  name: string;
  version: string;
  description?: string;
  website?: string;
  repository?: string;
  /** An SVG on the registry's CDN. */
  icon?: string;
  /** The distributions that suit the machine, in install order (a binary only for its OS and arch). */
  kinds?: RegistryKind[];
  /** `registry.install` can install it there (a binary for it, or npm/uv on its PATH). */
  installable: boolean;
  integrity: RegistryIntegrity;
  /** The version the daemon installed. */
  installed?: string;
  /** The provider on the machine that is this agent already. */
  offered?: ProviderId;
}

/** An agent the daemon installed from the registry. */
export interface RegistryInstalled {
  id: string;
  version: string;
  provider: ProviderId;
  kind: RegistryKind;
  installed_at: Millis;
}

/** `registry.list`'s answer. */
export interface RegistryList {
  agents: RegistryAgent[];
  /** When the daemon last fetched the registry (0: never). */
  fetched_at: Millis;
  /** Why the list is old or empty (the registry couldn't be reached). */
  error?: string;
}

export interface SettingsBase {
  worktree_dir: string;
  branch_prefix: string;
  clone_dir: string | null;
  /** Seconds; 0 never. */
  pull_every: number;
  max_depth: number;
  max_children: number;
  git_name: string | null;
  git_email: string | null;
}

/** A provider as the daemon has it, and what its `config.toml` entry says (variable names only). */
export interface ProviderBase {
  id: ProviderId;
  name: string;
  binary: string | null;
  /** How many arguments `config.toml` gives it. */
  args: number;
  env: string[];
}

/**
 * What the owner set for a machine (T3 Code's server settings that belong to a machine). Every
 * field overrides `SettingsBase` or the provider's `config.toml` entry; unset falls back. As the
 * wire has it: read a provider's through `providerSettings`.
 */
export interface MachineSettings {
  providers?: Record<ProviderId, ProviderSettings>;
  /** What new threads on the machine start with, after their project's defaults. */
  defaults?: ThreadDefaults;
  worktree_dir?: string;
  branch_prefix?: string;
  clone_dir?: string;
  pull_every?: number;
  max_depth?: number;
  max_children?: number;
  /** Who publishing commits as (both or neither). */
  git_name?: string;
  git_email?: string;
  /** When to compact idle threads, for providers that don't say (unset: on return). */
  idle_compaction?: IdleCompaction;
}

/** When to compact a big idle conversation (T3's `idleCompaction`). */
export type IdleCompaction = "on_return" | "before_cache_expires" | "off";
export const IDLE_COMPACTION_LABELS: Record<IdleCompaction, string> = {
  on_return: "When you return",
  before_cache_expires: "Before the cache expires (beta)",
  off: "Never",
};
/** Below this a cold read is cheap: no idle compaction (T3's `COMPACTION_SUGGESTION_MIN_TOKENS`). */
export const IDLE_COMPACTION_MIN_TOKENS = 200_000;
/** T3's "Auto-compact after" range. */
export const AUTO_COMPACT_MIN = 100_000;
export const AUTO_COMPACT_MAX = 1_000_000;

/** When a provider's idle threads compact: its own setting, else the machine's, else on return. */
export function idleCompactionOf(set: MachineSettings | undefined, provider: ProviderId): IdleCompaction {
  return set?.providers?.[provider]?.idle_compaction ?? set?.idle_compaction ?? "on_return";
}

/** One provider's settings on a machine (unset `enabled`: on). */
export interface ProviderSettings {
  enabled?: boolean;
  binary?: string;
  /** After `config.toml`'s. */
  args?: string[];
  /** Over `config.toml`'s; never a credential (`isCredentialEnv`). */
  env?: Record<string, string>;
  models?: CustomModel[];
  /** Compact after this many tokens (T3's "Auto-compact after"; Claude). */
  auto_compact_window?: number;
  /** Over the machine's `idle_compaction`. */
  idle_compaction?: IdleCompaction;
}

export interface CustomModel {
  /** The slug the provider takes. */
  id: string;
  name?: string;
  /** What the composer offers for it (T3's custom `optionDescriptors`); unset: the provider's own. */
  options?: ModelOption[];
}

/** A provider's settings with the defaults filled in. */
export function providerSettings(
  set: MachineSettings,
  id: ProviderId,
): Required<Omit<ProviderSettings, "binary" | "auto_compact_window" | "idle_compaction">> & Pick<ProviderSettings, "binary" | "auto_compact_window" | "idle_compaction"> {
  const p = set.providers?.[id] ?? {};
  return { enabled: p.enabled ?? true, binary: p.binary, args: p.args ?? [], env: p.env ?? {}, models: p.models ?? [], auto_compact_window: p.auto_compact_window, idle_compaction: p.idle_compaction };
}

/**
 * A variable that may carry a credential or point a provider somewhere else (agent-proto's
 * `is_credential_env`, kept identical): roll never relays one, so settings refuse it and say to
 * set it in the machine's `config.toml`.
 */
export function isCredentialEnv(name: string): boolean {
  const upper = name.toUpperCase();
  return ["KEY", "TOKEN", "SECRET", "PASS", "CREDENTIAL", "AUTH", "COOKIE", "SESSION", "PRIVATE", "URL", "ENDPOINT", "PROXY", "HOST"].some((w) =>
    upper.includes(w),
  );
}

/** `starting`: on its way up again after a stop (woken by a command, or started by its owner). */
export type SandboxStatus = "creating" | "starting" | "running" | "stopped" | "failed" | "deleting";

/** Why a sandbox is stopped: asleep (`idle`: nothing ran on it for a while) or its owner's stop. A command to either wakes it. */
export type StopReason = "idle" | "owner";

export interface SandboxInfo {
  backend: string;
  size: string;
  status: SandboxStatus;
  hourly_cents: number;
  /** Set while `stopped`. */
  stopped_reason?: StopReason;
}

export interface Machine {
  id: MachineId;
  name: string;
  online: boolean;
  info: MachineInfo | null;
  sandbox: SandboxInfo | null;
  /** Its colour and kind, as its owner set them. */
  appearance: MachineAppearance;
  created_at: Millis;
  last_seen: Millis | null;
}

/**
 * What a machine is doing, as the canvas says it. A sandbox that isn't connected is never just
 * "offline": it's asleep (stopped while idle), stopped by its owner, waking, booting or connecting,
 * and a command to it wakes it (the relay holds the command until the sandbox is up).
 */
export type MachineState = "online" | "asleep" | "stopped" | "waking" | "booting" | "connecting" | "offline";

export function machineState(m: Machine): MachineState {
  if (m.online) return "online";
  switch (m.sandbox?.status) {
    case "creating":
      return "booting";
    case "starting":
      return "waking";
    case "running":
      return "connecting";
    case "stopped":
      return m.sandbox.stopped_reason === "owner" ? "stopped" : "asleep";
    default:
      return "offline";
  }
}

/** A command sent to the machine gets to it: now, or once its sandbox has woken. */
export const reachable = (m: Machine | undefined): boolean => !!m && machineState(m) !== "offline";

/** On its way up: the canvas pulses it. */
export const coming = (s: MachineState): boolean => s === "waking" || s === "booting" || s === "connecting";

// ── commands ──

export type SendMode = "queue" | "steer";

export type Command =
  | { type: "project.add"; path: string; name?: string; clone?: RepoRef }
  | { type: "project.remove"; project: ProjectId }
  | {
      type: "project.update";
      project: ProjectId;
      name?: string;
      settings?: { auto_pull: boolean; defaults?: ThreadDefaults; scripts?: ProjectScript[]; auto_settle?: AutoSettle; cleanup?: Cleanup; color?: IdentityColor; icon?: ProjectIcon };
    }
  | { type: "project.pull"; project: ProjectId }
  | { type: "project.cleanup"; project: ProjectId }
  | {
      type: "thread.create";
      project: ProjectId;
      provider: ProviderId;
      model?: string;
      effort?: string;
      options?: ModelOptions;
      runtime_mode?: RuntimeMode;
      interaction_mode?: InteractionMode;
      worktree?: boolean;
      title?: string;
      prompt?: string;
      attachments?: Attachment[];
      /** Start an orchestrator: `prompt` is a goal it plans and delegates to subagents. */
      orchestrator?: boolean;
    }
  | {
      type: "thread.configure";
      thread: ThreadId;
      title?: string;
      provider?: ProviderId;
      model?: string;
      effort?: string;
      /** Replaces the thread's options; a new model keeps those it takes unless this is set. */
      options?: ModelOptions;
      runtime_mode?: RuntimeMode;
      interaction_mode?: InteractionMode;
    }
  | { type: "thread.fork"; thread: ThreadId; at?: RunId; worktree?: boolean }
  | { type: "thread.archive"; thread: ThreadId }
  | { type: "thread.unarchive"; thread: ThreadId }
  | { type: "thread.delete"; thread: ThreadId }
  | { type: "thread.settle"; thread: ThreadId }
  /** With `restore`, undoes a settle: the inbox as it was before (back under auto-settle), if the user's settle still stands. */
  | { type: "thread.unsettle"; thread: ThreadId; restore?: InboxBefore }
  | { type: "thread.snooze"; thread: ThreadId; until: Millis }
  | { type: "thread.unsnooze"; thread: ThreadId }
  | { type: "thread.visit"; thread: ThreadId }
  | { type: "thread.auto_settle"; thread: ThreadId; enabled: boolean }
  | { type: "thread.change_ended"; thread: ThreadId; landed: boolean }
  | { type: "message.send"; thread: ThreadId; text: string; attachments?: Attachment[]; mode?: SendMode }
  | { type: "message.unqueue"; thread: ThreadId; message: string }
  /** T3's queue edit: `attachments` replaces the images when given. */
  | { type: "message.edit"; thread: ThreadId; message: string; text: string; attachments?: Attachment[] }
  /** Move a queued message in front of `before` (last when unset). */
  | { type: "message.reorder"; thread: ThreadId; message: string; before?: string }
  /** T3's "Send now": steers the running run, or starts it on a held queue. */
  | { type: "message.promote"; thread: ThreadId; message: string }
  | { type: "run.interrupt"; thread: ThreadId }
  | { type: "request.respond"; request: RequestId; response: RequestResponse }
  | { type: "checkpoint.rollback"; thread: ThreadId; checkpoint: CheckpointId }
  | { type: "thread.publish"; thread: ThreadId; message: string; land?: boolean }
  | {
      type: "terminal.open";
      project?: ProjectId;
      thread?: ThreadId;
      command?: string;
      /** A project script's id: its command, told where it runs. */
      script?: string;
      cols: number;
      rows: number;
    }
  | { type: "terminal.input"; terminal: TerminalId; data: string }
  | { type: "terminal.resize"; terminal: TerminalId; cols: number; rows: number }
  | { type: "terminal.close"; terminal: TerminalId }
  | { type: "provider.refresh" }
  /** Replace the machine's settings whole; the reply comes after its `hello` with them. */
  | { type: "machine.configure"; settings: MachineSettings }
  /** Add or replace an endpoint and change its key (sealed to the daemon; `agents-seal.ts`). */
  | { type: "endpoint.set"; endpoint: EndpointSpec; key?: KeyChange }
  | { type: "endpoint.remove"; endpoint: string }
  /** Ask the endpoint for its models now (answers `endpoint_checked`). */
  | { type: "endpoint.check"; endpoint: string }
  /** Set up, change, start or stop the gateway on this machine (replaced whole, applied live). */
  | { type: "gateway.configure"; settings: GatewaySettings }
  /** An upstream's key: sealed to this daemon (`sealUpstreamKey`), or cleared. */
  | { type: "gateway.key"; upstream: string; key?: KeyChange }
  /** Mint a client key sealed to `seal_key` (answers `gateway_client`); the relay sends it on joining. */
  | { type: "gateway.client.issue"; name: string; machine?: MachineId; seal_key: string }
  | { type: "gateway.client.revoke"; client: string }
  /** Ask upstreams for their model lists (answers `gateway_tested`); never generates. */
  | { type: "gateway.test"; upstream?: string }
  | { type: "gateway.remove" }
  | { type: "gateway.status" }
  | { type: "gateway.usage"; from: Millis; to: Millis; by?: UsageBy; bucket?: UsageBucket }
  /** Newest first: after `after` (the live tail) or before `before` (paging back). */
  | { type: "gateway.log"; after?: number; before?: number; limit: number; errors?: boolean }
  | { type: "gateway.log.body"; seq: number }
  /** Install an agent of the ACP registry on the machine; replies once it's offered as `acp:<agent>`. */
  | { type: "registry.install"; agent: string }
  /** Remove an agent the daemon installed from the registry. */
  | { type: "registry.remove"; agent: string }
  | { type: "diff.get"; thread: ThreadId; run?: RunId }
  | { type: "fs.list"; project?: ProjectId; path: string }
  | { type: "fs.read"; project: ProjectId; path: string }
  /** Write a text file in the thread's workspace (T3's `projects.writeFile`, "Save to workspace"); answers `written`. */
  | { type: "fs.write"; thread: ThreadId; path: string; text: string }
  | { type: "git.status"; thread: ThreadId }
  | { type: "terminal.scrollback"; terminal: TerminalId }
  /** The ACP registry's agents as the machine sees them (answers `registry`). */
  | { type: "registry.list" };

export type CommandType = Command["type"];

/** Queries only answer; they never change state. */
export const QUERIES: readonly CommandType[] = ["diff.get", "fs.list", "fs.read", "git.status", "terminal.scrollback", "registry.list", "gateway.status", "gateway.usage", "gateway.log", "gateway.log.body"];

export interface CommandRequest {
  /** The idempotency key: retrying with the same id never runs the command twice. */
  id: string;
  command: Command;
}

// ── replies ──

export type ErrorCode = "not_found" | "invalid" | "conflict" | "provider" | "machine_offline" | "timeout" | "internal";

export interface Diff {
  stat: DiffStat;
  files: FileChange[];
  truncated: boolean;
}

export interface FsEntry {
  name: string;
  dir: boolean;
  size: number;
}

export interface FileContent {
  path: string;
  text: string | null;
  size: number;
}

export interface GitStatus {
  branch: string | null;
  ahead: number;
  behind: number;
  files: FileChange[];
}

export interface Published {
  branch: string;
  commit: string;
  change: number | null;
  url: string | null;
  /** What sending it to landing did (`land: true`). */
  landing: Landing | null;
}

export interface Landing {
  status: "landed" | "queued" | "blocked" | "conflict" | "failed";
  message: string;
  blockers: string[];
}

export type Reply =
  | { type: "ok" }
  | { type: "created"; id: string }
  | ({ type: "diff" } & Diff)
  | { type: "entries"; entries: FsEntry[] }
  | ({ type: "file" } & FileContent)
  | ({ type: "git_status" } & GitStatus)
  | ({ type: "published" } & Published)
  | ({ type: "pulled" } & PullResult)
  | { type: "written"; path: string }
  | ({ type: "cleaned" } & CleanupReport)
  | { type: "scrollback"; data: string }
  | ({ type: "endpoint_checked" } & EndpointCheck)
  | ({ type: "gateway_client" } & IssuedClient)
  | { type: "gateway_tested"; checks: UpstreamCheck[] }
  | ({ type: "gateway_status" } & GatewayStatus)
  | ({ type: "gateway_usage" } & UsageReport)
  | { type: "gateway_log"; entries: LogEntry[] }
  | ({ type: "gateway_log_body" } & LogBody)
  | ({ type: "registry" } & RegistryList)
  | { type: "error"; code: ErrorCode; message: string };

// ── deltas and the stream ──

export type Delta =
  | { type: "text"; thread: ThreadId; node: NodeId; text: string }
  | { type: "output"; thread: ThreadId; node: NodeId; text: string }
  | { type: "terminal"; terminal: TerminalId; data: string };

export interface Mirrored {
  cursor: number;
  machine: MachineId;
  entity: Entity;
}

export interface State {
  cursor: number;
  machines: Machine[];
  entities: Mirrored[];
}

export type StreamEvent =
  | ({ type: "entity" } & Mirrored)
  | { type: "machine"; cursor: number; machine: Machine }
  | { type: "machine_removed"; cursor: number; machine: MachineId }
  | { type: "delta"; machine: MachineId; delta: Delta }
  /** The owner's canvas was saved: a device showing an older `revision` loads it again. */
  | { type: "canvas"; cursor: number; revision: number; updated_at: Millis }
  /** The owner's agents settings were saved: a device showing an older `revision` loads them again. */
  | { type: "settings"; cursor: number; revision: number; updated_at: Millis }
  /** An automation was made or changed (`automations-api.ts`). */
  | { type: "automation"; cursor: number; automation: Automation }
  | { type: "automation_removed"; cursor: number; automation: AutomationId }
  /** An automation run was made or moved on. */
  | { type: "automation_run"; cursor: number; run: AutomationRun }
  /** A `notify` step's notice: ephemeral (no cursor), for a toast. */
  | { type: "notice"; notice: Notice };

/** `GET`/`PUT /v1/code/agents/{owner}/canvas`: the canvas as the owner arranged it (opaque here). */
export interface SavedCanvas {
  /** `null` until the first save. */
  canvas: Record<string, unknown> | null;
  /** Saves so far (0: none). */
  revision: number;
  updated_at: Millis | null;
}

/** A save over a revision that isn't the current one (409): `current` is what is there now. */
export class CanvasConflict extends Error {
  constructor(readonly current: SavedCanvas) {
    super(`the canvas changed (it is at revision ${current.revision})`);
  }
}

/** The largest canvas the relay keeps, as compact JSON (bytes). */
export const MAX_CANVAS = 64 * 1024;

/** `GET`/`PUT /v1/code/agents/{owner}/settings`: the owner's agents settings (opaque here; `owner-settings.ts` parses them). */
export interface SavedSettings {
  /** `null` until the first save. */
  settings: Record<string, unknown> | null;
  revision: number;
  updated_at: Millis | null;
}

/** A settings save over a revision that isn't the current one (409): `current` is what is there now. */
export class SettingsConflict extends Error {
  constructor(readonly current: SavedSettings) {
    super(`the settings changed (they are at revision ${current.revision})`);
  }
}

/** The largest settings document the relay keeps, as compact JSON (bytes). */
export const MAX_SETTINGS = 32 * 1024;

// ── the relay protocol (daemon ↔ relay; mirrored for the fixture tests) ──

export interface Stamped {
  seq: number;
  entity: Entity;
}

export type DaemonFrame =
  | { type: "hello"; protocol: number; info: MachineInfo }
  | { type: "entities"; items: Stamped[] }
  | { type: "delta"; delta: Delta }
  | { type: "reply"; id: string; result: Reply }
  | { type: "pong" };

export type RelayFrame =
  | { type: "welcome"; machine: MachineId; owner: string; resume_from: number }
  | { type: "ack"; seq: number }
  | { type: "notice"; message: string }
  | { type: "command"; id: string; by: string; command: Command; grant: CodeGrant | null }
  | { type: "ping" };

/** Git access to one roll code repo, for one command (daemons only). */
export interface CodeGrant {
  repo: RepoRef;
  url: string;
  token: string;
  write: boolean;
  /** The one branch a write grant pushes (a thread's branch, or an empty repo's default branch). */
  branch?: string;
  expires_at: Millis;
  /** A connected repo's GitHub side: where a publish goes as a pull request while roll is down. */
  github?: GrantGithub;
}

export interface GrantGithub {
  owner: string;
  repo: string;
  url: string;
  default_branch: string;
}

// ── device login ──

export interface DeviceLogin {
  device_code: string;
  user_code: string;
  verification_uri: string;
  interval: number;
  expires_in: number;
}

export type LoginPoll =
  | { status: "pending" }
  | { status: "approved"; token: string; machine: MachineId; owner: string }
  | { status: "expired" }
  | { status: "denied" };

/** A device login waiting for approval: which machine asks. */
export interface PendingLogin {
  user_code: string;
  name: string;
  hostname: string | null;
  /** `macos`, `linux`, … (Rust's `std::env::consts::OS`). */
  os: string | null;
  created_at: Millis;
  expires_at: Millis;
}

/** The machine a machine token opens (`GET /v1/code/agents/connect` without an upgrade). */
export interface MachineLogin {
  machine: Machine;
  owner: string;
}

// ── shares ──

export interface SharedMachine {
  name: string;
  os: string | null;
}

/** A frozen, read-only copy of a thread (and its subagent threads) behind a public link. */
export interface Share {
  id: string;
  owner: string;
  title: string;
  thread: ThreadId;
  subtree: boolean;
  machine: SharedMachine;
  created_at: Millis;
  entities: Entity[];
  trimmed: boolean;
}

export interface ShareSummary {
  id: string;
  title: string;
  thread: ThreadId;
  subtree: boolean;
  url: string;
  threads: number;
  runs: number;
  bytes: number;
  created_at: Millis;
  created_by: string;
}

// ── decoders ──

const vec = <T>(d: Decoder<T>) => withDefault(array(d), () => []);
const opt = nullable;

const repoRefD: Decoder<RepoRef> = obj<RepoRef>({ owner: str, repo: str });
const gitInfoD = obj<GitInfo>({ branch: opt(str), head: opt(str), remote: opt(str), dirty: bool });
const runtimeModeD = literal("full_access", "auto_edit", "ask");
const interactionModeD = literal("default", "plan");
const optionValueD: Decoder<OptionValue> = strOrBool;
const modelOptionsD = withDefault(map(optionValueD), () => ({}));
const optionRoleD = optional(literal("reasoning", "speed", "context_window"));
const optionChoiceD = obj<OptionChoice>({ id: str, label: str, description: optional(str) });
const modelOptionD: Decoder<ModelOption> = tagged("type", {
  select: obj<Omit<Extract<ModelOption, { type: "select" }>, "type">>({
    id: str,
    label: str,
    description: optional(str),
    choices: vec(optionChoiceD),
    default: optional(str),
    role: optionRoleD,
  }),
  boolean: obj<Omit<Extract<ModelOption, { type: "boolean" }>, "type">>({
    id: str,
    label: str,
    description: optional(str),
    default: withDefault(bool, () => false),
    role: optionRoleD,
  }),
});
const modelInfoWireD = obj<ModelInfo & { efforts: string[] }>({
  id: str,
  name: str,
  short_name: optional(str),
  default: withDefault(bool, () => false),
  options: vec(modelOptionD),
  efforts: vec(str),
  legacy: optional(bool),
  images: optional(bool),
  custom: optional(bool),
  via: optional(str),
  badge: optional(literal("new")),
});
/** Daemons from before option descriptors send a model's reasoning levels alone, as `efforts` (agent-proto reads them the same way). */
const modelInfoD: Decoder<ModelInfo> = (v, p, ctx) => {
  const { efforts, ...m } = modelInfoWireD(v, p, ctx);
  if (efforts.length && !m.options.some((o) => o.role === "reasoning")) {
    m.options = [{ type: "select", id: EFFORT_OPTION, label: "Reasoning", choices: efforts.map((e) => ({ id: e, label: effortLabel(e) })), role: "reasoning" }, ...m.options];
  }
  return m;
};

/** T3's names for reasoning levels (agent-proto `effort_label`). */
export function effortLabel(effort: string): string {
  if (effort === "xhigh") return "Extra High";
  if (effort === "xlow") return "Extra Low";
  return effort ? effort[0].toUpperCase() + effort.slice(1) : effort;
}

const threadDefaultsD = obj<ThreadDefaults>({
  provider: optional(str),
  model: optional(str),
  effort: optional(str),
  options: optional(map(optionValueD)),
  runtime_mode: optional(runtimeModeD),
  interaction_mode: optional(interactionModeD),
  worktree: optional(bool),
});
const projectScriptD = obj<ProjectScript>({
  id: str,
  name: str,
  command: str,
  icon: withDefault(literal("play", "test", "lint", "configure", "build", "debug"), () => "play" as const),
  on_worktree: withDefault(bool, () => false),
});
const autoSettleD = obj<AutoSettle>({
  after_days: withDefault(nullable(int), () => DEFAULT_AUTO_SETTLE.after_days),
  on_land: withDefault(bool, () => true),
  keep_changes: optional(bool),
});
const keptReasonD = literal("local_changes", "busy", "terminal", "other_branch", "shared", "not_managed", "failed");
const cleanupD = obj<Cleanup>({
  worktree_after_days: optional(nullable(int)),
  keep: optional(literal("uncommitted_changes", "any_local_files")),
});
// Identity is decoded leniently: a colour or icon kind from a newer daemon shows the default
// rather than dropping the record. A logo is only ever a `data:image/…` URL.
const identityColorD: Decoder<IdentityColor | undefined> = Object.assign(
  (v: unknown) => (typeof v === "string" && (IDENTITY_COLORS as readonly string[]).includes(v) ? (v as IdentityColor) : undefined),
  { missing: () => undefined },
);
const projectIconD: Decoder<ProjectIcon | undefined> = Object.assign(
  (v: unknown, p: string, ctx: DecodeCtx) => {
    const kind = typeof v === "object" && v !== null ? (v as { kind?: unknown }).kind : undefined;
    if (kind !== "auto" && kind !== "monogram" && kind !== "emoji" && kind !== "file") return undefined;
    return tagged("kind", {
      auto: obj<object>({}),
      monogram: obj<{ text?: string }>({ text: optional(str) }),
      emoji: obj<{ emoji: string }>({ emoji: str }),
      file: obj<{ path: string }>({ path: str }),
    })(v, p, ctx) as ProjectIcon;
  },
  { missing: () => undefined },
);
const projectLogoD: Decoder<ProjectLogo | null> = Object.assign(
  (v: unknown, p: string, ctx: DecodeCtx) => {
    if (v === null || v === undefined) return null;
    const logo = obj<ProjectLogo>({ path: str, url: str })(v, p, ctx);
    return /^data:image\/(png|svg\+xml);base64,[A-Za-z0-9+/=]+$/.test(logo.url) ? logo : null;
  },
  { missing: () => null },
);
const settingsFields = {
  auto_pull: withDefault(bool, () => false),
  defaults: optional(threadDefaultsD),
  scripts: optional(array(projectScriptD)),
  auto_settle: optional(autoSettleD),
  cleanup: optional(cleanupD),
  color: identityColorD,
  icon: projectIconD,
};
const projectSettingsD = obj<ProjectSettings>({
  auto_pull: settingsFields.auto_pull,
  defaults: withDefault(threadDefaultsD, () => ({})),
  scripts: vec(projectScriptD),
  auto_settle: withDefault(autoSettleD, () => ({ ...DEFAULT_AUTO_SETTLE })),
  cleanup: optional(cleanupD),
  color: identityColorD,
  icon: projectIconD,
});
const pullOutcomeD: Decoder<PullOutcome> = tagged("status", {
  pulled: obj<{ branch: string; from: string; to: string; commits: number }>({ branch: str, from: str, to: str, commits: int }),
  up_to_date: obj<{ branch: string }>({ branch: str }),
  skipped: obj<{ reason: PullSkip; message: string }>({
    reason: literal("not_git", "detached", "other_branch", "no_upstream", "local_changes", "diverged", "busy", "no_access"),
    message: str,
  }),
  failed: obj<{ message: string }>({ message: str }),
} satisfies Record<PullOutcome["status"], Decoder<object>>);
const pullResultD = obj<PullResult>({ at: int, outcome: pullOutcomeD });
const projectD = obj<Project>({
  id: str,
  name: str,
  path: str,
  repo: opt(repoRefD),
  git: opt(gitInfoD),
  settings: withDefault(projectSettingsD, () => ({ ...NO_SETTINGS })),
  pull: opt(pullResultD),
  logo: projectLogoD,
  created_at: int,
  updated_at: int,
});
const attachmentD = obj<Attachment>({ name: str, mime: str, data: str });
const usageFields = {
  input_tokens: withDefault(int, () => 0),
  output_tokens: withDefault(int, () => 0),
  cached_tokens: withDefault(int, () => 0),
  cost_usd: opt(num),
};
const usageD = withDefault(obj<Usage>(usageFields), () => ({ ...ZERO_USAGE }));

const contextD = obj<ContextUsage>({ used_tokens: int, max_tokens: opt(int), total_processed_tokens: opt(int), cache_ttl_ms: opt(int), auto_compact_at: opt(int) });

export const ZERO_USAGE: Usage = { input_tokens: 0, output_tokens: 0, cached_tokens: 0, cost_usd: null };

const settleD = tagged("state", {
  settled: obj<{ at: Millis; by: SettledBy }>({ at: int, by: literal("user", "inactivity", "landed", "closed") }),
  active: obj<{ at: Millis }>({ at: int }),
} satisfies Record<Settle["state"], Decoder<object>>);
const snoozeD = obj<Snooze>({ until: int, at: int });

const diffStatD = obj<DiffStat>({ files: int, additions: int, deletions: int });

const threadD = obj<Thread>({
  id: str,
  project: str,
  title: str,
  provider: str,
  model: opt(str),
  effort: opt(str),
  options: modelOptionsD,
  runtime_mode: runtimeModeD,
  interaction_mode: interactionModeD,
  cwd: str,
  worktree: opt(obj<Worktree>({ path: str, branch: str, base: str, removed_at: opt(int) })),
  status: literal("idle", "starting", "running", "waiting", "error"),
  lineage: opt(obj<Lineage>({ parent: str, relation: literal("fork", "subagent"), root: str, from_run: opt(str) })),
  orchestrator: withDefault(bool, () => false),
  active_run: opt(str),
  queued: vec(obj<QueuedMessage>({ id: str, text: str, attachments: vec(attachmentD), at: int, delivers: withDefault(vec(str), () => []) })),
  run_count: int,
  pending_requests: int,
  diff: opt(diffStatD),
  plan_ready: withDefault(bool, () => false),
  activity: opt(str),
  usage: usageD,
  context: opt(contextD),
  error: opt(str),
  change: opt(
    obj<ThreadChange>({
      repo: str,
      number: int,
      status: literal("checking", "failing", "blocked", "ready", "queued", "landed", "conflict", "closed"),
      url: str,
      detail: opt(str),
    }),
  ),
  work: opt(obj<WorkRef>({ id: str, repo: str, number: int, title: str })),
  created_at: int,
  updated_at: int,
  archived_at: opt(int),
  inbox: withDefault(
    obj<Inbox>({
      settle: opt(settleD),
      snooze: opt(snoozeD),
      woke_at: opt(int),
      visited_at: opt(int),
      manual: withDefault(bool, () => false),
    }),
    () => ({ ...EMPTY_INBOX }),
  ),
});
const changeKindD = literal("add", "modify", "delete", "rename");
const fileStatD = obj<FileStat>({ path: str, change: changeKindD, additions: int, deletions: int });
const runD = obj<Run>({
  id: str,
  thread: str,
  ordinal: int,
  provider: str,
  model: opt(str),
  status: literal("queued", "starting", "running", "waiting", "completed", "interrupted", "failed", "cancelled", "rolled_back"),
  prompt: str,
  attachments: vec(attachmentD),
  steers: vec(str),
  steered_at: withDefault(vec(int), () => []),
  idle_compaction: withDefault(bool, () => false),
  delivers: withDefault(vec(str), () => []),
  requested_at: int,
  started_at: opt(int),
  completed_at: opt(int),
  checkpoint: opt(str),
  stat: opt(diffStatD),
  files: withDefault(vec(fileStatD), () => []),
  usage: usageD,
  context: opt(contextD),
  error: opt(str),
});
const fileChangeD = obj<FileChange>({ path: str, change: changeKindD, diff: opt(str) });
const toolFields = {
  name: str,
  kind: literal("command", "edit", "read", "search", "web", "mcp", "other"),
  title: str,
  input: json,
  output: opt(str),
  exit_code: opt(int),
  files: vec(fileChangeD),
};
const text = obj<{ text: string }>({ text: str });
const nodeBodyD: Decoder<NodeBody> = tagged("type", {
  message: text,
  reasoning: text,
  plan: text,
  todos: obj<{ items: Todo[] }>({
    items: array(obj<Todo>({ text: str, status: literal("pending", "in_progress", "completed") })),
  }),
  tool: obj<ToolCall>(toolFields),
  subagent: obj<Omit<SubagentBody, "type">>({
    agent: str,
    prompt: str,
    child: opt(str),
    result: opt(str),
    progress: opt(str),
    model: opt(str),
  }),
  compaction: obj<{ before_tokens: number | null; after_tokens: number | null; error: string | null }>({
    before_tokens: opt(int),
    after_tokens: opt(int),
    error: opt(str),
  }),
  request: obj<{ request: RequestId }>({ request: str }),
  system: text,
} satisfies Record<NodeBody["type"], Decoder<object>>);
const nodeD = obj<Node>({
  id: str,
  thread: str,
  run: str,
  parent: opt(str),
  ordinal: int,
  status: literal("running", "waiting", "completed", "failed", "interrupted"),
  started_at: int,
  completed_at: opt(int),
  body: nodeBodyD,
});
const decisionD = literal("accept", "accept_for_session", "decline", "cancel");
const approvalD = obj<Approval>({
  action: literal("command", "file_change", "permission", "tool", "plan"),
  title: str,
  command: opt(str),
  cwd: opt(str),
  files: vec(fileChangeD),
  reason: opt(str),
  tool: opt(str),
  input: json,
  decisions: array(decisionD),
});
const questionD = obj<Question>({
  id: str,
  header: str,
  question: str,
  options: vec(obj<QuestionOption>({ label: str, description: opt(str) })),
  multi: withDefault(bool, () => false),
});
const requestBodyD: Decoder<RequestBody> = tagged("type", {
  approval: approvalD,
  questions: obj<{ questions: Question[] }>({ questions: array(questionD) }),
} satisfies Record<RequestBody["type"], Decoder<object>>);
const responseD: Decoder<RequestResponse> = tagged("type", {
  decision: obj<{ decision: Decision; message?: string }>({ decision: decisionD, message: optional(str) }),
  answers: obj<{ answers: Record<string, string> }>({ answers: map(str) }),
} satisfies Record<RequestResponse["type"], Decoder<object>>);
const requestD = obj<Request>({
  id: str,
  thread: str,
  run: str,
  node: str,
  status: literal("pending", "resolved", "cancelled"),
  body: requestBodyD,
  created_at: int,
  resolved_at: opt(int),
  resolved_by: opt(str),
  response: opt(responseD),
});
const checkpointD = obj<Checkpoint>({
  id: str,
  thread: str,
  run: opt(str),
  ordinal: int,
  commit: str,
  stat: diffStatD,
  created_at: int,
});
const terminalD = obj<Terminal>({
  id: str,
  project: opt(str),
  thread: opt(str),
  cwd: str,
  title: str,
  status: literal("running", "exited"),
  exit_code: opt(int),
  cols: int,
  rows: int,
  created_at: int,
});
const entityKindD = literal("project", "thread", "run", "node", "request", "checkpoint", "terminal");
export const entityD: Decoder<Entity> = tagged("kind", {
  project: projectD,
  thread: threadD,
  run: runD,
  node: nodeD,
  request: requestD,
  checkpoint: checkpointD,
  terminal: terminalD,
  removed: obj<Removed>({ entity: entityKindD, id: str, thread: opt(str) }),
} satisfies Record<Entity["kind"], Decoder<object>>);

const capabilitiesD = obj<Capabilities>({
  steer: bool,
  fork: bool,
  rollback: bool,
  plan_mode: bool,
  images: bool,
  subagents: bool,
  mode_switch: bool,
  compact: withDefault(bool, () => false),
});
export const providerInfoD = obj<ProviderInfo>({
  id: str,
  name: str,
  installed: bool,
  version: opt(str),
  auth: literal("ready", "missing", "unknown"),
  detail: opt(str),
  models: vec(modelInfoD),
  update_required: vec(obj<UpdateRequired>({ id: str, name: str, min_version: str })),
  capabilities: capabilitiesD,
  checking: optional(bool),
  usage_limits: optional(
    obj<UsageLimits>({
      checked_at: int,
      windows: vec(
        obj<UsageWindow>({
          id: str,
          kind: literal("session", "weekly", "monthly", "other"),
          label: str,
          used_percent: num,
          resets_at: optional(int),
          window_minutes: optional(int),
        }),
      ),
      plan: optional(str),
    }),
  ),
});
const idleCompactionD = literal("on_return", "before_cache_expires", "off");
const providerSettingsD = obj<ProviderSettings>({
  enabled: optional(bool),
  binary: optional(str),
  args: optional(array(str)),
  env: optional(map(str)),
  models: optional(array(obj<CustomModel>({ id: str, name: optional(str), options: optional(array(modelOptionD)) }))),
  auto_compact_window: optional(int),
  idle_compaction: optional(idleCompactionD),
});
const machineSettingsD = obj<MachineSettings>({
  providers: optional(map(providerSettingsD)),
  defaults: optional(threadDefaultsD),
  worktree_dir: optional(str),
  branch_prefix: optional(str),
  clone_dir: optional(str),
  pull_every: optional(int),
  max_depth: optional(int),
  max_children: optional(int),
  git_name: optional(str),
  git_email: optional(str),
  idle_compaction: optional(idleCompactionD),
});
const endpointKindD = literal("anthropic", "openai", "cliproxy", "custom", "gateway");
const endpointSpecFields = {
  id: str,
  name: str,
  kind: endpointKindD,
  base_url: str,
  providers: optional(array(str)),
  url_env: optional(str),
  key_env: optional(str),
  key_ref: optional(str),
  fallback: optional(bool),
};
const endpointSpecD = obj<EndpointSpec>(endpointSpecFields);
const sealedSecretD = obj<SealedSecret>({ v: int, epk: str, nonce: str, ct: str });
const keyChangeD: Decoder<KeyChange> = tagged("type", {
  keep: obj<object>({}),
  clear: obj<object>({}),
  set: obj<{ sealed: SealedSecret }>({ sealed: sealedSecretD }),
} satisfies Record<KeyChange["type"], Decoder<object>>);
const endpointInfoD = obj<EndpointInfo>({
  ...endpointSpecFields,
  key: withDefault(literal("none", "sealed", "reference"), () => "none" as const),
  models: optional(array(obj<EndpointModel>({ id: str, name: optional(str) }))),
  checked_at: optional(int),
  error: optional(str),
});
const registryKindD = literal("binary", "npx", "uvx");
const registryListD = obj<RegistryList>({
  agents: array(
    obj<RegistryAgent>({
      id: str,
      name: str,
      version: str,
      description: optional(str),
      website: optional(str),
      repository: optional(str),
      icon: optional(str),
      kinds: optional(array(registryKindD)),
      installable: bool,
      integrity: literal("sha256", "registry", "none"),
      installed: optional(str),
      offered: optional(str),
    }),
  ),
  fetched_at: withDefault(int, () => 0),
  error: optional(str),
});
const endpointCheckD = obj<EndpointCheck>({ ok: bool, models: withDefault(int, () => 0), ms: withDefault(int, () => 0), error: optional(str) });
export const endpointProfileD = obj<EndpointProfile>({ ...endpointSpecFields, sandbox_default: optional(bool), updated_at: withDefault(int, () => 0) });

// The gateway's (agent-proto `gateway.rs`). What Rust skips when empty stays `undefined`, so
// `gateway.configure` serializes back exactly.
const pairD: Decoder<[string, string]> = (v, p, ctx) => {
  if (!Array.isArray(v) || v.length !== 2) throw new DecodeError(p, "expected a [name, value] pair");
  return [str(v[0], `${p}[0]`, ctx), str(v[1], `${p}[1]`, ctx)];
};
const upstreamKindD = literal(...UPSTREAM_KINDS);
const upstreamSpecD = obj<UpstreamSpec>({
  id: str,
  name: str,
  kind: upstreamKindD,
  base_url: optional(str),
  region: optional(str),
  project: optional(str),
  models: optional(array(str)),
  responses: optional(bool),
  headers: optional(array(pairD)),
  allow_private: optional(bool),
  key_ref: optional(str),
  disabled: optional(bool),
});
const routeSpecD = obj<RouteSpec>({ model: str, targets: array(obj<RouteTarget>({ upstream: str, model: optional(str) })) });
const modelPriceD = obj<ModelPrice>({ model: str, input: int, output: int, cache_read: withDefault(int, () => 0), cache_write: withDefault(int, () => 0) });
export const gatewaySettingsD = obj<GatewaySettings>({
  enabled: withDefault(bool, () => false),
  port: withDefault(int, () => GATEWAY_PORT),
  listen: optional(array(str)),
  upstreams: optional(array(upstreamSpecD)),
  routes: optional(array(routeSpecD)),
  prices: optional(array(modelPriceD)),
  log_bodies: optional(bool),
  affinity: optional(bool),
  retention_days: optional(int),
});
const gatewayRunD: Decoder<GatewayRun> = withDefault(
  tagged("state", {
    stopped: obj<object>({}),
    running: obj<{ addresses: string[]; since: Millis }>({ addresses: vec(str), since: int }),
    failed: obj<{ error: string; addresses?: string[] }>({ error: str, addresses: optional(array(str)) }),
  } satisfies Record<GatewayRun["state"], Decoder<object>>),
  () => ({ state: "stopped" as const }),
);
const keyStateD = literal("none", "sealed", "reference");
const gatewayClientD = obj<GatewayClient>({ id: str, name: str, machine: optional(str), created_at: int });
const gatewayInfoD = obj<GatewayInfo>({
  settings: gatewaySettingsD,
  keys: vec(obj<UpstreamKey>({ upstream: str, key: keyStateD })),
  clients: vec(gatewayClientD),
  run: gatewayRunD,
});
const upstreamHealthD = obj<UpstreamHealth>({
  upstream: str,
  circuit: literal("closed", "open", "half_open"),
  failures: withDefault(int, () => 0),
  open_until: optional(int),
  cooling_until: optional(int),
  last_ok_at: optional(int),
  last_error: optional(str),
  last_error_at: optional(int),
  requests_hour: withDefault(int, () => 0),
  p50_ms: optional(int),
});
const gatewayStatusD = obj<GatewayStatus>({
  run: gatewayRunD,
  upstreams: array(upstreamHealthD),
  clients_seen: vec(obj<ClientSeen>({ client: str, at: int })),
  models: vec(str),
});
const upstreamCheckD = obj<UpstreamCheck>({ upstream: str, ok: bool, ms: withDefault(int, () => 0), models: withDefault(int, () => 0), error: optional(str) });
const usageByD = literal("model", "machine", "project", "thread", "upstream");
const usageBucketD = literal("hour", "day");
const zero = withDefault(int, () => 0);
const usageRowD = obj<UsageRow>({
  at: int,
  key: str,
  requests: int,
  errors: zero,
  input_tokens: int,
  output_tokens: int,
  cache_read_tokens: zero,
  cache_write_tokens: zero,
  cost_micros: int,
});
const usageReportD = obj<UsageReport>({ by: usageByD, bucket: usageBucketD, from: int, to: int, rows: array(usageRowD), truncated: optional(bool) });
const attemptD = obj<Attempt>({ upstream: str, model: str, status: int, ms: int, error: optional(str) });
export const logEntryD = obj<LogEntry>({
  seq: int,
  at: int,
  client: optional(str),
  machine: optional(str),
  project: optional(str),
  thread: optional(str),
  format: literal("messages", "chat", "responses"),
  model: str,
  stream: bool,
  status: int,
  ms: int,
  first_byte_ms: optional(int),
  attempts: vec(attemptD),
  input_tokens: zero,
  output_tokens: zero,
  cache_read_tokens: zero,
  cache_write_tokens: zero,
  cost_micros: zero,
  error: optional(str),
  has_body: optional(bool),
});
const logBodyD = obj<LogBody>({ seq: int, request: str, response: str, truncated: optional(bool) });
const gatewayMemberD = obj<GatewayMember>({ machine: str, url: str, providers: optional(array(str)), client: optional(str), joined_at: int, error: optional(str) });
export const gatewayPlanD = obj<GatewayPlan>({
  host: str,
  urls: optional(array(obj<GatewayUrl>({ label: str, url: str }))),
  members: optional(array(gatewayMemberD)),
  new_sandboxes: optional(bool),
  fallback: optional(bool),
  acknowledged_at: withDefault(int, () => 0),
  updated_at: withDefault(int, () => 0),
});
export const gatewayJoinD = obj<GatewayJoin>({ machine: str, url: optional(str), providers: optional(array(str)) });
const machineConfigD = obj<MachineConfig>({
  set: machineSettingsD,
  base: obj<SettingsBase>({
    worktree_dir: str,
    branch_prefix: str,
    clone_dir: opt(str),
    pull_every: int,
    max_depth: int,
    max_children: int,
    git_name: opt(str),
    git_email: opt(str),
  }),
  providers: array(obj<ProviderBase>({ id: str, name: str, binary: opt(str), args: withDefault(int, () => 0), env: vec(str) })),
  config_path: str,
  endpoints: optional(array(endpointInfoD)),
  gateway: optional(gatewayInfoD),
  registry: optional(array(obj<RegistryInstalled>({ id: str, version: str, provider: str, kind: registryKindD, installed_at: int }))),
});
const machineInfoD = obj<MachineInfo>({
  hostname: str,
  os: str,
  arch: str,
  version: str,
  home: str,
  providers: array(providerInfoD),
  settings: opt(machineConfigD),
  seal_key: optional(str),
});
export const machineD = obj<Machine>({
  id: str,
  name: str,
  online: bool,
  info: opt(machineInfoD),
  sandbox: opt(
    obj<SandboxInfo>({
      backend: str,
      size: str,
      status: literal("creating", "starting", "running", "stopped", "failed", "deleting"),
      hourly_cents: int,
      stopped_reason: optional(literal("idle", "owner")),
    }),
  ),
  appearance: withDefault(
    obj<MachineAppearance>({
      color: identityColorD,
      kind: Object.assign(
        (v: unknown) => (typeof v === "string" && ["laptop", "desktop", "server", "cloud", "container"].includes(v) ? (v as MachineKind) : undefined),
        { missing: () => undefined },
      ),
    }),
    () => ({}),
  ),
  created_at: int,
  last_seen: opt(int),
});

type Body<C, K> = Omit<Extract<C, { type: K }>, "type">;
const cmd = <K extends CommandType>(shape: Decoder<Body<Command, K>>) => shape;
export const commandD: Decoder<Command> = tagged("type", {
  "project.add": cmd<"project.add">(obj({ path: str, name: optional(str), clone: optional(repoRefD) })),
  "project.remove": cmd<"project.remove">(obj({ project: str })),
  "project.update": cmd<"project.update">(obj({ project: str, name: optional(str), settings: optional(obj(settingsFields)) })),
  "project.pull": cmd<"project.pull">(obj({ project: str })),
  "project.cleanup": cmd<"project.cleanup">(obj({ project: str })),
  "thread.create": cmd<"thread.create">(
    obj({
      project: str,
      provider: str,
      model: optional(str),
      effort: optional(str),
      options: optional(map(optionValueD)),
      runtime_mode: optional(runtimeModeD),
      interaction_mode: optional(interactionModeD),
      worktree: optional(bool),
      title: optional(str),
      prompt: optional(str),
      attachments: optional(array(attachmentD)),
      orchestrator: optional(bool),
    }),
  ),
  "thread.configure": cmd<"thread.configure">(
    obj({
      thread: str,
      title: optional(str),
      provider: optional(str),
      model: optional(str),
      effort: optional(str),
      options: optional(map(optionValueD)),
      runtime_mode: optional(runtimeModeD),
      interaction_mode: optional(interactionModeD),
    }),
  ),
  "thread.fork": cmd<"thread.fork">(obj({ thread: str, at: optional(str), worktree: optional(bool) })),
  "thread.archive": cmd<"thread.archive">(obj({ thread: str })),
  "thread.unarchive": cmd<"thread.unarchive">(obj({ thread: str })),
  "thread.delete": cmd<"thread.delete">(obj({ thread: str })),
  "thread.settle": cmd<"thread.settle">(obj({ thread: str })),
  "thread.unsettle": cmd<"thread.unsettle">(obj({ thread: str, restore: optional(obj<InboxBefore>({ settle: opt(settleD), snooze: opt(snoozeD) })) })),
  "thread.snooze": cmd<"thread.snooze">(obj({ thread: str, until: int })),
  "thread.unsnooze": cmd<"thread.unsnooze">(obj({ thread: str })),
  "thread.visit": cmd<"thread.visit">(obj({ thread: str })),
  "thread.auto_settle": cmd<"thread.auto_settle">(obj({ thread: str, enabled: bool })),
  "thread.change_ended": cmd<"thread.change_ended">(obj({ thread: str, landed: bool })),
  "message.send": cmd<"message.send">(
    obj({ thread: str, text: str, attachments: optional(array(attachmentD)), mode: optional(literal("queue", "steer")) }),
  ),
  "message.unqueue": cmd<"message.unqueue">(obj({ thread: str, message: str })),
  "message.edit": cmd<"message.edit">(obj({ thread: str, message: str, text: str, attachments: optional(array(attachmentD)) })),
  "message.reorder": cmd<"message.reorder">(obj({ thread: str, message: str, before: optional(str) })),
  "message.promote": cmd<"message.promote">(obj({ thread: str, message: str })),
  "run.interrupt": cmd<"run.interrupt">(obj({ thread: str })),
  "request.respond": cmd<"request.respond">(obj({ request: str, response: responseD })),
  "checkpoint.rollback": cmd<"checkpoint.rollback">(obj({ thread: str, checkpoint: str })),
  "thread.publish": cmd<"thread.publish">(obj({ thread: str, message: str, land: optional(bool) })),
  "terminal.open": cmd<"terminal.open">(
    obj({ project: optional(str), thread: optional(str), command: optional(str), script: optional(str), cols: int, rows: int }),
  ),
  "terminal.input": cmd<"terminal.input">(obj({ terminal: str, data: str })),
  "terminal.resize": cmd<"terminal.resize">(obj({ terminal: str, cols: int, rows: int })),
  "terminal.close": cmd<"terminal.close">(obj({ terminal: str })),
  "provider.refresh": cmd<"provider.refresh">(obj({})),
  "machine.configure": cmd<"machine.configure">(obj({ settings: machineSettingsD })),
  "endpoint.set": cmd<"endpoint.set">(obj({ endpoint: endpointSpecD, key: optional(keyChangeD) })),
  "endpoint.remove": cmd<"endpoint.remove">(obj({ endpoint: str })),
  "endpoint.check": cmd<"endpoint.check">(obj({ endpoint: str })),
  "gateway.configure": cmd<"gateway.configure">(obj({ settings: gatewaySettingsD })),
  "gateway.key": cmd<"gateway.key">(obj({ upstream: str, key: optional(keyChangeD) })),
  "gateway.client.issue": cmd<"gateway.client.issue">(obj({ name: str, machine: optional(str), seal_key: str })),
  "gateway.client.revoke": cmd<"gateway.client.revoke">(obj({ client: str })),
  "gateway.test": cmd<"gateway.test">(obj({ upstream: optional(str) })),
  "gateway.remove": cmd<"gateway.remove">(obj({})),
  "gateway.status": cmd<"gateway.status">(obj({})),
  "gateway.usage": cmd<"gateway.usage">(obj({ from: int, to: int, by: optional(usageByD), bucket: optional(usageBucketD) })),
  "gateway.log": cmd<"gateway.log">(obj({ after: optional(int), before: optional(int), limit: withDefault(int, () => 0), errors: optional(bool) })),
  "gateway.log.body": cmd<"gateway.log.body">(obj({ seq: int })),
  "registry.install": cmd<"registry.install">(obj({ agent: str })),
  "registry.remove": cmd<"registry.remove">(obj({ agent: str })),
  "registry.list": cmd<"registry.list">(obj({})),
  "diff.get": cmd<"diff.get">(obj({ thread: str, run: optional(str) })),
  "fs.list": cmd<"fs.list">(obj({ project: optional(str), path: str })),
  "fs.read": cmd<"fs.read">(obj({ project: str, path: str })),
  "fs.write": cmd<"fs.write">(obj({ thread: str, path: str, text: str })),
  "git.status": cmd<"git.status">(obj({ thread: str })),
  "terminal.scrollback": cmd<"terminal.scrollback">(obj({ terminal: str })),
} satisfies Record<CommandType, Decoder<object>>);

export const commandRequestD = obj<CommandRequest>({ id: str, command: commandD });

const diffFields = { stat: diffStatD, files: array(fileChangeD), truncated: withDefault(bool, () => false) };
export const replyD: Decoder<Reply> = tagged("type", {
  ok: obj<object>({}),
  created: obj<{ id: string }>({ id: str }),
  diff: obj<Diff>(diffFields),
  entries: obj<{ entries: FsEntry[] }>({
    entries: array(obj<FsEntry>({ name: str, dir: bool, size: withDefault(int, () => 0) })),
  }),
  file: obj<FileContent>({ path: str, text: opt(str), size: int }),
  git_status: obj<GitStatus>({ branch: opt(str), ahead: int, behind: int, files: array(fileChangeD) }),
  published: obj<Published>({
    branch: str,
    commit: str,
    change: opt(int),
    url: opt(str),
    landing: opt(
      obj<Landing>({ status: literal("landed", "queued", "blocked", "conflict", "failed"), message: str, blockers: vec(str) }),
    ),
  }),
  scrollback: obj<{ data: string }>({ data: str }),
  written: obj<{ path: string }>({ path: str }),
  endpoint_checked: endpointCheckD,
  gateway_client: obj<IssuedClient>({ client: gatewayClientD, sealed: sealedSecretD }),
  gateway_tested: obj<{ checks: UpstreamCheck[] }>({ checks: array(upstreamCheckD) }),
  gateway_status: gatewayStatusD,
  gateway_usage: usageReportD,
  gateway_log: obj<{ entries: LogEntry[] }>({ entries: array(logEntryD) }),
  gateway_log_body: logBodyD,
  registry: registryListD,
  pulled: pullResultD,
  cleaned: obj<CleanupReport>({ removed: array(str), kept: array(obj<{ thread: string; reason: KeptReason }>({ thread: str, reason: keptReasonD })) }),
  error: obj<{ code: ErrorCode; message: string }>({
    code: literal("not_found", "invalid", "conflict", "provider", "machine_offline", "timeout", "internal"),
    message: str,
  }),
} satisfies Record<Reply["type"], Decoder<object>>);

export const deltaD: Decoder<Delta> = tagged("type", {
  text: obj<{ thread: string; node: string; text: string }>({ thread: str, node: str, text: str }),
  output: obj<{ thread: string; node: string; text: string }>({ thread: str, node: str, text: str }),
  terminal: obj<{ terminal: string; data: string }>({ terminal: str, data: str }),
} satisfies Record<Delta["type"], Decoder<object>>);

const mirroredFields = { cursor: int, machine: str, entity: entityD };
export const mirroredD = obj<Mirrored>(mirroredFields);
export const stateD = obj<State>({ cursor: int, machines: array(machineD), entities: array(mirroredD) });

export const streamEventD: Decoder<StreamEvent> = tagged("type", {
  entity: obj<Mirrored>(mirroredFields),
  machine: obj<{ cursor: number; machine: Machine }>({ cursor: int, machine: machineD }),
  machine_removed: obj<{ cursor: number; machine: MachineId }>({ cursor: int, machine: str }),
  delta: obj<{ machine: MachineId; delta: Delta }>({ machine: str, delta: deltaD }),
  canvas: obj<{ cursor: number; revision: number; updated_at: Millis }>({ cursor: int, revision: int, updated_at: int }),
  settings: obj<{ cursor: number; revision: number; updated_at: Millis }>({ cursor: int, revision: int, updated_at: int }),
  automation: obj<{ cursor: number; automation: Automation }>({ cursor: int, automation: automationD }),
  automation_removed: obj<{ cursor: number; automation: AutomationId }>({ cursor: int, automation: str }),
  automation_run: obj<{ cursor: number; run: AutomationRun }>({ cursor: int, run: automationRunD }),
  notice: obj<{ notice: Notice }>({ notice: noticeD }),
} satisfies Record<StreamEvent["type"], Decoder<object>>);

/** The canvas document: any JSON object (the canvas parses it), kept whole. */
const canvasDocD: Decoder<Record<string, unknown> | null> = nullable((v, p) => {
  if (typeof v !== "object" || v === null || Array.isArray(v)) throw new DecodeError(p, "expected an object");
  return v as Record<string, unknown>;
});
export const savedCanvasD = obj<SavedCanvas>({ canvas: canvasDocD, revision: int, updated_at: nullable(int) });
export const savedSettingsD = obj<SavedSettings>({ settings: canvasDocD, revision: int, updated_at: nullable(int) });

const stampedD = obj<Stamped>({ seq: int, entity: entityD });
export const daemonFrameD: Decoder<DaemonFrame> = tagged("type", {
  hello: obj<{ protocol: number; info: MachineInfo }>({ protocol: int, info: machineInfoD }),
  entities: obj<{ items: Stamped[] }>({ items: array(stampedD) }),
  delta: obj<{ delta: Delta }>({ delta: deltaD }),
  reply: obj<{ id: string; result: Reply }>({ id: str, result: replyD }),
  pong: obj<object>({}),
} satisfies Record<DaemonFrame["type"], Decoder<object>>);

export const relayFrameD: Decoder<RelayFrame> = tagged("type", {
  welcome: obj<{ machine: string; owner: string; resume_from: number }>({ machine: str, owner: str, resume_from: int }),
  ack: obj<{ seq: number }>({ seq: int }),
  notice: obj<{ message: string }>({ message: str }),
  command: obj<{ id: string; by: string; command: Command; grant: CodeGrant | null }>({
    id: str,
    by: str,
    command: commandD,
    grant: opt(
      obj<CodeGrant>({
        repo: repoRefD,
        url: str,
        token: str,
        write: bool,
        branch: optional(str),
        expires_at: int,
        github: optional(obj<GrantGithub>({ owner: str, repo: str, url: str, default_branch: str })),
      }),
    ),
  }),
  ping: obj<object>({}),
} satisfies Record<RelayFrame["type"], Decoder<object>>);

export const deviceLoginD = obj<DeviceLogin>({
  device_code: str,
  user_code: str,
  verification_uri: str,
  interval: int,
  expires_in: int,
});

export const loginPollD: Decoder<LoginPoll> = tagged("status", {
  pending: obj<object>({}),
  approved: obj<{ token: string; machine: string; owner: string }>({ token: str, machine: str, owner: str }),
  expired: obj<object>({}),
  denied: obj<object>({}),
} satisfies Record<LoginPoll["status"], Decoder<object>>);

export const pendingLoginD = obj<PendingLogin>({
  user_code: str,
  name: str,
  hostname: opt(str),
  os: opt(str),
  created_at: int,
  expires_at: int,
});

export const machineLoginD = obj<MachineLogin>({ machine: machineD, owner: str });

export const shareD = obj<Share>({
  id: str,
  owner: str,
  title: str,
  thread: str,
  subtree: bool,
  machine: obj<SharedMachine>({ name: str, os: opt(str) }),
  created_at: int,
  entities: array(entityD),
  trimmed: withDefault(bool, () => false),
});

export const shareSummaryD = obj<ShareSummary>({
  id: str,
  title: str,
  thread: str,
  subtree: bool,
  url: str,
  threads: int,
  runs: int,
  bytes: int,
  created_at: int,
  created_by: str,
});

// ── helpers ──

export const isBusy = (s: ThreadStatus) => s === "starting" || s === "running" || s === "waiting";
/** Working right now (not waiting on anyone): what the canvas animates and the header counts. */
export const isRunning = (s: ThreadStatus) => s === "starting" || s === "running";
export const isRunTerminal = (s: RunStatus) =>
  s === "completed" || s === "interrupted" || s === "failed" || s === "cancelled" || s === "rolled_back";

/** A fresh command id: one per user action; retries reuse it. */
export function commandId(): string {
  const bytes = new Uint8Array(12);
  crypto.getRandomValues(bytes);
  return `cmd_${Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("")}`;
}

export function addUsage(a: Usage, b: Usage): Usage {
  return {
    input_tokens: a.input_tokens + b.input_tokens,
    output_tokens: a.output_tokens + b.output_tokens,
    cached_tokens: a.cached_tokens + b.cached_tokens,
    cost_usd: a.cost_usd == null && b.cost_usd == null ? null : (a.cost_usd ?? 0) + (b.cost_usd ?? 0),
  };
}

/** A reply that says the command failed, as an error the UI can show. */
export class CommandError extends Error {
  constructor(
    readonly code: ErrorCode,
    message: string,
  ) {
    super(message);
  }
}

/** The reply, or a `CommandError` for `{type: "error"}`. */
export function expectOk<R extends Reply>(reply: Reply, ...types: R["type"][]): R {
  if (reply.type === "error") throw new CommandError(reply.code, reply.message);
  if (types.length && !types.includes(reply.type)) throw new CommandError("internal", `unexpected reply ${reply.type}`);
  return reply as R;
}

// ── the relay API ──

/** `POST …/machines/{machine}/projects`: a new roll code repo, cloned onto the machine. */
export interface NewRepoProject {
  /** The idempotency key. */
  id: string;
  repo: { name: string; description?: string; visibility?: "public" | "private" };
  path?: string;
  name?: string;
}

/** `POST …/threads/{thread}/work`: a given item (`wi_…` or `owner/repo#N`), or the next of `repo`. */
export type TakeWork = { work: string } | { repo: string; labels?: string[] };

/** What a thread took (the work item, as roll code's API answers it; only what the canvas reads). */
export interface WorkItemRef {
  id: string;
  repo: string;
  number: number;
  title: string;
}

/** A machine minted from the dashboard ("Add machine"): its token is shown once. */
export interface MintedMachine {
  machine: Machine;
  token: string;
}

const enc = encodeURIComponent;
const ag = (owner: string) => `/v1/code/agents/${enc(owner)}`;
const query = (q: Record<string, string | undefined>) => {
  const parts = Object.entries(q)
    .filter(([, v]) => v)
    .map(([k, v]) => `${k}=${v}`);
  return parts.length ? `?${parts.join("&")}` : "";
};
const threadsParam = (threads: readonly string[]) => (threads.length ? threads.map(enc).join(",") : undefined);

/** Network failures and 5xx/408/429 are worth retrying with the same command id. */
function retryable(e: unknown) {
  if (e instanceof ApiError) return e.status >= 500 || e.status === 408 || e.status === 429;
  return e instanceof TypeError;
}

export function createAgentsApi(
  base: string,
  token?: string | (() => string | undefined),
  onUnauthorized?: () => void,
  doFetch: typeof fetch = (...a) => fetch(...a),
) {
  const authHeaders = (): Record<string, string> => {
    const t = typeof token === "function" ? token() : token;
    return t ? { authorization: `Bearer ${t}` } : {};
  };

  const call = async (method: string, path: string, body?: unknown): Promise<unknown> => {
    const headers = authHeaders();
    if (body !== undefined) headers["content-type"] = "application/json";
    const res = await doFetch(
      `${base}${path}`,
      withSession({
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        cache: "no-store",
      }),
    );
    const raw = await res.text();
    let data: unknown = null;
    try {
      data = raw ? JSON.parse(raw) : null;
    } catch {
      data = raw;
    }
    if (!res.ok) {
      if (res.status === 401) onUnauthorized?.();
      // A command the relay couldn't deliver still answers with a reply (`machine_offline`, `timeout`).
      if (data && typeof data === "object" && (data as { type?: unknown }).type === "error") return data;
      const message = (data as { error?: string } | null)?.error ?? `${res.status} ${res.statusText}`;
      throw new ApiError(message, res.status, data);
    }
    return data;
  };

  const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

  return {
    authHeaders,
    /** `/v1/code/agents/{owner}/automations…` (`automations-api.ts`). */
    automations: createAutomationsApi(call),
    /** `/v1/code/agents/{owner}/integrations…` (`integrations-api.ts`): Slack, Discord and GitHub. */
    integrations: createIntegrationsApi(call),
    /** `/v1/code/agents/{owner}/linear…` (`linear-api.ts`). */
    linear: createLinearApi(call),
    /** The owner's model gateway plan and its members (docs/code/gateway.md; `agents-gateway.ts`). */
    gateway: createGatewayApi(call),
    /** `/v1/code/agents/{owner}/lookup…` (`lookup-api.ts`): integration rows for pickers. */
    lookup: createLookupApi(call),
    /** The fetch this client uses (the event stream follows it too). */
    fetch: doFetch,
    state: async (owner: string, threads: readonly string[] = []) =>
      decode(stateD, await call("GET", `${ag(owner)}/state${query({ threads: threadsParam(threads) })}`)),
    /** The event stream after `after` (a snapshot's cursor); reconnects also send Last-Event-ID. */
    eventsUrl: (owner: string, threads: readonly string[] = [], after?: number) =>
      `${base}${ag(owner)}/events${query({ threads: threadsParam(threads), after: after ? String(after) : undefined })}`,
    /**
     * Run a command on a machine. The request id is the idempotency key, so a dropped connection
     * or a 5xx is retried with the same id (the daemon replays its stored reply).
     */
    command: async (owner: string, machine: MachineId, req: CommandRequest, retries = 3): Promise<Reply> => {
      for (let attempt = 0; ; attempt++) {
        try {
          return decode(replyD, await call("POST", `${ag(owner)}/machines/${enc(machine)}/commands`, req));
        } catch (e) {
          if (attempt >= retries || !retryable(e)) throw e;
          await sleep(300 * 2 ** attempt);
        }
      }
    },
    machines: async (owner: string) => decode(array(machineD), await call("GET", `${ag(owner)}/machines`)),
    addMachine: async (owner: string, name?: string) => {
      const r = (await call("POST", `${ag(owner)}/machines`, name ? { name } : {})) as { machine: unknown; token: string };
      return { machine: decode(machineD, r.machine), token: str(r.token, "token", { strict: false }) } satisfies MintedMachine;
    },
    renameMachine: async (owner: string, machine: MachineId, name: string) =>
      decode(machineD, await call("PATCH", `${ag(owner)}/machines/${enc(machine)}`, { name })),
    /** A machine's colour and kind (`null` clears one: it goes back to the one made from its id and OS). */
    styleMachine: async (owner: string, machine: MachineId, style: MachineStyle) =>
      decode(machineD, await call("PATCH", `${ag(owner)}/machines/${enc(machine)}`, style)),
    removeMachine: async (owner: string, machine: MachineId) => {
      await call("DELETE", `${ag(owner)}/machines/${enc(machine)}`);
    },
    /** Approve `roll agent login`'s code for `owner` (404: no pending login has it). */
    approveLogin: async (owner: string, userCode: string, name?: string) => {
      await call("POST", `${ag(owner)}/login/approve`, { user_code: userCode, ...(name ? { name } : {}) });
    },
    denyLogin: async (owner: string, userCode: string) => {
      await call("POST", `${ag(owner)}/login/deny`, { user_code: userCode });
    },
    /** Which machine a pending `roll agent login` is (404: none is waiting with the code). */
    pendingLogin: async (owner: string, userCode: string) => decode(pendingLoginD, await call("GET", `${ag(owner)}/login/${enc(userCode)}`)),
    /** Freeze a read-only copy of a thread for a public link; `subtree` adds its subagent threads. */
    createShare: async (owner: string, thread: ThreadId, subtree = false) =>
      decode(shareSummaryD, await call("POST", `${ag(owner)}/threads/${enc(thread)}/shares`, { subtree })),
    shares: async (owner: string) => decode(array(shareSummaryD), await call("GET", `${ag(owner)}/shares`)),
    revokeShare: async (owner: string, id: string) => {
      await call("DELETE", `${ag(owner)}/shares/${enc(id)}`);
    },
    /** A share by its link: no sign-in needed. */
    share: async (id: string) => decode(shareD, await call("GET", `/v1/code/agents/shares/${enc(id)}`)),
    /**
     * Create a roll code repo and clone it onto the machine as a project, in one step. `id` is the
     * idempotency key (a retry answers the same). Answers the daemon's reply (`created` = project).
     */
    createProject: async (owner: string, machine: MachineId, req: NewRepoProject) =>
      decode(replyD, await call("POST", `${ag(owner)}/machines/${enc(machine)}/projects`, req)),
    /** The thread takes a work item (`work`), or claims the next open one of `repo`; `null`: none to claim (204). */
    takeWork: async (owner: string, thread: ThreadId, req: TakeWork) =>
      (await call("POST", `${ag(owner)}/threads/${enc(thread)}/work`, req)) as WorkItemRef | null,
    /** The thread lets go of its work item (the claim stays the claimant's). */
    releaseWork: async (owner: string, thread: ThreadId) => {
      await call("DELETE", `${ag(owner)}/threads/${enc(thread)}/work`);
    },
    /** The canvas as the owner arranged it (`canvas: null` before the first save). */
    canvas: async (owner: string) => decode(savedCanvasD, await call("GET", `${ag(owner)}/canvas`)),
    /**
     * Save the canvas. With `revision`, only over that revision (0: none saved yet), else a
     * `CanvasConflict` holding what is there now; without, the last writer wins. Owners only:
     * agent tokens get a 403.
     */
    saveCanvas: async (owner: string, doc: Record<string, unknown>, revision?: number): Promise<SavedCanvas> => {
      try {
        return decode(savedCanvasD, await call("PUT", `${ag(owner)}/canvas`, revision === undefined ? { canvas: doc } : { canvas: doc, revision }));
      } catch (e) {
        if (e instanceof ApiError && e.status === 409) throw new CanvasConflict(decode(savedCanvasD, e.body));
        throw e;
      }
    },
    /** The owner's endpoint profiles (never a key; docs/code/agents.md, "Endpoints"). */
    endpoints: async (owner: string): Promise<EndpointProfile[]> => decode(array(endpointProfileD), await call("GET", `${ag(owner)}/endpoints`)),
    /** Add or replace a profile (owners only). A key never goes here: it's sealed to each machine (`agents-seal.ts`). */
    saveEndpoint: async (owner: string, profile: Omit<EndpointProfile, "updated_at">): Promise<EndpointProfile> =>
      decode(endpointProfileD, await call("PUT", `${ag(owner)}/endpoints/${encodeURIComponent(profile.id)}`, profile)),
    removeEndpoint: async (owner: string, id: string): Promise<void> => {
      await call("DELETE", `${ag(owner)}/endpoints/${encodeURIComponent(id)}`);
    },
    /** The owner's agents settings (`settings: null` before the first save). */
    settings: async (owner: string) => decode(savedSettingsD, await call("GET", `${ag(owner)}/settings`)),
    /**
     * Save the owner's agents settings. With `revision`, only over that revision, else a
     * `SettingsConflict` holding what is there now; without, the last writer wins. Owners only.
     */
    saveSettings: async (owner: string, doc: Record<string, unknown>, revision?: number): Promise<SavedSettings> => {
      try {
        return decode(savedSettingsD, await call("PUT", `${ag(owner)}/settings`, revision === undefined ? { settings: doc } : { settings: doc, revision }));
      } catch (e) {
        if (e instanceof ApiError && e.status === 409) throw new SettingsConflict(decode(savedSettingsD, e.body));
        throw e;
      }
    },
  };
}

export type AgentsApi = ReturnType<typeof createAgentsApi>;

export const agentsApi = createAgentsApi(ROLL_API, getToken, requestSignIn);
