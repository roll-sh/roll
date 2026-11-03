/** Agent records for tests (crates/agent-proto shapes, complete with defaults); each takes overrides. */
import type {
  Checkpoint,
  Entity,
  Machine,
  Mirrored,
  Node,
  NodeBody,
  Project,
  ProviderInfo,
  Request,
  Run,
  StreamEvent,
  Terminal,
  Thread,
} from "@/lib/agents-api";

export const T0 = 1_762_000_000_000;
export const M1 = "mch_1";

export function provider(over: Partial<ProviderInfo> = {}): ProviderInfo {
  return {
    id: "claude",
    name: "Claude Code",
    installed: true,
    version: "2.1.310",
    auth: "ready",
    detail: null,
    models: [
      { id: "claude-opus-5-5", name: "Opus 5.5", default: true, efforts: ["low", "medium", "high", "max"] },
      { id: "claude-sonnet-5", name: "Sonnet 5", default: false, efforts: [] },
    ],
    capabilities: { steer: true, fork: true, rollback: false, plan_mode: true, images: true, subagents: true, mode_switch: true },
    ...over,
  };
}

export function machine(over: Partial<Machine> = {}): Machine {
  return {
    id: M1,
    name: "ada-mbp",
    online: true,
    info: { hostname: "ada-mbp", os: "macos", arch: "aarch64", version: "0.1.0", home: "/Users/ada", providers: [provider()] },
    sandbox: null,
    created_at: T0,
    last_seen: T0,
    ...over,
  };
}

export function project(over: Partial<Project> = {}): Project {
  return { id: "prj_1", name: "roll", path: "/Users/ada/roll", repo: null, git: null, created_at: T0, updated_at: T0, ...over };
}

export function thread(over: Partial<Thread> = {}): Thread {
  return {
    id: "thr_1",
    project: "prj_1",
    title: "Fix the flaky queue test",
    provider: "claude",
    model: "claude-opus-5-5",
    effort: "high",
    runtime_mode: "auto_edit",
    interaction_mode: "default",
    cwd: "/Users/ada/roll",
    worktree: null,
    status: "idle",
    lineage: null,
    active_run: null,
    queued: [],
    run_count: 0,
    pending_requests: 0,
    activity: null,
    usage: { input_tokens: 0, output_tokens: 0, cached_tokens: 0, cost_usd: null },
    error: null,
    created_at: T0,
    updated_at: T0,
    archived_at: null,
    ...over,
  };
}

export function run(over: Partial<Run> = {}): Run {
  return {
    id: "run_1",
    thread: "thr_1",
    ordinal: 1,
    provider: "claude",
    model: "claude-opus-5-5",
    status: "running",
    prompt: "Find out why the queue test is flaky",
    attachments: [],
    steers: [],
    requested_at: T0,
    started_at: T0,
    completed_at: null,
    checkpoint: null,
    stat: null,
    usage: { input_tokens: 0, output_tokens: 0, cached_tokens: 0, cost_usd: null },
    error: null,
    ...over,
  };
}

export function node(body: NodeBody, over: Partial<Node> = {}): Node {
  return { id: "nod_1", thread: "thr_1", run: "run_1", parent: null, ordinal: 1, status: "running", started_at: T0, completed_at: null, body, ...over };
}

export function approval(over: Partial<Request> = {}): Request {
  return {
    id: "req_1",
    thread: "thr_1",
    run: "run_1",
    node: "nod_9",
    status: "pending",
    body: {
      type: "approval",
      action: "command",
      title: "Run `cargo test -p code-control`",
      command: "cargo test -p code-control",
      cwd: "/Users/ada/roll",
      files: [],
      reason: "Check the fix",
      tool: "Bash",
      input: null,
      decisions: ["accept", "accept_for_session", "decline", "cancel"],
    },
    created_at: T0,
    resolved_at: null,
    resolved_by: null,
    response: null,
    ...over,
  };
}

export function question(over: Partial<Request> = {}): Request {
  return approval({
    id: "req_2",
    body: {
      type: "questions",
      questions: [
        {
          id: "db",
          header: "Database",
          question: "Which database should the test use?",
          options: [
            { label: "SQLite", description: "Fast, in memory" },
            { label: "Postgres", description: null },
          ],
          multi: false,
        },
        {
          id: "checks",
          header: "Checks",
          question: "What should run after the fix?",
          options: [
            { label: "Unit tests", description: null },
            { label: "Clippy", description: null },
          ],
          multi: true,
        },
      ],
    },
    ...over,
  });
}

export function checkpoint(over: Partial<Checkpoint> = {}): Checkpoint {
  return { id: "chk_1", thread: "thr_1", run: "run_1", ordinal: 1, commit: "9fceb02d", stat: { files: 2, additions: 14, deletions: 3 }, created_at: T0, ...over };
}

export function terminal(over: Partial<Terminal> = {}): Terminal {
  return { id: "trm_1", project: "prj_1", thread: null, cwd: "/Users/ada/roll", title: "zsh", status: "running", exit_code: null, cols: 100, rows: 30, created_at: T0, ...over };
}

type Rec = { kind: Entity["kind"] } & object;

/** A stream event for a record. */
export function ev<K extends Exclude<Entity["kind"], "removed">>(kind: K, value: object, cursor: number, machine = M1): StreamEvent {
  return { type: "entity", cursor, machine, entity: { kind, ...value } as Entity };
}

export function mirrored(kind: Exclude<Entity["kind"], "removed">, value: object, cursor: number, machine = M1): Mirrored {
  return { cursor, machine, entity: { kind, ...value } as Rec as Entity };
}
