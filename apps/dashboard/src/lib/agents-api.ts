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
import { getToken, requestSignIn } from "./code-token";
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
  tagged,
  withDefault,
  type Decoder,
} from "./decode";
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
  created_at: Millis;
  updated_at: Millis;
}

/** A project's settings (T3's per-project settings); `project.update` replaces them whole. */
export interface ProjectSettings {
  /** Fast-forward the checkout's default branch when the remote's moves, while it is clean. */
  auto_pull: boolean;
  /** What new threads in the project start with. */
  defaults: ThreadDefaults;
  /** Commands for the checkout or a thread's worktree (T3's project scripts). */
  scripts: ProjectScript[];
}

/** Unset fields fall back to the composer's last choice. */
export interface ThreadDefaults {
  provider?: ProviderId;
  model?: string;
  effort?: string;
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

export const NO_SETTINGS: ProjectSettings = { auto_pull: false, defaults: {}, scripts: [] };

export type RuntimeMode = "full_access" | "auto_edit" | "ask";
export type InteractionMode = "default" | "plan";
export type ThreadStatus = "idle" | "starting" | "running" | "waiting" | "error";
export type Relation = "fork" | "subagent";

export interface Worktree {
  path: string;
  branch: string;
  base: string;
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
}

export interface Thread {
  id: ThreadId;
  project: ProjectId;
  title: string;
  provider: ProviderId;
  model: string | null;
  effort: string | null;
  runtime_mode: RuntimeMode;
  interaction_mode: InteractionMode;
  cwd: string;
  worktree: Worktree | null;
  status: ThreadStatus;
  lineage: Lineage | null;
  active_run: RunId | null;
  queued: QueuedMessage[];
  run_count: number;
  pending_requests: number;
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
}

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
  requested_at: Millis;
  started_at: Millis | null;
  completed_at: Millis | null;
  checkpoint: CheckpointId | null;
  stat: DiffStat | null;
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
  | { type: "subagent"; agent: string; prompt: string; child: ThreadId | null; result: string | null }
  | { type: "request"; request: RequestId }
  | { type: "system"; text: string };

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

export interface ModelInfo {
  id: string;
  name: string;
  default: boolean;
  efforts: string[];
}

export interface Capabilities {
  steer: boolean;
  fork: boolean;
  rollback: boolean;
  plan_mode: boolean;
  images: boolean;
  subagents: boolean;
  mode_switch: boolean;
}

export interface ProviderInfo {
  id: ProviderId;
  name: string;
  installed: boolean;
  version: string | null;
  auth: AuthStatus;
  detail: string | null;
  models: ModelInfo[];
  capabilities: Capabilities;
}

export interface MachineInfo {
  hostname: string;
  os: string;
  arch: string;
  version: string;
  home: string;
  providers: ProviderInfo[];
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
      settings?: { auto_pull: boolean; defaults?: ThreadDefaults; scripts?: ProjectScript[] };
    }
  | { type: "project.pull"; project: ProjectId }
  | {
      type: "thread.create";
      project: ProjectId;
      provider: ProviderId;
      model?: string;
      effort?: string;
      runtime_mode?: RuntimeMode;
      interaction_mode?: InteractionMode;
      worktree?: boolean;
      title?: string;
      prompt?: string;
      attachments?: Attachment[];
    }
  | {
      type: "thread.configure";
      thread: ThreadId;
      title?: string;
      provider?: ProviderId;
      model?: string;
      effort?: string;
      runtime_mode?: RuntimeMode;
      interaction_mode?: InteractionMode;
    }
  | { type: "thread.fork"; thread: ThreadId; at?: RunId; worktree?: boolean }
  | { type: "thread.archive"; thread: ThreadId }
  | { type: "thread.unarchive"; thread: ThreadId }
  | { type: "thread.delete"; thread: ThreadId }
  | { type: "message.send"; thread: ThreadId; text: string; attachments?: Attachment[]; mode?: SendMode }
  | { type: "message.unqueue"; thread: ThreadId; message: string }
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
  | { type: "diff.get"; thread: ThreadId; run?: RunId }
  | { type: "fs.list"; project?: ProjectId; path: string }
  | { type: "fs.read"; project: ProjectId; path: string }
  | { type: "git.status"; thread: ThreadId }
  | { type: "terminal.scrollback"; terminal: TerminalId };

export type CommandType = Command["type"];

/** Queries only answer; they never change state. */
export const QUERIES: readonly CommandType[] = ["diff.get", "fs.list", "fs.read", "git.status", "terminal.scrollback"];

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
  | { type: "scrollback"; data: string }
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
  | { type: "canvas"; cursor: number; revision: number; updated_at: Millis };

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
  expires_at: Millis;
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
const threadDefaultsD = obj<ThreadDefaults>({
  provider: optional(str),
  model: optional(str),
  effort: optional(str),
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
const settingsFields = { auto_pull: withDefault(bool, () => false), defaults: optional(threadDefaultsD), scripts: optional(array(projectScriptD)) };
const projectSettingsD = obj<ProjectSettings>({
  auto_pull: settingsFields.auto_pull,
  defaults: withDefault(threadDefaultsD, () => ({})),
  scripts: vec(projectScriptD),
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

const contextD = obj<ContextUsage>({ used_tokens: int, max_tokens: opt(int), total_processed_tokens: opt(int) });

export const ZERO_USAGE: Usage = { input_tokens: 0, output_tokens: 0, cached_tokens: 0, cost_usd: null };

const threadD = obj<Thread>({
  id: str,
  project: str,
  title: str,
  provider: str,
  model: opt(str),
  effort: opt(str),
  runtime_mode: runtimeModeD,
  interaction_mode: interactionModeD,
  cwd: str,
  worktree: opt(obj<Worktree>({ path: str, branch: str, base: str })),
  status: literal("idle", "starting", "running", "waiting", "error"),
  lineage: opt(obj<Lineage>({ parent: str, relation: literal("fork", "subagent"), root: str, from_run: opt(str) })),
  active_run: opt(str),
  queued: vec(obj<QueuedMessage>({ id: str, text: str, attachments: vec(attachmentD), at: int })),
  run_count: int,
  pending_requests: int,
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
});
const diffStatD = obj<DiffStat>({ files: int, additions: int, deletions: int });
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
  requested_at: int,
  started_at: opt(int),
  completed_at: opt(int),
  checkpoint: opt(str),
  stat: opt(diffStatD),
  usage: usageD,
  context: opt(contextD),
  error: opt(str),
});
const changeKindD = literal("add", "modify", "delete", "rename");
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
  subagent: obj<{ agent: string; prompt: string; child: ThreadId | null; result: string | null }>({
    agent: str,
    prompt: str,
    child: opt(str),
    result: opt(str),
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
});
const providerInfoD = obj<ProviderInfo>({
  id: str,
  name: str,
  installed: bool,
  version: opt(str),
  auth: literal("ready", "missing", "unknown"),
  detail: opt(str),
  models: vec(obj<ModelInfo>({ id: str, name: str, default: withDefault(bool, () => false), efforts: vec(str) })),
  capabilities: capabilitiesD,
});
const machineInfoD = obj<MachineInfo>({
  hostname: str,
  os: str,
  arch: str,
  version: str,
  home: str,
  providers: array(providerInfoD),
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
  "thread.create": cmd<"thread.create">(
    obj({
      project: str,
      provider: str,
      model: optional(str),
      effort: optional(str),
      runtime_mode: optional(runtimeModeD),
      interaction_mode: optional(interactionModeD),
      worktree: optional(bool),
      title: optional(str),
      prompt: optional(str),
      attachments: optional(array(attachmentD)),
    }),
  ),
  "thread.configure": cmd<"thread.configure">(
    obj({
      thread: str,
      title: optional(str),
      provider: optional(str),
      model: optional(str),
      effort: optional(str),
      runtime_mode: optional(runtimeModeD),
      interaction_mode: optional(interactionModeD),
    }),
  ),
  "thread.fork": cmd<"thread.fork">(obj({ thread: str, at: optional(str), worktree: optional(bool) })),
  "thread.archive": cmd<"thread.archive">(obj({ thread: str })),
  "thread.unarchive": cmd<"thread.unarchive">(obj({ thread: str })),
  "thread.delete": cmd<"thread.delete">(obj({ thread: str })),
  "message.send": cmd<"message.send">(
    obj({ thread: str, text: str, attachments: optional(array(attachmentD)), mode: optional(literal("queue", "steer")) }),
  ),
  "message.unqueue": cmd<"message.unqueue">(obj({ thread: str, message: str })),
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
  "diff.get": cmd<"diff.get">(obj({ thread: str, run: optional(str) })),
  "fs.list": cmd<"fs.list">(obj({ project: optional(str), path: str })),
  "fs.read": cmd<"fs.read">(obj({ project: str, path: str })),
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
  pulled: pullResultD,
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
} satisfies Record<StreamEvent["type"], Decoder<object>>);

/** The canvas document: any JSON object (the canvas parses it), kept whole. */
const canvasDocD: Decoder<Record<string, unknown> | null> = nullable((v, p) => {
  if (typeof v !== "object" || v === null || Array.isArray(v)) throw new DecodeError(p, "expected an object");
  return v as Record<string, unknown>;
});
export const savedCanvasD = obj<SavedCanvas>({ canvas: canvasDocD, revision: int, updated_at: nullable(int) });

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
    grant: opt(obj<CodeGrant>({ repo: repoRefD, url: str, token: str, write: bool, expires_at: int })),
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
    const res = await doFetch(`${base}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: "no-store",
    });
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
  };
}

export type AgentsApi = ReturnType<typeof createAgentsApi>;

export const agentsApi = createAgentsApi(ROLL_API, getToken, requestSignIn);
