/**
 * roll code: the control-plane API (`/v1/code/...`, crates/code-control/src/api.rs). Owners,
 * repos, the read/write API agents use instead of a checkout, changes, work items, checks,
 * previews, the landing event feed and the GitHub App. The contract is docs/code/http-api.md.
 *
 * Repos are addressed by full name, `owner/name`, everywhere.
 */

import { getToken, requestSignIn } from "./code-token";
import { ROLL_API } from "./roll-api";
import { streamSse } from "./sse";

// ── owners and names ──

export type OwnerKind = "user" | "org";

export interface Owner {
  name: string;
  kind: OwnerKind;
  display_name: string;
  bio: string | null;
  url: string | null;
  created_at: number;
  repos_count: number;
}

export type NameReason = "taken" | "reserved" | "invalid";

export interface NameCheck {
  name: string;
  available: boolean;
  reason: NameReason | null;
  message: string | null;
}

// ── repos ──

/** `queue`: nothing lands on the default branch untested. The server still accepts the old name `required_checks`. */
export type LandPolicy = "post_merge" | "queue";
export type Visibility = "public" | "private";

export interface CodeStats {
  /** Changesets landed on any ref since 00:00 UTC. */
  merges_today: number;
  /** Landing rate over the last minute. */
  merges_per_s: number;
  /** Changes waiting for the lander and in the merge queue. */
  queue_depth: number;
}

export interface CodeInfo {
  storage: string;
  /** Base of clone URLs, e.g. `https://roll.sh` or `http://127.0.0.1:7070/git`. */
  git_url: string;
  repos: number;
  stats: CodeStats;
  github: { configured: boolean };
  /** Whether the server wants tokens, and who this request's token is. */
  auth: {
    required: boolean;
    caller: { kind: "admin" | "owner" | "anonymous" | "invalid"; owner: string | null };
  };
}

export interface RepoStats extends CodeStats {
  changesets: number;
  open_changes: number;
  open_work: number;
  contributors: number;
  /** Merges per day, oldest first, for the last 30 days (sparklines). Optional in the contract. */
  merges_daily?: number[];
  /** Changes in the merge-queue batch being tested right now; 0 when none. */
  queue_batch?: number;
}

export type MirrorState = "ok" | "pending" | "error" | "paused";

export interface GithubLink {
  installation_id: number;
  owner: string;
  repo: string;
  /** https://github.com/owner/repo */
  url: string;
  /** Whether branch protection on the default branch lets only the roll App push. `null` = unknown. */
  default_branch_protected: boolean | null;
  mirror: {
    state: MirrorState;
    last_push_at: number | null;
    /** Landed commits not yet pushed to GitHub. */
    behind: number;
    error: string | null;
  };
}

export interface RepoImport {
  state: "importing" | "done" | "error";
  commits: number;
  total: number | null;
  error: string | null;
}

export interface Repo {
  id: string;
  owner: string;
  name: string;
  /** `owner/name` */
  full_name: string;
  description: string | null;
  visibility: Visibility;
  source: "native" | "github";
  default_branch: string;
  /** Changeset id at the tip of the default branch (null for an empty repo). */
  head: string | null;
  head_git: string | null;
  generation: number;
  created_at: number;
  updated_at: number;
  land_policy: LandPolicy;
  /** Checks every change must have passed before it's queued (any source). */
  required_checks: string[];
  /** https://roll.sh/owner/name.git */
  clone_url: string;
  /** https://roll.sh/owner/name */
  web_url: string;
  github: GithubLink | null;
  import: RepoImport | null;
  /** Bytes per language. */
  languages: Record<string, number>;
  /** Path of the README at head, if any. */
  readme: string | null;
  stats: RepoStats;
}

export type RepoSource = "native" | { github: { installation_id: number; owner: string; repo: string } };

export interface RepoInput {
  owner: string;
  name: string;
  description?: string;
  visibility?: Visibility;
  default_branch?: string;
  source: RepoSource;
}

export interface RepoSettings {
  name?: string;
  description?: string;
  visibility?: Visibility;
  land_policy?: LandPolicy;
  required_checks?: string[];
  default_branch?: string;
}

export interface Ref {
  name: string;
  changeset: string;
  git: string | null;
}

// ── reading code ──

export type EntryKind = "file" | "dir" | "symlink" | "submodule";

export interface TreeEntry {
  name: string;
  path: string;
  kind: EntryKind;
  /** Git mode, e.g. `100644`. */
  mode: string;
  size: number | null;
  last_change: { changeset: string; message: string; time: number } | null;
}

export interface Tree {
  rev: string;
  changeset: string;
  path: string;
  entries: TreeEntry[];
}

export interface Readme {
  path: string;
  /** Rendered, sanitized GitHub-flavoured markdown. */
  html: string;
}

/** Every file path at a revision, for the file finder. Not in the contract yet; see `paths()`. */
export interface Paths {
  changeset: string;
  paths: string[];
  truncated: boolean;
}

export interface Signature {
  name: string;
  email: string;
  /** Unix seconds. */
  time: number;
}

export interface ChangesetSummary {
  id: string;
  git_commit: string | null;
  generation: number;
  parents: string[];
  author: Signature;
  message: string;
  files_changed: number;
}

export interface FileDiff {
  path: string;
  /** For renames and copies. */
  old_path: string | null;
  change: "added" | "modified" | "deleted";
  mode: string | null;
  old_mode: string | null;
  binary: boolean;
  additions: number;
  deletions: number;
  /** Unified diff (null for binary or very large files). */
  patch: string | null;
}

export interface Changeset extends ChangesetSummary {
  committer: Signature;
  files: FileDiff[];
}

export interface Compare {
  base: string;
  head: string;
  changesets: ChangesetSummary[];
  files: FileDiff[];
}

export interface Page<T> {
  items: T[];
  cursor: string | null;
}

/** A file to write: content as text, as base64, or a delete. */
export type FileWrite = { content: string; mode?: string } | { content_b64: string; mode?: string } | { delete: true };

export interface WriteChangeset {
  base: string;
  message: string;
  author?: { name: string; email: string };
  files: Record<string, FileWrite>;
}

export type LandResult =
  | { status: "landed"; changeset: string; git_commit: string | null; ref: string; generation: number }
  | { status: "conflict"; error: string | null; paths: string[]; head: string }
  /** `queue` policy: the change waits in the merge queue (202). */
  | { status: "queued"; position: number | null; queue_id: string; change: Change }
  /** It can't be queued or land yet (422). */
  | { status: "blocked"; error: string; blockers: Blocker[] };

export type Lane = "urgent" | "normal";

// ── changes, checks, previews ──

export type ChangeState = "open" | "queued" | "landed" | "conflict" | "closed";

/** Why a change can't be queued or land right now. New kinds may come: show unknown ones by message. */
export interface Blocker {
  kind:
    | "state"
    | "draft"
    | "untrusted"
    | "checks"
    | "needs_approval"
    | "needs_codeowner"
    | "changes_requested"
    | "unresolved_threads"
    | "review_pending"
    | "review_required"
    | (string & {});
  message: string;
}

export type EntryState = "waiting" | "testing" | "bisecting" | "landing" | "ejected" | "landed";

/** A change's latest time in the merge queue. */
export interface QueueInfo {
  id: string;
  /** 1-based while queued; null once it left. */
  position: number | null;
  lane: Lane;
  state: EntryState;
  /** Why it was ejected: one line. */
  error: string | null;
  /** The failing checks' log tail behind `error`, `<check>:\n<tail>` per check. */
  output?: string | null;
  /** `qb_…` while being tested. */
  batch: string | null;
  enqueued_at: number;
  enqueued_by: string | null;
  finished_at: number | null;
}
export type CheckStatus = "queued" | "running" | "success" | "failure" | "cancelled" | "skipped";

export interface Check {
  name: string;
  status: CheckStatus;
  source: "runner" | "external" | "github";
  url: string | null;
  summary: string | null;
  started_at: number | null;
  finished_at: number | null;
}

export type PreviewState = "creating" | "running" | "sleeping" | "failed";

export interface Preview {
  state: PreviewState;
  url: string | null;
  /** Branched roll Postgres (project + branch) and roll Redis database, when configured. */
  postgres: { project_id: string; branch_id: string } | null;
  redis: { db_id: string } | null;
  error: string | null;
  created_at: number;
}

export interface Change {
  id: string;
  repo_id: string;
  /** Full name of the repo. */
  repo: string;
  /** Per repo, from 1; shared with work items. */
  number: number;
  title: string;
  body: string;
  /** Agent or human that opened it. */
  author: string;
  state: ChangeState;
  base: string;
  head: string;
  revision: number;
  work_item: string | null;
  github_pr: number | null;
  github_url: string | null;
  /** `false` for a pull request from a fork: roll runs nothing for it on its own. */
  trusted?: boolean;
  /** A writer approved running this untrusted change's current head. */
  run_approved?: boolean;
  draft?: boolean;
  /** Its latest time in the merge queue. */
  queue?: QueueInfo | null;
  /** Why it can't be queued or land right now; empty when it can. */
  blockers?: Blocker[];
  /** Where its review stands under the repo's rules. */
  review?: ReviewSummary;
  checks: Check[];
  preview: Preview | null;
  /** The changeset it landed as, once landed. */
  landed_as: string | null;
  conflict: { paths: string[]; head: string } | null;
  created_at: number;
  updated_at: number;
}

export type ChangeInput = { title: string; body?: string; work_item?: string; author?: string } & (
  | { changeset: string }
  | WriteChangeset
);

// ── merge queue ──

export type QueueState = "idle" | "paused" | "testing" | "bisecting" | "landing";

export interface QueueBatch {
  id: string;
  /** Change ids, in landing order. */
  changes: string[];
  tip: string | null;
  started_at: number;
  state: "testing" | "bisecting" | "landing";
  /**
   * How many of `changes` (from the first) the test running now covers: all of them while
   * testing, fewer while bisecting; null when no test is running (landing).
   */
  testing_prefix?: number | null;
  /** The latest test's checks. */
  checks: Check[];
}

export interface QueueEntry {
  id: string;
  change: string;
  number: number;
  title: string;
  author: string;
  position: number | null;
  lane: Lane;
  state: EntryState;
  /** Why it was ejected: one line. */
  error: string | null;
  /** The failing checks' log tail behind `error`. */
  output?: string | null;
  batch: string | null;
  enqueued_at: number;
  enqueued_by: string | null;
  finished_at: number | null;
}

export interface QueueSettings {
  /** `[queue] checks`; null: every `[checks.*]`. */
  checks: string[] | null;
  change_checks: string[];
  batch_size: number;
  timeout_s: number;
  require_change_checks: boolean;
  auto_revert: boolean;
  /** roll.toml on the default branch couldn't be read; defaults apply. */
  error: string | null;
}

export interface Queue {
  repo: string;
  policy: LandPolicy;
  state: QueueState;
  paused: boolean;
  batch: QueueBatch | null;
  /** In the queue, in order (the batch first). */
  entries: QueueEntry[];
  /** The 20 that left most recently, newest first. */
  recent: QueueEntry[];
  stats: { depth: number; landed_1h: number; avg_wait_s: number; batch_size: number; max_batch_size: number };
  settings: QueueSettings;
  /** Newest default-branch head whose checks all passed. */
  last_green: string | null;
}

export interface AuditEntry {
  id: string;
  at: number;
  actor: string;
  action:
    | "queue_bypass"
    | "queue_pause"
    | "queue_resume"
    | "queue_lane"
    | "approve_run"
    | "review_bypass"
    | "review_dismiss"
    | "review_settings"
    | "review_app_create"
    | "review_app_update"
    | "review_app_rotate"
    | "review_app_delete"
    | (string & {});
  detail: Record<string, unknown>;
}

/** The `queue` event: a summary; refetch the queue for its entries. */
export interface QueueEvent {
  state: QueueState;
  paused: boolean;
  depth: number;
  batch: QueueBatch | null;
}

// ── reviews ──

export type ReviewerKind = "user" | "agent" | "app" | "roll";

/** Who left a review or comment: a person, an agent, a review app (or a GitHub bot), or roll's reviewer. */
export interface Reviewer {
  kind: ReviewerKind;
  name: string;
  /** The review app's id, for apps registered on roll. */
  app: string | null;
}

export type Verdict = "approve" | "request_changes" | "comment";
export type ReviewSource = "roll" | "github";

export interface Review {
  id: string;
  change: string;
  /** The change revision it reviewed. */
  revision: number;
  head: string;
  reviewer: Reviewer;
  verdict: Verdict;
  body: string;
  source: ReviewSource;
  /** On GitHub, for ingested reviews. */
  url: string | null;
  /** It reviewed an older revision. */
  stale: boolean;
  dismissed: boolean;
  dismissed_by: string | null;
  dismiss_reason: string | null;
  created_at: number;
  updated_at: number;
  /** How roll's reviewer made it and what it cost; null for everyone else. */
  ai?: ReviewAi | null;
  /** Only in the answer to submitting a review: the threads it opened. */
  threads?: Thread[];
}

export interface ReviewComment {
  id: string;
  thread: string;
  author: Reviewer;
  body: string;
  /** The lines of a ```suggestion block in the body, if any. */
  suggestion: string | null;
  /** On comments roll wrote for a finding; null otherwise (and on older servers, absent). */
  finding?: CommentFinding | null;
  source: ReviewSource;
  created_at: number;
  updated_at: number;
}

export interface CommentFinding {
  severity: "bug" | "security" | "perf" | "style" | "nit";
  title: string;
  /** 0–1, for deep and max reviews. */
  confidence: number | null;
}

export type ThreadSide = "new" | "old";

export interface Thread {
  id: string;
  change: string;
  /** The review that opened it, if any. */
  review: string | null;
  path: string;
  side: ThreadSide;
  /** The lines it's on now, `start_line..=line`, 1-based, in `revision`. */
  line: number;
  start_line: number;
  revision: number;
  original_line: number;
  original_start_line: number;
  original_revision: number;
  /** Its lines changed in a later revision. */
  outdated: boolean;
  resolved: boolean;
  resolved_by: string | null;
  resolved_at: number | null;
  source: ReviewSource;
  created_at: number;
  updated_at: number;
  comments: ReviewComment[];
}

export type ReviewState = "approved" | "changes_requested" | "pending" | "auto_approved";

/** On every change: where its review stands. */
export interface ReviewSummary {
  state: ReviewState;
  /** Approvals that count. */
  approvals: number;
  required: number;
}

export interface ReviewRules {
  /** 0–20. */
  required_approvals: number;
  require_codeowners: boolean;
  count_ai_approval: boolean;
  block_on_request_changes: boolean;
  dismiss_stale: boolean;
  require_resolved_threads: boolean;
  allow_self_approval: boolean;
  auto_approve: { paths: string[]; comment_only: boolean }[];
}

export interface ReviewStatus {
  state: ReviewState;
  required: number;
  approvals: { reviewer: Reviewer; review: string; stale: boolean; counted: boolean; reason: string | null }[];
  codeowners: { path_pattern: string; owners: string[]; paths: string[]; satisfied: boolean }[];
  changes_requested: Reviewer[];
  unresolved_threads: number;
  /** Required reviews still owed. */
  pending: Reviewer[];
  blockers: Blocker[];
  auto_approved: { rule: string } | null;
  rules: ReviewRules;
}

export interface ReviewRequest {
  reviewer: Reviewer | string;
  head: string;
  /** It's for the change's current head. */
  current: boolean;
  required: boolean;
  requested_at: number;
  done_at: number | null;
}

/** A comment on lines of a file, sent with a review. */
export interface NewReviewComment {
  path: string;
  line: number;
  start_line?: number;
  side?: ThreadSide;
  body: string;
}

export interface ReviewSettings {
  /** What's stored on the repo. */
  settings: ReviewRules;
  /** `.roll/review.yaml` at the default branch's head: it can only tighten the settings. */
  file: null | { path: string; rules: Partial<ReviewRules> | null; reviewer: unknown; errors: string[] };
  codeowners: { path: string | null; rules: { pattern: string; owners: string[]; line: number }[]; errors: string[] };
  /** What applies: the settings tightened by the file. */
  effective: ReviewRules;
  /** The default-branch head the file and CODEOWNERS were read at. */
  at: string;
}

export interface ReviewApp {
  id: string;
  owner: string;
  name: string;
  webhook_url: string;
  events: string[];
  /** Full names. */
  repos: string[];
  all_repos: boolean;
  /** Every new head owes it a review. */
  required: boolean;
  active: boolean;
  created_at: number;
  updated_at: number;
}

export interface ReviewAppInput {
  name: string;
  webhook_url: string;
  events?: string[];
  repos?: string[];
  required?: boolean;
}

/** Shown once, when an app is registered or its credentials are rotated. */
export interface ReviewAppSecrets {
  token: string;
  secret: string;
}

export interface Delivery {
  id: string;
  event: string;
  state: "pending" | "delivered" | "failed";
  attempts: number;
  last_status: number | null;
  last_error: string | null;
  created_at: number;
  delivered_at: number | null;
  next_at: number | null;
  payload: unknown;
}


// ── roll's reviewer ──

export type AiMode = "standard" | "deep" | "max";
export type AiModeSetting = AiMode | "auto";
export type AiSpeed = "fast" | "balanced" | "thorough";
export type AiProvider = "anthropic" | "openai_compatible";
export type AiEffort = "low" | "medium" | "high" | "xhigh" | "max";
export type ChosenBy = "auto-heuristic" | "auto-model" | "settings" | "command";
export type KeySource = "hosted" | "byok";

export interface AiTokens {
  input: number;
  output: number;
  cache_write: number;
  cache_read: number;
}

/** A file roll's reviewer didn't review, and why (ignored, binary, too large, refused…). */
export interface SkippedFile {
  path: string;
  reason: string;
}

/** On a roll review: how it was made and what it cost. */
export interface ReviewAi {
  mode: AiMode;
  chosen_by: ChosenBy;
  reasons: string[];
  speed: AiSpeed;
  /** Only what changed since roll's last review. */
  incremental: boolean;
  provider: AiProvider;
  model: string;
  key: KeySource;
  tokens: AiTokens;
  /** Charged. */
  price_usd: number;
  list_price_usd: number;
  skipped_files: SkippedFile[];
  details: {
    model: string;
    effort: AiEffort | null;
    requests: number;
    files_reviewed: number;
    findings: number;
    dropped: { invalid: number; duplicates: number; unverified: number; low_confidence: number };
    skipped_files: SkippedFile[] | number;
  };
}

export interface ReviewerSettings {
  enabled: boolean;
  mode: AiModeSetting;
  auto: { deep_paths: string[]; max_paths: string[]; standard_below_lines: number; classifier: "heuristic" | "model" };
  speed: AiSpeed;
  auto_review: boolean;
  drafts: boolean;
  required: boolean;
  profile: "chill" | "assertive";
  ignore: string[];
  instructions: { path: string; text: string }[];
  rules: string[];
  prompt: string | null;
  prompt_file: string | null;
  model: string | null;
  secondary_model: string | null;
  effort: AiEffort | null;
  diagram: boolean;
  max_files: number;
}

export type AiJobState = "queued" | "running" | "done" | "failed" | "skipped";

export interface AiJob {
  id: string;
  repo_id: string;
  change_id: string;
  kind: "review" | "command";
  mode: string | null;
  full: boolean;
  state: AiJobState;
  attempts: number;
  error: string | null;
  review_id: string | null;
  requested_by: string | null;
  created_at: number;
  updated_at: number;
  /** Where the job is: set as it moves through the engine. */
  stage?: AiJobStage;
  /** Set once the diff is split into file groups. */
  progress?: { groups_done: number; groups_total: number } | null;
}

export type AiJobStage = "queued" | "indexing" | "chunking" | "reviewing" | "verifying" | "posting" | "done" | "failed";

export interface ReviewerView {
  /** What applies: the stored settings overlaid with `.roll/review.yaml`'s `reviewer:` section. */
  effective: ReviewerSettings;
  /** What's stored on the repo (only the keys that were set). */
  settings: Partial<ReviewerSettings>;
  /** The repo file's `reviewer:` section, as written. */
  file: Record<string, unknown> | null;
  errors: string[];
  provider: null | { source: KeySource; provider: AiProvider; model: string };
  jobs: { queued: number; running: number; last: AiJob | null };
  index: null | { head: string | null; stats: { files: number; symbols: number; refs: number; trigrams: number } };
}

export interface Learning {
  id: string;
  repo_id: string;
  /** A glob, or null for the whole repo. */
  path: string | null;
  text: string;
  source: "command" | "resolved" | "dismissed" | "api";
  created_by: string;
  thread: string | null;
  created_at: number;
}

export interface SearchMatch {
  path: string;
  line: number;
  text: string;
}

export interface SearchResult {
  matches: SearchMatch[];
  files_scanned: number;
  truncated: boolean;
  /** The indexed head (the default branch). */
  head: string | null;
  /** The changeset searched (`cs_…`): link results here. Absent on older servers. */
  rev?: string | null;
  /** With `rev`: the files of that revision that differ from the indexed head, searched directly. */
  overlay?: { base: string; files: number } | null;
}

export interface SearchQuery {
  q: string;
  regex?: boolean;
  case?: boolean;
  path?: string;
  limit?: number;
  /** A branch, change head, changeset or commit; the default branch when left out. */
  rev?: string;
}

// ── blame ──

export interface BlameRange {
  /** 1-based, inclusive. */
  start: number;
  end: number;
  changeset: string;
  git_commit: string | null;
  author: { name: string; email: string; time: number };
  time: number;
  /** First line of the message. */
  message: string;
}

export interface Blame {
  rev: string;
  changeset: string;
  path: string;
  lines: number;
  /** False when the walk stopped early; the remaining lines go to the oldest changeset reached. */
  complete: boolean;
  /** Binary files and files over 1 MiB: no ranges. */
  binary: boolean;
  ranges: BlameRange[];
}

// ── webhooks and commit statuses ──

export const WEBHOOK_EVENTS = ["landed", "change", "review", "thread", "check", "queue", "work", "preview", "push"] as const;
export type WebhookEvent = (typeof WEBHOOK_EVENTS)[number];

export interface Webhook {
  id: string;
  url: string;
  /** Event names, or `["*"]` for all of them. */
  events: string[];
  active: boolean;
  created_by: string;
  created_at: number;
  updated_at: number;
}

export interface WebhookInput {
  url?: string;
  events?: string[];
  secret?: string;
  active?: boolean;
}

export type StatusState = "pending" | "success" | "failure" | "error";

export interface Status {
  context: string;
  state: StatusState;
  target_url: string | null;
  description: string | null;
  creator: string;
  created_at: number;
  updated_at: number;
}

export interface CombinedStatus {
  changeset: string;
  git_commit: string | null;
  /** failure if any failed or errored, else pending if any is pending (or there are none), else success. */
  state: "pending" | "success" | "failure";
  total_count: number;
  statuses: Status[];
}

export interface PushEvent {
  ref: string;
  /** Null when the ref was created. */
  before: string | null;
  /** Null when the ref was deleted. */
  after: string | null;
  pusher: string | null;
  accepted: boolean;
  /** Why it was rejected (or queued). */
  reason: string | null;
}

export interface OwnerToken {
  token: string;
  id: string;
  /** The token's first 8 characters. */
  prefix: string;
  name: string;
  owner: string;
  created_at: number;
}

/** A token as listed: never the token itself. */
export interface OwnerTokenInfo {
  id: string;
  name: string;
  created_at: number;
  last_used_at: number | null;
  /** Null for tokens made before ids existed. */
  prefix: string | null;
}

export interface Competitor {
  name: string;
  per_seat_month?: number;
  per_seat_month_annual?: number;
  per_seat_month_monthly?: number;
  included_credits_per_seat?: number;
  credit_usd?: number;
  credits_per_review?: Record<string, number>;
  overage_per_reviewed_file?: number;
}

export type OnCap = "pause" | "byok_fallback" | "standard_only";

export interface Pricing {
  currency: string;
  hosted: {
    standard: { per_file: number; min_per_review: number };
    deep: { multiplier: number };
    max: { multiplier: number };
    chat: number;
  };
  byok: { standard: number; deep: number; max: number; chat: number };
  public_repos: { free_standard_reviews_per_month: number };
  human_monthly_cap: { standard: number; note: string };
  spending_caps: { on_cap: OnCap[] };
  competitors: Competitor[];
}

/** An owner's own model provider. The key itself is never returned. */
export interface ReviewProvider {
  owner: string;
  provider: AiProvider;
  base_url: string | null;
  model: string | null;
  secondary_model: string | null;
  key_last4: string;
  updated_at: number;
}

export interface ReviewProviderInput {
  provider?: AiProvider;
  api_key: string;
  base_url?: string;
  model?: string;
  secondary_model?: string;
}

export interface Billing {
  owner: string;
  monthly_cap_usd: number | null;
  on_cap: OnCap;
  month: string;
  spent_usd: number;
  remaining_usd: number | null;
  updated_at: number | null;
}

export interface UsageRow {
  id: string;
  owner_id: string;
  repo_id: string;
  change_id: string;
  review_id: string | null;
  kind: "review" | "chat";
  mode: AiMode | null;
  chosen_by: ChosenBy | null;
  reasons: string[];
  speed: AiSpeed | null;
  provider: AiProvider;
  model: string;
  key_source: KeySource;
  files_reviewed: number;
  input_tokens: number;
  output_tokens: number;
  cache_write_tokens: number;
  cache_read_tokens: number;
  /** Null for models without known rates. */
  provider_cost_usd: number | null;
  list_usd: number;
  price_usd: number;
  free: boolean;
  capped: boolean;
  human_author: boolean;
  author: string | null;
  public: boolean;
  /** YYYY-MM. */
  month: string;
  created_at: number;
}

export interface Usage {
  owner: string;
  month: string;
  totals: {
    reviews: number;
    chats: number;
    price_usd: number;
    list_usd: number;
    provider_cost_usd: number;
    free_reviews: number;
    capped_reviews: number;
    tokens: AiTokens;
  };
  by_repo: { repo: string; repo_id: string; count: number; price_usd: number }[];
  by_mode: Partial<Record<AiMode, { reviews: number; price_usd: number }>>;
  by_key: Partial<Record<KeySource, { count: number; price_usd: number }>>;
  cap: { monthly_cap_usd: number | null; on_cap: OnCap; remaining_usd: number | null };
  /** Newest first, 25 at most. */
  recent: UsageRow[];
}

// ── work items ──

export type WorkState = "open" | "claimed" | "review" | "done" | "closed";

export interface WorkItem {
  id: string;
  repo_id: string;
  repo: string;
  /** Shares the per-repo counter with changes. */
  number: number;
  title: string;
  body: string;
  labels: string[];
  priority: number;
  state: WorkState;
  /** `native` or `github#123`. */
  source: string;
  github_url: string | null;
  claimed_by: string | null;
  lease_until: number | null;
  /** Change ids. */
  changes: string[];
  created_at: number;
  updated_at: number;
}

export interface WorkInput {
  title: string;
  body?: string;
  labels?: string[];
  priority?: number;
}

// ── GitHub ──

export interface GithubApp {
  configured: boolean;
  app_slug: string | null;
  /** Where to install the App on an account or organization. */
  install_url: string | null;
}

export interface GithubRepo {
  owner: string;
  name: string;
  full_name: string;
  private: boolean;
  default_branch: string;
  /** `owner/name` of the roll repo it's already connected to. */
  connected_repo: string | null;
}

export interface GithubInstallation {
  id: number;
  account: string;
  account_type: "User" | "Organization";
  repos: GithubRepo[];
}

// ── events (SSE) ──

export interface LandedEvent {
  changeset: string;
  git_commit: string | null;
  ref: string;
  generation: number;
  message: string;
  author: string;
  /** Change id, when it landed through a change. */
  change: string | null;
  files: number;
  at: number;
}

export interface CodeEvents {
  landed: LandedEvent;
  change: Change;
  work: WorkItem;
  /** A check on a change, or a commit status posted for a changeset. */
  check: { change: string; check: Check } | { changeset: string; status: Status };
  preview: { change: string; preview: Preview | null };
  queue: QueueEvent;
  review: Review;
  thread: Thread;
  push: PushEvent;
}

// ── client ──

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly body: unknown,
  ) {
    super(message);
  }
}

const enc = encodeURIComponent;
/** `owner/name` → `/v1/code/repos/owner/name`. */
const rp = (repo: string) => {
  const [owner, name] = repo.split("/");
  return `/v1/code/repos/${enc(owner)}/${enc(name ?? "")}`;
};
const ch = (id: string) => `/v1/code/changes/${enc(id)}`;
const wk = (id: string) => `/v1/code/work/${enc(id)}`;
/** Paths keep their slashes; each segment is encoded. */
const filePath = (path: string) => path.split("/").filter(Boolean).map(enc).join("/");
const qs = (q: Record<string, string | number | undefined | null>) => {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) if (v != null && v !== "") p.set(k, String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
};

/**
 * The API against one base URL: the browser talks to `NEXT_PUBLIC_ROLL_API`, server components to
 * `ROLL_API_URL` (see code-server.ts). Requests aren't cached; pass `token` (or a function that
 * reads it) for private repos and writes. `before` runs ahead of every request: the server passes
 * `connection()` so reads happen at request time. `onUnauthorized` hears every 401.
 */
export function createCodeApi(
  base: string,
  token?: string | (() => string | undefined),
  before?: () => Promise<unknown>,
  onUnauthorized?: () => void,
) {
  const auth = (): Record<string, string> => {
    const t = typeof token === "function" ? token() : token;
    return t ? { authorization: `Bearer ${t}` } : {};
  };
  const request = async (method: string, path: string, body?: unknown): Promise<Response> => {
    await before?.();
    const headers: Record<string, string> = auth();
    if (body !== undefined) headers["content-type"] = "application/json";
    return fetch(`${base}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: "no-store",
    });
  };

  const call = async <T>(method: string, path: string, body?: unknown): Promise<T> => {
    const res = await request(method, path, body);
    const text = await res.text();
    let data: unknown = null;
    try {
      data = text ? JSON.parse(text) : null;
    } catch {
      data = text;
    }
    if (!res.ok) {
      if (res.status === 401) onUnauthorized?.();
      const message = (data as { error?: string } | null)?.error ?? `${res.status} ${res.statusText}`;
      throw new ApiError(message, res.status, data);
    }
    return data as T;
  };

  /**
   * Landing answers 409 with the conflicting paths and 422 with blockers; those are results, not
   * errors. A 202 is the queue taking the change. Anything else (403 for a bypass without the admin
   * token, 409 for a landed change) stays an error.
   */
  const land = async (path: string, body?: unknown): Promise<LandResult> => {
    try {
      return await call<LandResult>("POST", path, body);
    } catch (e) {
      const settled = landOutcome(e);
      if (settled) return settled;
      throw e;
    }
  };

  return {
    info: () => call<CodeInfo>("GET", "/v1/code"),
    checkName: (name: string) => call<NameCheck>("GET", `/v1/code/names/${enc(name)}`),

    owners: () => call<Owner[]>("GET", "/v1/code/owners"),
    owner: (name: string) => call<Owner & { repos: Repo[] }>("GET", `/v1/code/owners/${enc(name)}`),
    createOwner: (name: string, kind: OwnerKind, display_name?: string) =>
      call<Owner>("POST", "/v1/code/owners", { name, kind, display_name }),
    updateOwner: (name: string, patch: { display_name?: string; bio?: string; url?: string }) =>
      call<Owner>("PATCH", `/v1/code/owners/${enc(name)}`, patch),

    repos: (owner?: string) => call<Repo[]>("GET", `/v1/code/repos${qs({ owner })}`),
    repo: (repo: string) => call<Repo>("GET", rp(repo)),
    createRepo: (input: RepoInput) => call<Repo>("POST", "/v1/code/repos", input),
    updateRepo: (repo: string, settings: RepoSettings) => call<Repo>("PATCH", rp(repo), settings),
    deleteRepo: (repo: string) => call<null>("DELETE", rp(repo)),

    refs: (repo: string) => call<Ref[]>("GET", `${rp(repo)}/refs`),
    tree: (repo: string, rev: string, path = "") => call<Tree>("GET", `${rp(repo)}/tree/${enc(rev)}/${filePath(path)}`),
    /** Raw file bytes; fetch it directly or link to it. */
    blobUrl: (repo: string, rev: string, path: string) => `${base}${rp(repo)}/blob/${enc(rev)}/${filePath(path)}`,
    /** Raw file and the changeset it was read at (`X-Roll-Changeset`); null when missing. */
    blob: async (repo: string, rev: string, path: string) => {
      const res = await request("GET", `${rp(repo)}/blob/${enc(rev)}/${filePath(path)}`);
      if (res.status === 404) return null;
      if (!res.ok) throw new ApiError(`${res.status} ${res.statusText}`, res.status, null);
      return { bytes: new Uint8Array(await res.arrayBuffer()), changeset: res.headers.get("x-roll-changeset") };
    },
    readme: (repo: string, rev: string, path?: string) =>
      call<Readme>("GET", `${rp(repo)}/readme/${enc(rev)}${qs({ path })}`),
    paths: (repo: string, rev: string) => call<Paths>("GET", `${rp(repo)}/paths/${enc(rev)}`),
    log: (repo: string, q: { rev?: string; path?: string; limit?: number; cursor?: string | null } = {}) =>
      call<Page<ChangesetSummary>>("GET", `${rp(repo)}/log${qs(q)}`),
    changeset: (repo: string, id: string) => call<Changeset>("GET", `${rp(repo)}/changesets/${enc(id)}`),
    blame: (repo: string, rev: string, path: string) => call<Blame>("GET", `${rp(repo)}/blame/${enc(rev)}/${filePath(path)}`),
    compare: (repo: string, base: string, head: string) =>
      call<Compare>("GET", `${rp(repo)}/compare/${enc(base)}...${enc(head)}`),
    writeChangeset: (repo: string, body: WriteChangeset) => call<Changeset>("POST", `${rp(repo)}/changesets`, body),
    land: (repo: string, body: (WriteChangeset | { changeset: string }) & { ref?: string; bypass?: boolean }) =>
      land(`${rp(repo)}/land`, body),

    changes: (repo: string, state?: ChangeState) => call<Change[]>("GET", `${rp(repo)}/changes${qs({ state })}`),
    createChange: (repo: string, body: ChangeInput) => call<Change>("POST", `${rp(repo)}/changes`, body),
    /** By per-repo number, or by `chg_…` id (landed events and work items carry ids). */
    change: (repo: string, ref: string | number) =>
      typeof ref === "string" && ref.startsWith("chg_")
        ? call<Change>("GET", ch(ref))
        : call<Change>("GET", `${rp(repo)}/changes/${enc(String(ref))}`),
    revise: (id: string, body: { changeset: string } | WriteChangeset) => call<Change>("POST", `${ch(id)}/revise`, body),
    /** `bypass` (admin token) lands past the merge queue; it's audited. */
    landChange: (id: string, opts: { bypass?: boolean; lane?: Lane } = {}) =>
      land(`${ch(id)}/land`, opts.bypass || opts.lane ? opts : undefined),
    updateChange: (id: string, patch: { title?: string; body?: string; draft?: boolean }) => call<Change>("PATCH", ch(id), patch),
    approveRun: (id: string) => call<Change>("POST", `${ch(id)}/approve-run`),
    enqueue: (id: string, lane?: Lane) => land(`${ch(id)}/enqueue`, lane ? { lane } : {}),
    /** Moves a queued change to the other lane, keeping its place in time. */
    moveLane: (id: string, lane: Lane) => call<Change>("PATCH", `${ch(id)}/enqueue`, { lane }),
    dequeue: (id: string) => call<Change>("DELETE", `${ch(id)}/enqueue`),
    queue: (repo: string) => call<Queue>("GET", `${rp(repo)}/queue`),
    pauseQueue: (repo: string) => call<Queue>("POST", `${rp(repo)}/queue/pause`),
    resumeQueue: (repo: string) => call<Queue>("POST", `${rp(repo)}/queue/resume`),
    audit: (repo: string) => call<AuditEntry[]>("GET", `${rp(repo)}/audit`),
    closeChange: (id: string) => call<Change>("POST", `${ch(id)}/close`),
    reportCheck: (id: string, check: { name: string; status: CheckStatus; url?: string; summary?: string }) =>
      call<Change>("POST", `${ch(id)}/checks`, check),
    createPreview: (id: string) => call<Change>("POST", `${ch(id)}/preview`),
    deletePreview: (id: string) => call<Change>("DELETE", `${ch(id)}/preview`),

    reviews: (id: string) => call<Review[]>("GET", `${ch(id)}/reviews`),
    submitReview: (id: string, body: { verdict: Verdict; body?: string; comments?: NewReviewComment[] }) =>
      call<Review>("POST", `${ch(id)}/reviews`, body),
    dismissReview: (id: string, review: string, reason?: string) =>
      call<Review>("POST", `${ch(id)}/reviews/${enc(review)}/dismiss`, reason ? { reason } : {}),
    reviewStatus: (id: string) => call<ReviewStatus>("GET", `${ch(id)}/review-status`),
    threads: (id: string) => call<Thread[]>("GET", `${ch(id)}/threads`),
    createThread: (id: string, body: NewReviewComment) => call<Thread>("POST", `${ch(id)}/threads`, body),
    thread: (thread: string) => call<Thread>("GET", `/v1/code/threads/${enc(thread)}`),
    reply: (thread: string, body: string) => call<Thread>("POST", `/v1/code/threads/${enc(thread)}/comments`, { body }),
    resolveThread: (thread: string) => call<Thread>("POST", `/v1/code/threads/${enc(thread)}/resolve`),
    unresolveThread: (thread: string) => call<Thread>("POST", `/v1/code/threads/${enc(thread)}/unresolve`),
    /** Applies a comment's suggestion (default: the latest) as a new revision of the change. */
    applySuggestion: (thread: string, comment?: string) =>
      call<{ change: Change; thread: Thread }>("POST", `/v1/code/threads/${enc(thread)}/apply`, comment ? { comment } : {}),
    reviewRequests: (id: string) => call<ReviewRequest[]>("GET", `${ch(id)}/review-requests`),
    /** `@name`, `agent:name`, `app:name` or `roll`. */
    requestReview: (id: string, reviewer: string, required?: boolean) =>
      call<ReviewRequest[]>("POST", `${ch(id)}/review-requests`, required === undefined ? { reviewer } : { reviewer, required }),
    reviewSettings: (repo: string) => call<ReviewSettings>("GET", `${rp(repo)}/review-settings`),
    updateReviewSettings: (repo: string, patch: Partial<ReviewRules>) =>
      call<ReviewSettings>("PATCH", `${rp(repo)}/review-settings`, patch),

    reviewApps: (owner: string) => call<ReviewApp[]>("GET", `/v1/code/owners/${enc(owner)}/review-apps`),
    createReviewApp: (owner: string, input: ReviewAppInput) =>
      call<{ app: ReviewApp } & ReviewAppSecrets>("POST", `/v1/code/owners/${enc(owner)}/review-apps`, input),
    reviewApp: (id: string) => call<ReviewApp>("GET", `/v1/code/review-apps/${enc(id)}`),
    updateReviewApp: (id: string, patch: Partial<ReviewAppInput> & { active?: boolean }) =>
      call<ReviewApp>("PATCH", `/v1/code/review-apps/${enc(id)}`, patch),
    deleteReviewApp: (id: string) => call<null>("DELETE", `/v1/code/review-apps/${enc(id)}`),
    rotateReviewApp: (id: string) => call<ReviewAppSecrets>("POST", `/v1/code/review-apps/${enc(id)}/rotate`),
    deliveries: (id: string) => call<Delivery[]>("GET", `/v1/code/review-apps/${enc(id)}/deliveries`),
    redeliver: (id: string, delivery: string) =>
      call<Delivery>("POST", `/v1/code/review-apps/${enc(id)}/deliveries/${enc(delivery)}/redeliver`),

    reviewer: (repo: string) => call<ReviewerView>("GET", `${rp(repo)}/reviewer`),
    updateReviewer: (repo: string, patch: Partial<ReviewerSettings>) => call<ReviewerView>("PATCH", `${rp(repo)}/reviewer`, patch),
    /** Without `full`, only what changed since roll's last review. */
    runAiReview: (id: string, opts: { mode?: AiModeSetting; full?: boolean } = {}) =>
      call<{ job: AiJob }>("POST", `${ch(id)}/ai-review`, opts),
    aiJobs: (id: string) => call<AiJob[]>("GET", `${ch(id)}/ai-jobs`),
    learnings: (repo: string) => call<Learning[]>("GET", `${rp(repo)}/learnings`),
    addLearning: (repo: string, body: { text: string; path?: string }) => call<Learning>("POST", `${rp(repo)}/learnings`, body),
    deleteLearning: (repo: string, id: string) => call<null>("DELETE", `${rp(repo)}/learnings/${enc(id)}`),
    search: (repo: string, q: SearchQuery) =>
      call<SearchResult>(
        "GET",
        `${rp(repo)}/search${qs({ q: q.q, regex: q.regex ? "true" : undefined, case: q.case ? "true" : undefined, path: q.path, limit: q.limit, rev: q.rev })}`,
      ),
    pricing: () => call<Pricing>("GET", "/v1/code/pricing"),
    /** Null when the owner uses roll's hosted reviewer. */
    /** The owner's own key, or null when reviews use roll's hosted key. */
    reviewProvider: async (owner: string) => {
      try {
        const p = await call<ReviewProvider | (Omit<ReviewProvider, "provider"> & { provider: null })>(
          "GET",
          `/v1/code/owners/${enc(owner)}/review-provider`,
        );
        return p.provider === null ? null : (p as ReviewProvider);
      } catch (e) {
        if (e instanceof ApiError && e.status === 404) return null;
        throw e;
      }
    },
    setReviewProvider: (owner: string, input: ReviewProviderInput) =>
      call<ReviewProvider>("PUT", `/v1/code/owners/${enc(owner)}/review-provider`, input),
    clearReviewProvider: (owner: string) => call<null>("DELETE", `/v1/code/owners/${enc(owner)}/review-provider`),
    billing: (owner: string) => call<Billing>("GET", `/v1/code/owners/${enc(owner)}/billing`),
    setBilling: (owner: string, patch: { monthly_cap_usd?: number | null; on_cap?: OnCap }) =>
      call<Billing>("PUT", `/v1/code/owners/${enc(owner)}/billing`, patch),
    usage: (owner: string, month?: string) => call<Usage>("GET", `/v1/code/owners/${enc(owner)}/usage${qs({ month })}`),

    work: (repo: string, q: { state?: WorkState; label?: string } = {}) => call<WorkItem[]>("GET", `${rp(repo)}/work${qs(q)}`),
    createWork: (repo: string, body: WorkInput) => call<WorkItem>("POST", `${rp(repo)}/work`, body),
    /** The highest-priority open item, leased to `agent`; null when there's nothing to do. */
    claim: (repo: string, agent: string, opts: { labels?: string[]; lease_s?: number } = {}) =>
      call<WorkItem | null>("POST", `${rp(repo)}/work/claim`, { agent, ...opts }),
    workItem: (repo: string, number: number) => call<WorkItem>("GET", `${rp(repo)}/work/${number}`),
    updateWork: (id: string, patch: Partial<WorkInput> & { state?: WorkState }) => call<WorkItem>("PATCH", wk(id), patch),
    heartbeat: (id: string, agent?: string) => call<WorkItem>("POST", `${wk(id)}/heartbeat`, { agent }),
    release: (id: string, agent?: string) => call<WorkItem>("POST", `${wk(id)}/release`, { agent }),
    complete: (id: string, agent?: string) => call<WorkItem>("POST", `${wk(id)}/complete`, { agent }),

    webhooks: (repo: string) => call<Webhook[]>("GET", `${rp(repo)}/webhooks`),
    createWebhook: (repo: string, input: WebhookInput & { url: string }) =>
      call<{ webhook: Webhook; ping: string; secret?: string }>("POST", `${rp(repo)}/webhooks`, input),
    updateWebhook: (repo: string, id: string, patch: WebhookInput) => call<Webhook>("PATCH", `${rp(repo)}/webhooks/${enc(id)}`, patch),
    deleteWebhook: (repo: string, id: string) => call<null>("DELETE", `${rp(repo)}/webhooks/${enc(id)}`),
    pingWebhook: (repo: string, id: string) => call<Delivery>("POST", `${rp(repo)}/webhooks/${enc(id)}/ping`),
    webhookDeliveries: (repo: string, id: string) => call<Delivery[]>("GET", `${rp(repo)}/webhooks/${enc(id)}/deliveries`),
    redeliverWebhook: (repo: string, id: string, delivery: string) =>
      call<Delivery>("POST", `${rp(repo)}/webhooks/${enc(id)}/deliveries/${enc(delivery)}/redeliver`),
    postStatus: (repo: string, rev: string, body: { state: StatusState; context?: string; target_url?: string; description?: string }) =>
      call<{ changeset: string; git_commit: string | null; status: Status; changes: string[] }>("POST", `${rp(repo)}/statuses/${enc(rev)}`, body),
    statuses: (repo: string, rev: string) => call<CombinedStatus>("GET", `${rp(repo)}/statuses/${enc(rev)}`),
    createToken: (owner: string, name: string) => call<OwnerToken>("POST", `/v1/code/owners/${enc(owner)}/tokens`, { name }),
    tokens: (owner: string) => call<OwnerTokenInfo[]>("GET", `/v1/code/owners/${enc(owner)}/tokens`),
    revokeToken: (owner: string, id: string) => call<void>("DELETE", `/v1/code/owners/${enc(owner)}/tokens/${enc(id)}`),

    github: () => call<GithubApp>("GET", "/v1/code/github"),
    installations: () => call<GithubInstallation[]>("GET", "/v1/code/github/installations"),

    /** The repo's API base, for snippets agents can copy. */
    url: (repo: string) => `${base}${rp(repo)}`,
    eventsUrl: (repo: string) => `${base}${rp(repo)}/events`,
    /** The MCP endpoint, for agent configs. */
    mcpUrl: () => `${base}/v1/code/mcp`,
    /** Headers every request carries (for the event stream). */
    authHeaders: auth,
  };
}

export type CodeApi = ReturnType<typeof createCodeApi>;

/** The browser's client, against the local control plane, with the signed-in token if there is one. */
export const codeApi = createCodeApi(ROLL_API, getToken, undefined, requestSignIn);

/**
 * The land-like answers that are results rather than failures: 409 with conflicting paths and 422
 * with blockers. Null for anything else.
 */
export function landOutcome(e: unknown): LandResult | null {
  if (!(e instanceof ApiError)) return null;
  if (e.status === 409) {
    const d = e.body as { paths?: string[]; head?: string; error?: string } | null;
    if (d?.paths) return { status: "conflict", error: d.error ?? null, paths: d.paths, head: d.head ?? "" };
  }
  if (e.status === 422) {
    const d = e.body as { blockers?: Blocker[]; error?: string } | null;
    if (Array.isArray(d?.blockers)) return { status: "blocked", error: d.error ?? e.message, blockers: d.blockers };
  }
  return null;
}

/** True for a 404 from the API. */
export const isNotFound = (e: unknown) => e instanceof ApiError && e.status === 404;

export type EventHandlers = { [K in keyof CodeEvents]?: (data: CodeEvents[K]) => void } & { open?: (open: boolean) => void };

const EVENT_KINDS: (keyof CodeEvents)[] = ["landed", "change", "work", "check", "preview", "queue", "review", "thread", "push"];

/** Route one raw event to its handler; unknown kinds and bad JSON are dropped. */
export function dispatchEvent(handlers: EventHandlers, kind: string, data: string) {
  if (!EVENT_KINDS.includes(kind as keyof CodeEvents)) return;
  const fn = handlers[kind as keyof CodeEvents] as ((d: unknown) => void) | undefined;
  if (!fn) return;
  let parsed: unknown;
  try {
    parsed = JSON.parse(data);
  } catch {
    return;
  }
  fn(parsed);
}

/**
 * Subscribe to a repo's event feed (with the signed-in token, so private repos work). Reconnects
 * by itself after a drop. Returns the unsubscribe function.
 */
export function subscribe(repo: string, handlers: EventHandlers, api: CodeApi = codeApi) {
  return streamSse(api.eventsUrl(repo), {
    headers: api.authHeaders,
    onEvent: (e) => dispatchEvent(handlers, e.event, e.data),
    onOpen: (open) => handlers.open?.(open),
  });
}

// ── names ──

/** Owner names: 1–39 of `[a-z0-9-]`, no leading/trailing `-`, no `--`. Case-insensitive. */
export function validOwnerName(name: string) {
  return /^[a-z0-9](?:[a-z0-9]|-(?!-)){0,38}$/.test(name) && !name.endsWith("-");
}

/** Repo names: 1–100 of `[A-Za-z0-9._-]`, not `.`/`..`, not ending in `.git`. */
export function validRepoName(name: string) {
  return /^[A-Za-z0-9._-]{1,100}$/.test(name) && name !== "." && name !== ".." && !name.toLowerCase().endsWith(".git");
}

// ── formatting ──

/** `cs_` + 12 hex chars, the way ids are shown everywhere. */
export function shortId(id: string | null | undefined) {
  if (!id) return "—";
  const hex = id.replace(/^cs_/, "");
  return `cs_${hex.slice(0, 12)}`;
}

export function shortGit(sha: string | null | undefined) {
  return sha ? sha.slice(0, 7) : "—";
}

export function firstLine(message: string) {
  const i = message.indexOf("\n");
  return (i === -1 ? message : message.slice(0, i)).trim();
}

/** Merges per second. */
export function formatMergeRate(n: number | null | undefined) {
  if (n == null || !Number.isFinite(n)) return "—";
  if (n === 0) return "0/s";
  if (n < 0.1) return `${n.toFixed(2)}/s`;
  if (n < 10) return `${n.toFixed(1)}/s`;
  return `${Math.round(n).toLocaleString("en-US")}/s`;
}

export function cloneCommand(repo: Pick<Repo, "clone_url">) {
  return `git clone ${repo.clone_url}`;
}

/** Changes still in play first (conflicts, landing, open), then landed and closed; newest first within. */
export function sortChanges(changes: Change[]) {
  const rank: Record<ChangeState, number> = { conflict: 0, queued: 1, open: 2, landed: 3, closed: 4 };
  return [...changes].sort((a, b) => rank[a.state] - rank[b.state] || b.updated_at - a.updated_at);
}

/** Languages by share, largest first. */
export function languageShares(languages: Record<string, number>) {
  const total = Object.values(languages).reduce((s, n) => s + n, 0);
  return Object.entries(languages)
    .filter(([, n]) => n > 0)
    .sort((a, b) => b[1] - a[1])
    .map(([name, bytes]) => ({ name, bytes, share: total ? bytes / total : 0 }));
}
