/** API-shaped fixtures for component tests (docs/code/http-api.md); each takes overrides. */
import type {
  Blame,
  BlameRange,
  Change,
  CodeInfo,
  FileDiff,
  Queue,
  QueueEntry,
  Repo,
  Review,
  ReviewAi,
  ReviewComment,
  Reviewer,
  Thread,
} from "@/lib/code-api";

export const T0 = 1_760_000_000;

export const user = (name: string): Reviewer => ({ kind: "user", name, app: null });
export const roll: Reviewer = { kind: "roll", name: "roll", app: null };

export function info(over: Partial<CodeInfo["auth"]> = {}): CodeInfo {
  return {
    storage: "local",
    git_url: "http://127.0.0.1:7070/git",
    repos: 1,
    stats: { merges_today: 0, merges_per_s: 0, queue_depth: 0 },
    github: { configured: false },
    auth: { required: false, caller: { kind: "admin", owner: null }, ...over },
  };
}

export function repo(over: Partial<Repo> = {}): Repo {
  return {
    id: "repo_1",
    owner: "acme",
    name: "web",
    full_name: "acme/web",
    description: null,
    visibility: "public",
    source: "native",
    default_branch: "main",
    head: "cs_head",
    head_git: null,
    generation: 3,
    created_at: T0,
    updated_at: T0,
    land_policy: "queue",
    required_checks: [],
    clone_url: "https://roll.sh/acme/web.git",
    web_url: "https://roll.sh/acme/web",
    github: null,
    import: null,
    languages: {},
    readme: null,
    stats: { merges_today: 0, merges_per_s: 0, queue_depth: 0, changesets: 3, open_changes: 1, open_work: 0, contributors: 1 },
    ...over,
  };
}

export function change(over: Partial<Change> = {}): Change {
  return {
    id: "chg_1",
    repo_id: "repo_1",
    repo: "acme/web",
    number: 7,
    title: "Fix the parser",
    body: "",
    author: "ada",
    state: "open",
    base: "cs_base000000",
    head: "cs_head000000",
    revision: 2,
    work_item: null,
    github_pr: null,
    github_url: null,
    blockers: [],
    checks: [],
    preview: null,
    landed_as: null,
    conflict: null,
    created_at: T0,
    updated_at: T0,
    ...over,
  };
}

/** `src/app.ts`: line 1 kept, old 2 → new 2 replaced, new 3 added, old 3/new 4 kept. */
export const PATCH = [
  "@@ -1,3 +1,4 @@",
  " const a = 1;",
  "-const b = 2;",
  "+const b = 3;",
  "+const c = 4;",
  " export { a };",
].join("\n");

export function fileDiff(over: Partial<FileDiff> = {}): FileDiff {
  return {
    path: "src/app.ts",
    old_path: null,
    change: "modified",
    mode: null,
    old_mode: null,
    binary: false,
    additions: 2,
    deletions: 1,
    patch: PATCH,
    ...over,
  };
}

export function comment(over: Partial<ReviewComment> = {}): ReviewComment {
  return {
    id: "cmt_1",
    thread: "thr_1",
    author: user("grace"),
    body: "Looks off",
    suggestion: null,
    source: "roll",
    created_at: T0,
    updated_at: T0,
    ...over,
  };
}

export function thread(over: Partial<Thread> & { body?: string } = {}): Thread {
  const { body, ...rest } = over;
  const id = rest.id ?? "thr_1";
  return {
    id,
    change: "chg_1",
    review: null,
    path: "src/app.ts",
    side: "new",
    line: 2,
    start_line: 2,
    revision: 2,
    original_line: 2,
    original_start_line: 2,
    original_revision: 2,
    outdated: false,
    resolved: false,
    resolved_by: null,
    resolved_at: null,
    source: "roll",
    created_at: T0,
    updated_at: T0,
    comments: [comment({ id: `${id}_c`, thread: id, body: body ?? "Looks off" })],
    ...rest,
  };
}

export function review(over: Partial<Review> = {}): Review {
  return {
    id: "rev_1",
    change: "chg_1",
    revision: 2,
    head: "cs_head000000",
    reviewer: user("grace"),
    verdict: "comment",
    body: "",
    source: "roll",
    url: null,
    stale: false,
    dismissed: false,
    dismissed_by: null,
    dismiss_reason: null,
    created_at: T0,
    updated_at: T0,
    ai: null,
    ...over,
  };
}

export function reviewAi(over: Partial<ReviewAi> = {}): ReviewAi {
  return {
    mode: "deep",
    chosen_by: "auto-heuristic",
    reasons: ["it touches auth", "over 400 changed lines"],
    speed: "balanced",
    incremental: false,
    provider: "anthropic",
    model: "claude-opus-5-5",
    key: "hosted",
    tokens: { input: 12_000, output: 2_000, cache_write: 0, cache_read: 0 },
    price_usd: 0.7,
    list_price_usd: 0.7,
    skipped_files: [],
    details: {
      model: "claude-opus-5-5",
      effort: "high",
      requests: 3,
      files_reviewed: 4,
      findings: 2,
      dropped: { invalid: 0, duplicates: 0, unverified: 0, low_confidence: 0 },
      skipped_files: [],
    },
    ...over,
  };
}

export function entry(over: Partial<QueueEntry> = {}): QueueEntry {
  const number = over.number ?? 1;
  return {
    id: `qe_${number}`,
    change: `chg_${number}`,
    number,
    title: `Change ${number}`,
    author: "ada",
    position: number,
    lane: "normal",
    state: "waiting",
    error: null,
    output: null,
    batch: null,
    enqueued_at: T0,
    enqueued_by: null,
    finished_at: null,
    ...over,
  };
}

export function queue(over: Partial<Queue> = {}): Queue {
  return {
    repo: "acme/web",
    policy: "queue",
    state: "idle",
    paused: false,
    batch: null,
    entries: [],
    recent: [],
    stats: { depth: 0, landed_1h: 0, avg_wait_s: 0, batch_size: 4, max_batch_size: 8 },
    settings: { checks: null, change_checks: [], batch_size: 8, timeout_s: 600, require_change_checks: false, auto_revert: false, error: null },
    last_green: null,
    ...over,
  };
}

export function blameRange(over: Partial<BlameRange> & { changeset: string; start: number; end: number }): BlameRange {
  return {
    git_commit: null,
    author: { name: "ada", email: "ada@example.com", time: T0 },
    time: T0,
    message: "a change",
    ...over,
  };
}

export function blame(over: Partial<Blame> = {}): Blame {
  return { rev: "main", changeset: "cs_head", path: "src/app.ts", lines: 0, complete: true, binary: false, ranges: [], ...over };
}
