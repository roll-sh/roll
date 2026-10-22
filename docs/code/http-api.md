# roll code — HTTP API shapes

This is the contract between `code-control` (Rust) and the dashboard's `lib/code-api.ts`. Routes
are listed in [architecture.md](architecture.md#http-api-v1code).

- Timestamps are Unix seconds.
- Ids are strings: changesets `cs_…` (full 64-hex ids are accepted too), git commits are 40-hex.
- Errors are `{error: string}` with a matching status: 400, 401, 403, 404, 409 or 422.

## Info and names

```
GET /v1/code → {storage, git_url /* e.g. "https://roll.sh" or "http://127.0.0.1:7070/git" */, repos,
                stats: {merges_today, merges_per_s, queue_depth}, github: {configured},
                auth: {required: bool /* ROLL_CODE_TOKEN is set */,
                       caller: {kind: "admin" | "owner" | "app" | "anonymous" | "invalid", owner: string | null,
                                app?: string /* kind app */,
                                reviewer: {kind: "user" | "agent" | "app", name} | null /* who it reviews and comments as,
                                           with the request's X-Roll-Agent / X-Roll-User; null when anonymous or invalid */}}}
                The caller is whoever the request's Bearer token is; the dashboard checks a token with it, and
                compares `reviewer` with a thread's or review's author to know which are the viewer's.

GET /v1/code/names/{owner}/{repo} → same shape, for a repo name within an owner
GET /v1/code/names/{name} → {name, available: bool, reason: null | "taken" | "reserved" | "invalid", message: string | null}
```

## Owners

```
Owner = {name, kind: "user" | "org", display_name, bio: string | null, url: string | null,
         created_at, repos_count}
GET  /v1/code/owners → Owner[]
POST /v1/code/owners {name, kind, display_name?} → Owner          409 when the name is taken, 422 when reserved or invalid
GET  /v1/code/owners/{owner} → Owner & {repos: Repo[]}            only public repos without a token
PATCH /v1/code/owners/{owner} {display_name?, bio?, url?} → Owner
```

## Repos

```
Repo = {
  id, owner, name, full_name /* "owner/name" */, description: string | null,
  visibility: "public" | "private",
  source: "native" | "github", default_branch,
  head: cs id | null, head_git: sha | null, generation, created_at, updated_at,
  land_policy: "post_merge" | "queue" /* "required_checks" is accepted as an old name for "queue" */,
  required_checks: string[] /* checks every change must have passed before it's queued (any source) */,
  clone_url /* https://roll.sh/owner/name.git */, web_url /* https://roll.sh/owner/name */,
  github: null | {installation_id, owner, repo, url,
                  default_branch_protected: bool | null,
                  mirror: {state: "ok" | "pending" | "error" | "paused", last_push_at: number | null,
                           behind: number, error: string | null}},
  import: null | {state: "importing" | "done" | "error", commits, total: number | null, error: string | null},
  languages: {[name: string]: number /* bytes */},
  readme: string | null /* path of the README at head, if any */,
  stats: {merges_today, merges_per_s, queue_depth, changesets, open_changes, open_work, contributors,
          merges_daily: number[] /* 30 daily counts, oldest first */,
          queue_batch: number /* changes in the merge queue's batch being tested now; 0 when none */}
}
GET    /v1/code/repos?owner= → Repo[]
POST   /v1/code/repos {owner, name, description?, visibility?, default_branch?, source} → Repo   409 name taken; a GitHub source needs the admin token
GET    /v1/code/repos/{owner}/{repo} → Repo     an old name answers 301 with Location and {redirect: "owner/new"} (404 to callers who can't read the repo)
PATCH  /v1/code/repos/{owner}/{repo} {name?, description?, visibility?, land_policy?, required_checks?, default_branch?} → Repo
DELETE /v1/code/repos/{owner}/{repo} → 204
```

## Reading code

```
GET /refs → [{name /* "main" */, changeset, git: sha | null}]
GET /tree/{rev}/{*path} → {rev, changeset, path,
      entries: [{name, path, kind: "file" | "dir" | "symlink" | "submodule", mode /* "100644" */, size: number | null,
                 last_change: null | {changeset, message, time}}]}
GET /paths/{rev} → {changeset, paths: string[] /* every file path, for the file finder */, truncated: bool /* capped at 50k */}
GET /blob/{rev}/{*path} → raw bytes (Content-Type sniffed; X-Roll-Changeset header)
GET /readme/{rev}?path= → {path, html /* rendered GitHub-flavoured markdown, sanitized, relative links rewritten to roll.sh */}
GET /log?rev=&path=&limit=&cursor= → {items: ChangesetSummary[], cursor: string | null}
GET /blame/{rev}/{*path} → {rev /* as asked */, changeset /* resolved */, path, lines, complete: bool, binary: bool,
      ranges: [{start, end /* 1-based, inclusive */, changeset, git_commit: sha | null, author: {name, email, time},
                time, message /* first line */}]}
      first-parent history (`git blame --first-parent`); `complete: false` when the walk stopped early (2,000
      changesets touching the file), with the remaining lines given to the oldest changeset reached; binary
      files and files over 1 MiB come back `binary: true` with no ranges; 404 for a missing file or revision.
GET /changesets/{id} → Changeset
GET /compare/{base}...{head} → {base, head, changesets: ChangesetSummary[], files: FileDiff[]}
GET /compare/{base}...{head}?format=diff → text/plain: each file's unified diff after a `diff --git a/… b/…` line

ChangesetSummary = {id, git_commit: sha | null, generation, parents: string[],
                    author: {name, email, time}, message, files_changed}
Changeset = ChangesetSummary & {committer: {name, email, time}, files: FileDiff[]}
FileDiff = {path, old_path: string | null, change: "added" | "modified" | "deleted",
            mode: string | null, old_mode: string | null, binary: bool,
            additions, deletions, patch: string | null /* unified diff */}
```

## Writing and landing

```
POST /changesets {base, message, author?: {name, email}, files: {path: {content?: string, content_b64?: string, delete?: true, mode?: "100644" | "100755" | "120000"}}} → Changeset
     at most 10,000 files and 32 MiB a file; request bodies up to 64 MiB
POST /land  {base, message, files…, ref?, bypass?} | {changeset, ref?, bypass?} →     ref defaults to the default branch
     200 {status: "landed", changeset, git_commit, ref, generation, fast_forward: bool, merged_paths: string[]}
     202 {status: "queued", position, queue_id, change: Change}   land_policy "queue", default branch: the write becomes
          a change (title from the message) in the merge queue. bypass: true (admin token) lands directly, audited; 403 otherwise.
     422 {error, blockers: Blocker[]}                              it can't be queued (see Blocker)
     409 {status: "conflict", error, paths: string[], head, ref,
          conflicts: [{path, kind: "content" | "delete_modify" | "mode" | "file_directory" | "missing"}]}
```

## Changes

```
Check   = {name, status: "queued" | "running" | "success" | "failure" | "cancelled" | "skipped",
           source: "runner" | "external" | "github", url: string | null, summary: string | null,
           started_at: number | null, finished_at: number | null}
Preview = {state: "creating" | "running" | "sleeping" | "failed", url: string | null,
           postgres: {project_id, branch_id} | null, redis: {db_id} | null, error: string | null, created_at}
Change  = {id, repo_id, repo /* full name */, number /* per-repo, from 1 */, title, body, author,
           state: "open" | "queued" | "landed" | "conflict" | "closed",
           base, head, revision, work_item: string | null, branch: string | null /* git branch it was pushed as */,
           github_pr: number | null, github_url: string | null,
           trusted: bool /* false for a pull request from a fork: no automatic checks or preview */,
           run_approved: bool /* a writer approved running this untrusted change's current head */,
           draft: bool,
           checks: Check[], preview: Preview | null,
           landed_as: string | null, conflict: {paths, head} | null,
           queue: QueueInfo | null /* its latest time in the merge queue */,
           blockers: Blocker[] /* why it can't be queued or land right now; [] when it can */,
           review: ReviewSummary /* see Reviews */,
           created_at, updated_at}
Blocker   = {kind: "state" | "draft" | "untrusted" | "checks" | "needs_approval" | "needs_codeowner" | "changes_requested"
                   | "unresolved_threads" | "review_pending" | "review_required" /* more may come: show unknown kinds by message */,
             message}
QueueInfo = {id /* qe_… */, position: number | null /* 1-based while queued; null once it left */,
             lane: "urgent" | "normal", state: EntryState, error: string | null /* why it was ejected: one line */,
             output: string | null /* the failing checks' log tail behind `error`, "<check>:\n<tail>" per check */,
             batch: string | null /* qb_… while being tested */, enqueued_at, enqueued_by: string | null, finished_at: number | null}
EntryState = "waiting" | "testing" | "bisecting" | "landing" | "ejected" | "landed"

A change in the queue has state "queued". Ejected for a conflict: state "conflict" (with `conflict`);
for failing checks or a blocker: back to "open", with `queue.state` "ejected" and `queue.error`.

GET  /v1/code/repos/{owner}/{repo}/changes?state= → Change[]
POST /v1/code/repos/{owner}/{repo}/changes {title, body?, changeset | (base + message + files), work_item?, author?, draft?} → Change
GET  /v1/code/repos/{owner}/{repo}/changes/{number} → Change
GET  /v1/code/changes/{id} → Change (404 when missing)
PATCH /v1/code/changes/{id} {title?, body?, draft?} → Change          marking a queued change draft dequeues it
POST /v1/code/changes/{id}/approve-run → Change                       writers; recorded in the audit log
POST /v1/code/changes/{id}/revise {changeset | (base + message + files)} → Change
POST /v1/code/changes/{id}/land {bypass?, lane?} → same bodies as /land (409 if landed or closed)
POST /v1/code/changes/{id}/enqueue {lane?: "urgent" | "normal"} → 202 {status: "queued", position, queue_id, change}
     409 if landed or closed; 422 {error, blockers}; enqueueing a queued change returns its entry as it is
PATCH /v1/code/changes/{id}/enqueue {lane: "urgent" | "normal"} → Change     writers; 400 without a lane, 409 if it isn't queued
     moves it to the other lane, keeping its place in time (`enqueued_at` and the entry id stay); its
     position is recomputed by the queue order (urgent first, then by when it was queued). A change in
     the batch being tested moves for the next batch. Recorded in the audit log (`queue_lane`).
DELETE /v1/code/changes/{id}/enqueue → Change (back to "open"; 409 if it isn't queued)
POST /v1/code/changes/{id}/close → Change
POST /v1/code/changes/{id}/checks {name, status, url?, summary?, head?} → Change
     head: the changeset the check ran on (any unique prefix); 409 if the change has moved past it
POST /v1/code/changes/{id}/preview → Change · DELETE → Change
```

## Merge queue

```
GET  /v1/code/repos/{owner}/{repo}/queue → Queue          readable by whoever can read the repo
POST /v1/code/repos/{owner}/{repo}/queue/pause → Queue    writers; the batch being tested finishes
POST /v1/code/repos/{owner}/{repo}/queue/resume → Queue
GET  /v1/code/repos/{owner}/{repo}/audit → AuditEntry[]   writers; newest first, 200 at most

Queue = {repo /* full name */, policy: "post_merge" | "queue",
         state: "idle" | "paused" | "testing" | "bisecting" | "landing", paused: bool,
         batch: null | {id /* qb_… */, changes: string[] /* change ids, landing order */, tip: string | null,
                        started_at, state: "testing" | "bisecting" | "landing",
                        testing_prefix: number | null /* how many of `changes` (from the first) the test running
                                                         now covers: all while testing, fewer while bisecting */,
                        checks: Check[] /* the latest test's */},
         entries: Entry[] /* in the queue, in order (the batch first) */,
         recent: Entry[]  /* the 20 that left most recently, newest first */,
         stats: {depth, landed_1h, avg_wait_s /* enqueue → landed, last hour */, batch_size /* current, adaptive */, max_batch_size},
         settings: {checks: string[] | null /* [queue] checks; null: every [checks.*] */, change_checks: string[],
                    batch_size, timeout_s, require_change_checks: bool, auto_revert: bool,
                    error: string | null /* roll.toml on the default branch couldn't be read; defaults apply */},
         last_green: string | null /* newest default-branch head whose checks all passed */}
Entry = {id, change, number, title, author, position: number | null, lane, state: EntryState, error: string | null,
         output: string | null, batch: string | null, enqueued_at, enqueued_by: string | null, finished_at: number | null}
AuditEntry = {id, at, actor, action: "queue_bypass" | "queue_pause" | "queue_resume" | "queue_lane" | "approve_run"
                                     | "review_bypass" | "review_dismiss" | "review_settings" | "review_app_create"
                                     | "review_app_update" | "review_app_rotate" | "review_app_delete", detail: object}
```

Repo `stats.queue_depth` (and `GET /v1/code`'s) counts changes waiting for the lander and in the
merge queue.

## Reviews

The design is in [reviews.md](reviews.md). People, agents, review apps, GitHub reviewers and roll's
own reviewer all leave the same records.

```
Reviewer = {kind: "user" | "agent" | "app" | "roll", name, app: string | null /* the review app's id */}
Review   = {id /* rv_… */, change, revision /* the change revision it reviewed */, head, reviewer: Reviewer,
            verdict: "approve" | "request_changes" | "comment", body, source: "roll" | "github", url: string | null,
            stale: bool /* reviewed an older revision */, dismissed: bool, dismissed_by: string | null,
            dismiss_reason: string | null, created_at, updated_at,
            ai: null | ReviewAi /* roll's reviewer only, see "roll's reviewer" */,
            threads?: Thread[] /* only in the response to POST …/reviews: the threads it opened */}
Thread   = {id /* th_… */, change, review: string | null, path, side: "new" | "old",
            line, start_line /* the lines it's on now (start_line..=line, 1-based) */, revision /* … in this revision */,
            original_line, original_start_line, original_revision,
            outdated: bool /* its lines changed in a later revision */, resolved: bool, resolved_by: string | null,
            resolved_at: number | null, source: "roll" | "github", created_at, updated_at, comments: Comment[]}
Comment  = {id /* cm_… */, thread, author: Reviewer, body, suggestion: string | null /* a ```suggestion block's lines */,
            source, created_at, updated_at,
            finding: null | {severity, title, confidence: number | null} /* roll's finding comments, see "roll's reviewer" */}
ReviewSummary = {state: "approved" | "changes_requested" | "pending" | "auto_approved", approvals /* that count */, required}
ReviewStatus  = {state, required, approvals: [{reviewer, review, stale, counted: bool, reason: string | null}],
                 codeowners: [{path_pattern, owners: string[], paths: string[], satisfied: bool}],
                 changes_requested: Reviewer[], unresolved_threads, pending: Reviewer[] /* required reviews owed */,
                 blockers: Blocker[], auto_approved: {rule: string} | null, rules: Rules}
Rules = {required_approvals /* 0-20 */, require_codeowners, count_ai_approval, block_on_request_changes, dismiss_stale,
         require_resolved_threads, allow_self_approval, auto_approve: [{paths: string[], comment_only: bool}]}

GET   /v1/code/changes/{id}/reviews → Review[]                      oldest first
POST  /v1/code/changes/{id}/reviews {verdict, body?, comments?: [{path, line, start_line?, side?, body}]} → 201 Review
      approve / request_changes need write access (or a review app installed on the repo); comment needs a sign-in.
      request_changes and comment need a body or comments. 400 bad input; an inline comment that can't be placed
      (off the file, past its last line, a path not in that side) answers 400 {error: "comment 1: …", comment: 1 /* 0-based
      index into comments */} and nothing is written. 409 landed/closed.
POST  /v1/code/changes/{id}/reviews/{rid}/dismiss {reason?} → Review   writers, or the reviewer themself; 409 already;
      400 for comment reviews
GET   /v1/code/changes/{id}/review-status → ReviewStatus
GET   /v1/code/changes/{id}/threads → Thread[]
POST  /v1/code/changes/{id}/threads {path, line, start_line?, side?: "new" | "old", body} → 201 Thread    signed in
GET   /v1/code/threads/{id} → Thread
POST  /v1/code/threads/{id}/comments {body} → 201 Thread             signed in
POST  /v1/code/threads/{id}/resolve · /unresolve → Thread           writers, or whoever started the thread
POST  /v1/code/threads/{id}/apply {comment?} → {change: Change, thread: Thread}
      writers; applies the comment's (default: the latest) suggestion as a new revision and resolves the thread.
      409 when its lines changed since (outdated, or for an older revision); 422 without a suggestion or on the old side
GET   /v1/code/changes/{id}/review-requests → [{reviewer: Reviewer, head, current: bool /* for the change's head */, required,
      requested_at, done_at: number | null}]
POST  /v1/code/changes/{id}/review-requests {reviewer: "@name" | "agent:name" | "app:name" | "roll", required?} → 201 (the list)
      writers; a required request adds a review_pending blocker until that reviewer reviews the current head
GET   /v1/code/repos/{owner}/{repo}/review-settings → {settings: Rules /* stored */,
      file: null /* no .roll/review.yaml */ | {path: ".roll/review.yaml",
            rules: null | RulesPatch /* the rule keys the file sets, each null when it doesn't set it:
                   {required_approvals, require_codeowners, count_ai_approval, block_on_request_changes, dismiss_stale,
                    require_resolved_threads, allow_self_approval, auto_approve} */,
            reviewer: null | object /* the file's `reviewer:` section as written; see GET …/reviewer for what it means */,
            errors: string[]},
      codeowners: {path: string | null /* null: no CODEOWNERS file */, rules: [{pattern, owners: string[], line}], errors: string[]},
      effective: Rules, at: string | null /* the default head it was read at */}
PATCH /v1/code/repos/{owner}/{repo}/review-settings {…Rules fields} → same; writers; 400 unknown or invalid fields
```

- **Who reviews:** a review app's token reviews as the app. An owner token reviews as the owner,
  or as an agent with `X-Roll-Agent: name` (or when the token is named `agent:name`). The admin token
  (every caller of an open daemon) reviews as `X-Roll-Agent`, else `X-Roll-User`, else `admin`.
- **Rules:** the effective rules are the repo's settings tightened by `.roll/review.yaml` at the
  default branch's head, which can't lower them (auto_approve rules from both apply). CODEOWNERS is read
  from `CODEOWNERS`, `.roll/CODEOWNERS`, `.github/CODEOWNERS` or `docs/CODEOWNERS`. A change's paths are
  everything since its merge base with the default branch.
- **Landing:** on a repo whose rules require reviews, landing straight on the default branch
  (`/land`, `git push` to it or to `refs/land/<default>`) opens a change instead: 422 with a
  `review_required` blocker naming it (git: the push is refused with that message). Admins can
  pass `bypass: true` on `/land` and `/changes/{id}/land` (audited as `review_bypass`). The queue
  checks reviews before queueing and before landing a batch.

### Review apps

```
ReviewApp = {id /* app_… */, owner, name, webhook_url, events: string[], repos: string[] /* full names */,
             all_repos: bool, required: bool, active: bool, created_at, updated_at}
Delivery  = {id /* dl_… */, event, state: "pending" | "delivered" | "failed", attempts, last_status: number | null,
             last_error: string | null, last_response: string | null /* the start of the last attempt's
             response body, 2 KB at most, cut at a character */, created_at, delivered_at: number | null,
             next_at: number | null, payload}

GET    /v1/code/owners/{owner}/review-apps → ReviewApp[]                 the owner (or admin)
POST   /v1/code/owners/{owner}/review-apps {name, webhook_url, events?, repos?, required?}
       → 201 {app: ReviewApp, token /* rla_…, shown once */, secret /* rls_…, shown once */}   409 name taken
GET    /v1/code/review-apps/{id} → ReviewApp · PATCH {name?, webhook_url?, events?, repos?, required?, active?} → ReviewApp
DELETE /v1/code/review-apps/{id} → 204
POST   /v1/code/review-apps/{id}/rotate → {token, secret}               the old ones stop working
GET    /v1/code/review-apps/{id}/deliveries → Delivery[]                newest first, 100 at most
POST   /v1/code/review-apps/{id}/deliveries/{did}/redeliver → Delivery
```

`repos` lists the repos an app is installed on; leaving it out (or `[]`) installs it on every repo of
the owner, now and later: `all_repos: true` and `repos: []`.

**Renamed files:** roll doesn't detect renames. A renamed file is a deleted path plus an added one
(`FileDiff.old_path` is always null), so a thread on the old side (`side: "old"`) uses the old path
and one on the new side uses the new path; the other combinations are 400.

Events: `change.opened`, `change.revised`, `thread.comment` (default: those three) and
`review.requested` (always sent to required apps). A delivery is a POST of
`{event, delivery, app, repo, change, thread?, comment?, required?, urls: {repo, change, compare, diff, blob,
reviews, threads, web}}` with headers `X-Roll-Event`, `X-Roll-Delivery`, `X-Roll-Timestamp` and
`X-Roll-Signature-256: sha256=<hex HMAC-SHA256(secret, "{timestamp}.{body}")>`. Non-2xx answers
are retried with backoff (8 attempts), then the delivery is `failed`. `urls.diff` is
`GET …/compare/{base}...{head}?format=diff` (text/plain unified diffs).

## roll's reviewer

The design is in [reviews.md](reviews.md#rolls-reviewer-code-review-crate). Its reviews are
ordinary `Review`s by `{kind: "roll", name: "roll"}` with an `ai` record; its findings are threads.

```
ReviewAi = {mode: "standard" | "deep" | "max", chosen_by: "auto-heuristic" | "auto-model" | "settings" | "command",
            reasons: string[], speed: "fast" | "balanced" | "thorough", incremental: bool /* only what changed since its last review */,
            provider: "anthropic" | "openai_compatible", model, key: "hosted" | "byok",
            tokens: {input, output, cache_write, cache_read}, price_usd /* charged */, list_price_usd,
            skipped_files: [{path, reason}],
            details: {model, effort, requests, files_reviewed, findings,
                      dropped: {invalid, duplicates, unverified, low_confidence}, skipped_files}}
ReviewerSettings = {enabled, mode: "auto" | "standard" | "deep" | "max",
                    auto: {deep_paths: string[], max_paths: string[], standard_below_lines, classifier: "heuristic" | "model"},
                    speed, auto_review, drafts, required, profile: "chill" | "assertive", ignore: string[],
                    instructions: [{path, text}], rules: string[], prompt: string | null, prompt_file: string | null,
                    model: string | null, secondary_model: string | null,
                    effort: null | "low" | "medium" | "high" | "xhigh" | "max", diagram, max_files}
Job      = {id /* aij_… */, repo_id, change_id, kind: "review" | "command", mode: string | null, full,
            state: "queued" | "running" | "done" | "failed" | "skipped", attempts, error: string | null,
            review_id: string | null, requested_by: string | null, created_at, updated_at,
            stage: "queued" | "indexing" | "chunking" | "reviewing" | "verifying" | "posting" | "done" | "failed",
            progress: null | {groups_done, groups_total} /* set once the diff is split into groups */}
Learning = {id /* lrn_… */, repo_id, path: string | null /* a glob */, text,
            source: "command" | "resolved" | "dismissed" | "api", created_by, thread: string | null, created_at}
UsageRow = {id, owner_id, repo_id, change_id, review_id, kind: "review" | "chat", mode, chosen_by, reasons, speed, provider,
            model, key_source, files_reviewed, input_tokens, output_tokens, cache_write_tokens, cache_read_tokens,
            provider_cost_usd: number | null /* unknown model rates */, list_usd, price_usd, free, capped,
            human_author, author, public, month /* YYYY-MM */, created_at}

GET   /v1/code/repos/{owner}/{repo}/reviewer → {effective: ReviewerSettings, settings: object /* stored */,
      file: object | null /* .roll/review.yaml's reviewer: section */, errors: string[],
      provider: null | {source: "hosted" | "byok", provider, model},
      jobs: {queued, running, last: Job | null}, index: null | {head: string | null, stats: {files, symbols, refs, trigrams}}}
PATCH /v1/code/repos/{owner}/{repo}/reviewer {…ReviewerSettings fields} → same      writers; 400 names the bad field
POST  /v1/code/changes/{id}/ai-review {mode?: "auto" | "standard" | "deep" | "max", full?: bool} → 202 {job: Job}
      writers; 409 landed/closed. Without `full`, only what changed since roll's last review is reviewed.
GET   /v1/code/changes/{id}/ai-jobs → Job[]                                          oldest first
GET   /v1/code/repos/{owner}/{repo}/learnings → Learning[]
POST  /v1/code/repos/{owner}/{repo}/learnings {text, path?} → 201 Learning          writers
DELETE /v1/code/repos/{owner}/{repo}/learnings/{id} → 204                           writers; 404 unknown
GET   /v1/code/repos/{owner}/{repo}/search?q=&regex=&case=&path=&limit=&rev= →
      {matches: [{path, line, text}], files_scanned, truncated,
       head: string | null /* the indexed head (default branch) */,
       rev: string | null /* the changeset searched (cs_…): link results here */,
       overlay: null | {base /* the indexed head */, files /* files of `rev` that differ, searched directly */}}
      literal and case-insensitive by default; `path` is a glob; `limit` 1-500 (default 100); 400 bad regex;
      `rev` (a branch, change head, changeset or commit; 404 unknown) searches that revision: files that differ
      from the default head are searched at `rev` (deleted ones drop out), the index answers for the rest.
      Matches are sorted by path, then line.
GET   /v1/code/pricing → {currency, hosted: {standard: {per_file, min_per_review}, deep: {multiplier}, max: {multiplier}, chat},
      byok: {standard, deep, max, chat}, public_repos: {free_standard_reviews_per_month},
      human_monthly_cap: {standard, note}, spending_caps: {on_cap: string[]}, competitors: [...]}      public
GET   /v1/code/owners/{owner}/review-provider → {owner, provider, base_url, model, secondary_model, key_last4, updated_at}
      every field but `owner` is null without one (hosted); 404 to anyone but the owner or admin (the key itself is never returned)
PUT   /v1/code/owners/{owner}/review-provider {provider?: "anthropic" | "openai_compatible", api_key, base_url?, model?,
      secondary_model?} → same      422 without ROLL_SECRET_KEY on the server; 400 openai_compatible without base_url + model
      `base_url` can't carry credentials, and with an admin token it can't be a loopback, private,
      link-local or metadata address (400). Names resolving to one are refused on every connection;
      redirects aren't followed; answers over 16 MiB are refused. `ROLL_CODE_PROVIDER_ALLOW_PRIVATE=1`
      allows private endpoints (a self-hosted model on the operator's network).
GET   /v1/code/repos/{owner}/{repo}/reviewer → for readers who can't write, a custom `prompt` set
      through the API shows as "(hidden)"
DELETE /v1/code/owners/{owner}/review-provider → 204
      Auth for review-provider, billing and usage: the owner's own token (`rlc_…` of that owner) or the
      admin token (on an open local daemon everyone is the admin). Reads by anyone else answer 404 (so
      they don't learn what's configured); writes answer 401 without a token and 403 with another
      owner's or an app's token.
GET   /v1/code/owners/{owner}/billing → {owner, monthly_cap_usd: number | null, on_cap: "pause" | "byok_fallback" | "standard_only",
      month, spent_usd, remaining_usd: number | null, updated_at: number | null}
PUT   /v1/code/owners/{owner}/billing {monthly_cap_usd?: number | null, on_cap?} → same
GET   /v1/code/owners/{owner}/usage?month=YYYY-MM → {owner, month,
      totals: {reviews, chats, price_usd, list_usd, provider_cost_usd, free_reviews, capped_reviews,
               tokens: {input, output, cache_write, cache_read}},
      by_repo: [{repo, repo_id, count /* reviews and chat replies */, price_usd}], by_mode: {standard?: {reviews, price_usd}, deep?: …, max?: …},
      by_key: {hosted?: {count, price_usd}, byok?: …},
      cap: {monthly_cap_usd, on_cap, remaining_usd}, recent: UsageRow[] /* newest first, 25 */}
```

- **Commands** in a thread comment (`@roll review [mode]`, `@roll full review`, `@roll pause`,
  `@roll resume`, `@roll resolve`, `@roll explain`, `@roll fix`, `@roll don't flag …`, any other
  question) run for writers only; roll answers in the thread. `explain`, `fix` and questions are
  jobs (`kind: "command"`) metered as `chat`.
- **On GitHub** (connected repos), the same commands work in a pull request's conversation and in
  inline review comments, for people GitHub reports as `admin`, `maintain` or `write` on the repo
  (anyone it can't vouch for is a reader and is ignored). Conversation commands are answered with a
  pull request comment; `explain`, `fix` and questions need a line, so there they get a pointer to
  inline comments instead. Inline commands are answered in the thread (posted as a GitHub reply).
- **Findings as data:** a comment roll wrote for a finding carries
  `finding: {severity: "bug" | "security" | "perf" | "style" | "nit", title, confidence: number | null /* 0–1, deep and max */}`
  next to its text (`**severity** · title`, then the explanation); other comments have `finding: null`.
- **Skipped files** (`ai.skipped_files`, `ai.details.skipped_files`) are `[{path, reason}]`: ignored by
  settings, binary, too large, no textual changes, declined by the model.
- **Human authors** (`UsageRow.human_author`, the monthly standard-review cap for people): a change's
  author is an agent — not a person — when the author name starts with `agent:`, ends in `[bot]`
  (GitHub Apps) or is `roll`, or when the change does a work item that an agent claimed. Everyone
  else is a person.
- **Events:** roll's reviews and threads go out as `review` and `thread` events like anyone's (a
  review's event repeats once its `ai` record is attached).

## Work items

```
WorkItem = {id, repo_id, repo, number /* shares the per-repo counter with changes */, title, body, labels: string[],
            priority /* 0 = P0 … 3 */, state: "open" | "claimed" | "review" | "done" | "closed",
            source: "native" | "github#123", github_url: string | null,
            claimed_by: string | null, lease_until: number | null, changes: string[], created_at, updated_at}

GET   /v1/code/repos/{owner}/{repo}/work?state=&label= → WorkItem[]
POST  /v1/code/repos/{owner}/{repo}/work {title, body?, labels?, priority?} → WorkItem
POST  /v1/code/repos/{owner}/{repo}/work/claim {agent, labels?, lease_s?} → WorkItem | 204
GET   /v1/code/repos/{owner}/{repo}/work/{number} → WorkItem
GET   /v1/code/work/{id} → WorkItem · PATCH {title?, body?, labels?, priority?, state?} → WorkItem
POST  /v1/code/work/{id}/heartbeat | /release | /complete {agent?} → WorkItem
```

## Events (SSE)

`GET /v1/code/repos/{owner}/{repo}/events` sends the event types `landed`, `change`, `work`, `check`,
`preview`, `queue`, `review`, `thread` and `push`. Repo webhooks get the same events with the same data.

```
landed  = {changeset, git_commit: sha | null, ref, generation, message, author, change: string | null, files: number, at, count}
change  = Change      work = WorkItem      preview = {change, preview: Preview}
check   = {change, check: Check}  |  {changeset, status: Status} /* a commit status was posted */
queue   = {state, paused, depth, batch: Queue.batch}   a summary: refetch GET …/queue for the entries
review  = Review      thread = Thread
push    = {ref /* refs/heads/… */, before: sha | null /* null: created */, after: sha | null /* null: deleted */,
           pusher: string | null, accepted: bool, reason: string | null /* why it was rejected (or queued) */}
```

## Webhooks and commit statuses

```
Webhook = {id /* wh_… */, url, events: string[], active, created_by, created_at, updated_at}
Status  = {context, state: "pending" | "success" | "failure" | "error", target_url: string | null,
           description: string | null, creator, created_at, updated_at}

GET    /v1/code/repos/{owner}/{repo}/webhooks → Webhook[]                       writers (401 anonymous, 403 others)
POST   /v1/code/repos/{owner}/{repo}/webhooks {url, events?, secret?, active?}
       → 201 {webhook: Webhook, ping: delivery id, secret? /* only when roll generated it: shown once */}
       events: any of landed, change, review, thread, check, queue, work, preview, push, or ["*"] (default
       ["landed"]); secret: 16-200 characters (default: generated, rlw_…); 400 bad url/events/secret;
       at most 20 per repo (422)
GET    /v1/code/repos/{owner}/{repo}/webhooks/{id} → Webhook · PATCH {url?, events?, secret?, active?} → Webhook
DELETE /v1/code/repos/{owner}/{repo}/webhooks/{id} → 204
POST   /v1/code/repos/{owner}/{repo}/webhooks/{id}/ping → 202 Delivery
GET    /v1/code/repos/{owner}/{repo}/webhooks/{id}/deliveries → Delivery[]       newest first, 100 at most
POST   /v1/code/repos/{owner}/{repo}/webhooks/{id}/deliveries/{did}/redeliver → Delivery

POST   /v1/code/repos/{owner}/{repo}/statuses/{rev} {state, context? /* default "default" */, target_url?, description?}
       → 201 {changeset, git_commit: sha | null, status: Status, changes: string[] /* changes it became a check on */}
       writers; `rev` is a git commit (what CI knows), a changeset or a branch; 404 unknown; 400 bad state,
       non-http(s) target_url, context over 100 characters
GET    /v1/code/repos/{owner}/{repo}/statuses/{rev} → {changeset, git_commit, state /* combined */, total_count,
       statuses: Status[]}      readers; combined: failure if any failed or errored, else pending if any is
       pending (or there are none), else success
```

- **Deliveries** are `Delivery`s as for review apps: `POST` to the URL with `X-Roll-Event`,
  `X-Roll-Delivery`, `X-Roll-Timestamp` and `X-Roll-Signature-256: sha256=<hex HMAC-SHA256(secret,
  "{timestamp}.{body}")>`; anything but a 2xx is retried with backoff (8 attempts), then `failed`.
  The body is `{event, delivery, webhook, repo: {id, full_name, default_branch, visibility},
  data /* the event's data, as on /events */}`; `ping` is sent on creation and on request.
- **`landed` never drops one.** Landings are read from the store's land log (written in each
  landing's own transaction) after a cursor kept in the control database, not from the in-memory
  broadcast: a burst larger than the broadcast channel, or a daemon that stopped between a landing
  and its delivery, still delivers every landing, at least once, once per changeset (a stack or a
  merge-queue batch is one `landed` per changeset, `count: 1`). A new webhook only gets landings
  after it was created. Deliveries go out concurrently, so order isn't promised: sort by
  `data.generation`. The land log keeps 7 days.
- **SSRF**: with an admin token configured, deliveries to loopback, private, link-local and metadata
  addresses fail (`last_error: "… private address"`), checked on every connection. This includes
  NAT64/6to4 addresses that wrap one. For local development, `ROLL_CODE_WEBHOOK_ALLOW_PRIVATE=1`
  (`--code-webhook-allow-private`) allows them on a token-mode daemon too, for webhooks and
  review apps. `=0` refuses them even on an open daemon. Never set it on a shared server.
- **A status becomes a check** named after its context on every open change whose head is that
  commit: `pending` → `running`, `success` → `success`, `failure` and `error` → `failure` (an
  error's summary starts with `error:`), `source: "external"`, `url` = `target_url`. That's what
  a queue repo's `required_checks` look at, so external CI gates the merge queue like roll's
  runner.
- **A broken default branch.** A `failure` or `error` on a commit of the default branch (its head
  or a first-parent ancestor) opens a `broken-main` work item (P0) for that context, labelled
  `broken-main` and `status:<context>`, once: further failures of the context don't open another
  while it's open. roll can't re-run external CI, so the culprit comes from the statuses CI
  already reported: when the commit right after the context's last `success` is the first one
  reported failing, it's the culprit, and gets a `Revert: …` change (landed or queued with
  `[main] auto_revert`); otherwise the item names the range (`passed at …, failed at …`). A
  `success` for the context on that commit or a later one closes the item (and roll's unneeded
  revert), unless an agent claimed it. Items from roll's own runner (no `status:` label) are
  separate. Statuses on other commits are only recorded.
- **Wiring CI**: give the CI job an owner token (`roll code token create acme ci`) and, on each
  run, `curl -X POST -H "Authorization: Bearer $ROLL_TOKEN" -d '{"state":"pending","context":"ci/teamcity","target_url":"…"}'
  $ROLL_API/v1/code/repos/acme/api/statuses/$GIT_COMMIT`, then the same with `success` or
  `failure`. TeamCity (`%build.vcs.number%`), Jenkins (`$GIT_COMMIT`), Buildkite
  (`$BUILDKITE_COMMIT`) and GitHub Actions-style runners (`$GITHUB_SHA`) all expose the commit.
  To trigger CI from roll, add a webhook for `push` (branches) and `landed` (main).

## MCP

`/v1/code/mcp` is an MCP server (Streamable HTTP, JSON-RPC 2.0) whose tools call this API on the
caller's behalf. Responses are plain JSON, or server-sent events when the request's `Accept` is
`text/event-stream` without `application/json`. Tools, sessions and client setup are in
[mcp.md](mcp.md).

```
POST   /v1/code/mcp     JSON-RPC: initialize (→ Mcp-Session-Id header), ping, tools/list, tools/call;
                        400 without a session (or a bad MCP-Protocol-Version), 404 unknown/ended session or
                        another token's session, 401 a revoked or invalid token (the session ends),
                        403 a browser Origin other than the web app's or the API's.
                        Accept: text/event-stream only → 200 text/event-stream: one `message` event per
                        response, `:` keep-alive comments every 15 s while a call runs (initialize still
                        answers JSON; notifications alone still 202)
GET    /v1/code/mcp     405 (no server-sent stream)
DELETE /v1/code/mcp     ends the session: 204, then 404
```

## GitHub

```
GET  /v1/code/github → {configured, app_slug: string | null, install_url: string | null}
GET  /v1/code/github/installations → [{id, account, account_type: "User" | "Organization",
       repos: [{owner, name, full_name, private, default_branch, connected_repo: "owner/name" | null}]}]
POST /v1/code/github/webhook          X-Hub-Signature-256 verified; 401 on a bad signature, 202 when processing outlives GitHub's timeout
GET  /v1/code/github/setup            the App's setup URL: 302 to {web}/dashboard/code?github=installed
```

`Repo.github` also has `remote_head` (GitHub's default branch as last seen) and `connected` (it's
`false` once the App loses access). With `ROLL_CODE_TOKEN` set, listing installations and
connecting a repository need the admin token: an installation isn't tied to a roll owner, so an
owner token could otherwise import another account's repositories.

## CORS

The browser calls `/v1/code/*` directly: reads, SSE and raw blob links. Allow any origin, plus
`Authorization` and `Content-Type`, and set `Access-Control-Expose-Headers: x-roll-changeset`.

## Auth

- The local daemon is open, like the rest of the local API, unless `ROLL_CODE_TOKEN` is set.
- Owner tokens are created with `POST /v1/code/owners/{owner}/tokens {name}` →
  `{token /* rlc_…, shown once */, id /* tok_… */, prefix /* the token's first 8 characters */, name, owner, created_at}`.
  They're sent as `Authorization: Bearer`, or as the git password.
- `GET /v1/code/owners/{owner}/tokens` → `[{id, name, created_at, last_used_at: number | null, prefix: string | null}]`,
  newest first, never the tokens (the owner's own token or admin; anyone else 404). `last_used_at`
  is updated at most once a minute. Tokens from before ids existed get `tok_<hash prefix>` and a
  null `prefix`.
- `DELETE /v1/code/owners/{owner}/tokens/{id}` → 204 (the owner or admin; 401 anonymous, 403
  another owner, 404 unknown here). A revoked token stops working at once: the API, git and MCP
  look the token up on every request, and its MCP sessions end with 401. (With no
  `ROLL_CODE_TOKEN` the daemon is open, and every caller is admin, revoked token or not.)
- **Agent tokens:** a token named `agent:<name>` is that agent.
  - It works code: reads, writes, changes, landing, work items, statuses, and reviews as
    `{kind: "agent", name}`.
  - It can't manage its owner. That means tokens (create, list, revoke), the owner profile,
    review apps, billing, usage, the review provider, webhooks, repo settings (rename,
    visibility, land policy, delete), review rules, reviewer settings, queue pause/resume and
    `approve-run`. Those answer 403, and the owner-only reads (tokens, billing, usage,
    provider) answer 404.
  - It can't review as anyone else: `X-Roll-Agent` with another name answers 403.
  - Give each agent its own `agent:` token. An unbound owner token is the owner and may name the
    agent it runs with `X-Roll-Agent`.
- Anonymous requests can read public repos.
- Review app tokens (`rla_…`) are always the app, even on an open daemon: they read the repos the
  app is installed on (private ones too) and review their changes, nothing else.
- When `auth.required` is true, the dashboard shows a sign-in button. It keeps the token in
  localStorage, sends it as `Bearer`, and opens the prompt on any 401. The event stream uses
  `fetch`, not `EventSource`, so it can send the header too.
