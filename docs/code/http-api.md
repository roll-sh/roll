# roll code — HTTP API shapes

This is the contract between `code-control` (Rust) and the dashboard's `lib/code-api.ts`. Routes
are listed in [architecture.md](architecture.md#http-api-v1code).

- Timestamps are Unix seconds.
- Ids are strings: changesets `cs_…` (full 64-hex ids are accepted too), git commits are 40-hex.
- Errors are `{error: string}` with a matching status: 400, 401, 403, 404, 409 or 422 (and 429 for
  too many event streams, 503 while a repo's search index is first built, or with `Retry-After`
  while the database can't answer). A bad query string is a 400 too, and so is text with a NUL
  character where the database is Postgres.
- Writes without a token answer 401 (the dashboard's sign-in prompt), with a token that may not
  do it 403; reads of what the caller can't see answer 404.

## Info and names

```
GET /v1/code → {storage, git_url /* e.g. "https://roll.sh" or "http://127.0.0.1:7070/git" */, repos,
                stats: {merges_today, merges_per_s, queue_depth}, github: {configured},
                auth: {required: bool /* ROLL_CODE_TOKEN or ROLL_ADMIN_TOKEN is set */,
                       operator: bool /* the token is rolld's operator token (ROLL_ADMIN_TOKEN): every product's API takes it */,
                       caller: {kind: "admin" | "owner" | "app" | "anonymous" | "invalid", owner: string | null,
                                app?: string /* kind app */,
                                reviewer: {kind: "user" | "agent" | "app", name} | null /* who it reviews and comments as,
                                           with the request's X-Roll-Agent / X-Roll-User; null when anonymous or invalid */}}}
                The caller is whoever the request's Bearer token is; the dashboard checks a token with it, and
                compares `reviewer` with a thread's or review's author to know which are the viewer's.

GET /v1/code/names/{owner}/{repo} → same shape, for a repo name within an owner
GET /v1/code/names/{name} → {name, available: bool, reason: null | "taken" | "reserved" | "invalid", message: string | null}

GET /v1/code/health → 200 | 503 {status: "ok" | "unavailable", database: bool /* Postgres or SQLite answered within 5 s */,
                cluster: null /* one node */ | {node, healthy: bool /* a member whose leases are within their local
                deadline */, draining: bool, nodes: number /* live nodes it sees */, repos: number /* it owns */}}
                No caller needed: for load balancers. 503 (with Retry-After: 1) when the database doesn't answer, or in
                a cluster when the node can't trust its leases or is draining (shutting down).
```

**In a cluster** (docs/code/cluster.md) any node answers every route. Requests for a repo run on
the repo's owner node, forwarded there by whichever node received them; this is invisible to
clients except for the following:
- A repo that is moving between nodes (a handoff, or a dead node's lease that hasn't expired yet)
  makes the receiving node wait and try again for about a lease's length; past that the request
  answers `503 {error}` with `Retry-After` and `x-roll-code-unavailable`. Nothing ran: retry it.
- A write already forwarded to the owner when that node dies answers `502` (`forwarding to node
  … failed`, plain text): it may or may not have run, and the receiving node never sends a write
  twice. Retry it in a way that is safe either way: a landing that did land conflicts on its own
  paths when sent again (409 naming them).
- An event stream ends when its repo starts moving to another node: reconnect (the dashboard and
  `roll code watch` do).
- `x-roll-code-*` request headers belong to the cluster: a request carrying one that isn't signed
  by a node of the cluster answers 401.

## Owners

```
Owner = {name, kind: "user" | "org", display_name, bio: string | null, url: string | null,
         created_at, repos_count}
GET  /v1/code/owners → Owner[]
POST /v1/code/owners {name, kind, display_name?} → Owner          admin only (401 without a token, 403 with another);
                                                                  409 when the name is taken, 422 when reserved or invalid
GET  /v1/code/owners/{owner} → Owner & {repos: Repo[]}            only public repos without a token
PATCH /v1/code/owners/{owner} {display_name?, bio?, url?} → Owner the owner's own token or admin (401 without a token, 403 others)
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
           postgres: {project_id, branch_id} | null, redis: {db_id} | null, error: string | null, created_at,
           environment?: {id /* pv_… */, slug, state /* roll previews' Preview.state */,
                          services: [{name, url, state}], resources: [{kind, parent, ref: string | null}],
                          sleeping_usd_month, active_usd_hour, dashboard_url: string | null,
                          github: {comment, status, deployment},
                          head?: string /* the changeset it's for: statuses are about it (after pushes that touched only ignored paths its services run an earlier one) */,
                          schema_changes?: string[] /* "+ …"/"- …": its Postgres schema against the parents' */,
                          screenshot_url?: string /* a public preview's screenshot, signed and expiring */,
                          diff_url?: string /* the same for where it differs from the default branch's, when it does */,
                          expires_at?: number /* ms: when it goes (its TTL, or a keep, running out), once a day away */,
                          earlier?: [{service, head, url}] /* earlier pushes still reachable on the same data, newest first */,
                          visual_change?: {since, percent, phone_percent?} /* how much the first service's page changed since the screenshot at an earlier head */,
                          page_speed?: {ttfb_ms, fcp_ms: number | null, load_ms, bytes, before_load_ms: number | null} /* how fast that page loaded, timed with its screenshot */,
                          page_health?: {errors, first_error: string | null, failed, first_failed: string | null} /* what went wrong while it loaded: the page's own words, untrusted */,
                          page_a11y?: {issues: [{kind: "images" | "fields" | "buttons" | "links", count, first: string | null}], lang: bool, title: bool} /* what a screen reader can't name there; first is the page's markup, untrusted */,
                          base_change?: {branch, percent, phone_percent: number | null, head?: string | null, load_ms?: number, a11y_added?: number, errors_added?: number} /* how much the page differs from the project's preview of the default branch */,
                          pages?: [{path, errors, percent: number | null, base_percent: number | null}] /* the project's other pages (github.pages): errors while each loaded, its change since the push before, against the default branch */,
                          speed_budget?: {load_ms, bytes} /* the project's github.budget (0: no limit), for roll/speed */,
                          visual_review?: {state: 'unchanged' | 'changed' | 'accepted', branch, percent, by: string | null, required, head?, pages?: {path /* '': the first page */, percent, by: string | null}[] /* each page that differs, accepted per page */} /* the roll/visual status (the project's visual_review; required: a visual blocker too) */} /* roll previews (docs/previews.md) */}
Change  = {id, repo_id, repo /* full name */, number /* per-repo, from 1 */, title, body, author,
           state: "open" | "queued" | "landed" | "conflict" | "closed",
           base, head, revision, work_item: string | null, branch: string | null /* git branch it was pushed as */,
           github_pr: number | null, github_url: string | null,
           trusted: bool /* false for a pull request from a fork: no automatic checks or preview */,
           run_approved: bool /* a writer approved running this untrusted change's current head */,
           draft: bool,
           labels: string[] /* a GitHub pull request's, or set with PATCH; roll previews reads "preview" and "no-preview" */,
           checks: Check[], preview: Preview | null,
           landed_as: string | null, conflict: {paths, head} | null,
           queue: QueueInfo | null /* its latest time in the merge queue */,
           blockers: Blocker[] /* why it can't be queued or land right now; [] when it can */,
           review: ReviewSummary /* see Reviews */,
           created_at, updated_at}
Blocker   = {kind: "state" | "draft" | "untrusted" | "checks" | "needs_approval" | "needs_codeowner" | "changes_requested"
                   | "unresolved_threads" | "review_pending" | "review_required"
                   | "visual" /* its preview's pages differ from main's and nobody accepted them (visual_review = "required") */ /* more may come: show unknown kinds by message */,
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
PATCH /v1/code/changes/{id} {title?, body?, draft?, labels?: string[] /* at most 20, 50 characters each */, work_item?} → Change    marking a queued change draft dequeues it; work_item (wi_…, of the change's repo) links it as if it had been opened with it: 409 if it does another item or is landed/closed
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
            rules: null | RulesPatch /* the rule keys the file sets; keys it doesn't set are left out:
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
`preview`, `queue`, `review`, `thread`, `push` and `budget`. Repo webhooks get the same events with the same data.
A `budget` event is `{budget: {project, project_name, percent /* 80, 100 */, spent_usd_month, max_usd_month},
dashboard_url}`: the repo's workspace project's previews passed that share of their monthly budget
(docs/previews.md).

**Stream limits.** Each caller may hold at most 32 event streams open at once
(`--code-max-streams-per-caller`, `ROLL_CODE_MAX_STREAMS_PER_CALLER`), and the server 10,000 in total
(`--code-max-streams`). Past either limit the request answers `429 {error}` with `Retry-After: 5`
instead of a stream; closing a stream frees its slot. MCP's event-stream answers count against the
same slots.
- A caller is a token (the admin token, an owner token by its name, a review app).
- Anonymous callers, and everyone on an open daemon, are counted by client address. Behind a proxy,
  `--code-trust-forwarded-for` counts them by the first `X-Forwarded-For` address instead; never set
  it without a proxy that sets that header. Anonymous callers whose address is unknown share one
  bucket bounded only by the total.
- A refused request (401, 404) takes no slot.

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
       events: any of landed, change, review, thread, check, queue, work, preview, push, budget, or ["*"] (default
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

## Agents

roll code agents (docs/code/agents.md): machines running `roll agent up` connect out to the relay,
which mirrors their records so the canvas loads a snapshot and streams what changes, and forwards
commands to them. The wire types are `agent_proto` (Rust) and `apps/dashboard/src/lib/agents-api.ts`;
times in these shapes are **milliseconds** (`Millis`), unlike the rest of this API.

```
POST /v1/code/agents/login {name?, hostname?, os? /* what the machine says about itself, one line, ≤ 64 */}
       → DeviceLogin {device_code /* rld_… */, user_code /* "BCDF-GHJK" */,
       verification_uri /* {web}/dashboard/agents/connect?code=… */, interval: 5, expires_in: 900}
       No caller needed. 429 (Retry-After) while 20 logins from the client's address (an IPv6 address's
       /64; `X-Forwarded-For` only with trust_forwarded_for, as for event streams) are waiting; when
       10,000 are waiting in all, the oldest pending login of the address with the most goes to make
       room (never a refusal for everyone). 400 a non-string field.
POST /v1/code/agents/login/poll {device_code} → LoginPoll
       {status: "pending"} | {status: "approved", token /* rlm_…, handed out once */, machine /* mch_… */, owner}
       | {status: "denied"} | {status: "expired"} /* also for an unknown code and after the token was handed out */
GET  /v1/code/agents/{owner}/login/{user_code} → PendingLogin {user_code, name, hostname?, os?, created_at, expires_at}
       which machine asks, shown before approving ("Approve ada-mbp (macOS)?")
POST /v1/code/agents/{owner}/login/approve {user_code /* any case, dash optional */, name?} → 204
POST /v1/code/agents/{owner}/login/deny {user_code} → 204
       All three need Code::manages (the owner or admin, not agent tokens); 404 when no pending login has the code.

GET    /v1/code/agents/{owner}/machines → [Machine]   oldest first; owner's tokens (agents too), else 404
POST   /v1/code/agents/{owner}/machines {name?} → 201 {machine: Machine, token /* rlm_…, shown once */}
       ("Add machine", sandboxes); 422 past 200 machines
PATCH  /v1/code/agents/{owner}/machines/{machine} {name?, color?, kind?} → Machine
       renames it or changes how it shows (Machine.appearance): at least one key, each one there
       changes that field; `color` is an IdentityColor (slate | violet | purple | pink | rose |
       teal | cyan | green), `kind` a MachineKind (laptop | desktop | server | cloud | container),
       `null` clears either (the dashboard then makes a colour from the id, a kind from the OS);
       none of the three, an empty name or an unknown colour or kind → 400
DELETE /v1/code/agents/{owner}/machines/{machine} → 204: the token stops working, the live link closes now
       (close code 4401), the machine's mirrored records are dropped, and streams get machine_removed.
       POST, PATCH and DELETE need Code::manages: 401 anonymous, 403 agent tokens and other owners.

Machine = {id, name, online: bool /* its link is up */, info?: MachineInfo /* from its last hello */,
           sandbox?: SandboxInfo, created_at, last_seen?}
           A name is trimmed, one line, 1–64 characters.

GET /v1/code/agents/{owner}/state?threads=thr_a,thr_b → State {cursor, machines: [Machine], entities: [Mirrored]}
       Mirrored = {cursor, machine, entity: Entity}. The shell (projects, threads, runs, requests,
       terminals) always; detail (nodes, checkpoints) only for the listed threads (at most 200).
       Tombstones aren't in the snapshot. Owner's tokens (agents too), else 404.
GET /v1/code/agents/{owner}/events?threads=…&after=<cursor> → SSE, events named by their `type`:
       event: entity           id: <cursor>   data: {type: "entity", cursor, machine, entity}
       event: machine          id: <cursor>   data: {type: "machine", cursor, machine: Machine}
       event: machine_removed  id: <cursor>   data: {type: "machine_removed", cursor, machine: mch_…}
       event: delta            (no id)        data: {type: "delta", machine, delta: Delta}
       event: canvas           id: <cursor>   data: {type: "canvas", cursor, revision, updated_at}
       event: settings         id: <cursor>   data: {type: "settings", cursor, revision, updated_at}
       Load /state, then open /events?after=<state.cursor>: a snapshot plus everything with a newer
       cursor is complete. A reconnect with Last-Event-ID (EventSource sends it) first gets what
       changed since, from the mirror (each record once, at its newest), then goes on live; cursors
       only ascend. Without either, only what happens from now on. `entity` events include tombstones
       ({kind: "removed", entity, id, thread?}). Text and output deltas only for the listed threads;
       terminal deltas always. Deltas are never replayed. Stream limits as for repo events (429).
       With both `?after=` and Last-Event-ID (the canvas sends both on a reconnect) the stream resumes
       after the newer; a Last-Event-ID that isn't a number is ignored, an `after` that isn't is a 400.

GET /v1/code/agents/{owner}/canvas → SavedCanvas {canvas: object | null, revision, updated_at: Millis | null}
       The canvas as the owner arranged it (the dashboard's document; opaque to roll), so every device
       shows the same. Before the first save: {canvas: null, revision: 0, updated_at: null}. Owner's
       tokens (agents too), else 404.
PUT /v1/code/agents/{owner}/canvas {canvas: object, revision?: int} → SavedCanvas
       Saves it (revision + 1). With `revision`, only over that revision (0: none saved yet), else 409
       SavedCanvas + {error}: what is there now, unchanged; without it the last writer wins. `canvas`
       is at most 64 KiB as compact JSON and the body is read no further than a little past that: 413
       (also for a Content-Length past it). 400 bad JSON, a body that isn't an object, a `canvas` that
       isn't an object, a `revision` that isn't a whole number. Code::manages: 401 anonymous, 403 agent
       tokens and other owners; machine tokens are 401. Each save goes out on /events as `canvas` with
       its cursor (the owner's, so the snapshot's cursor counts it), and a stream catching up gets the
       canvas's last save if it's newer: a device showing an older revision loads it again.

GET /v1/code/agents/{owner}/settings → SavedSettings {settings: object | null, revision, updated_at: Millis | null}
PUT /v1/code/agents/{owner}/settings {settings: object, revision?: int} → SavedSettings
       The owner's agents settings (docs/code/agents.md, "Owner settings"): a document of its own
       beside the canvas, exactly as …/canvas (revisions, 409 with what is there, the same readers and
       writers, 400/401/403/404), at most 32 KiB of compact JSON (413), announced on /events as
       `settings`. The dashboard's schema (lib/owner-settings.ts); roll keeps nothing secret there.

GET /v1/code/agents/{owner}/endpoints → [EndpointProfile]
PUT /v1/code/agents/{owner}/endpoints/{id} EndpointProfile → EndpointProfile
DELETE /v1/code/agents/{owner}/endpoints/{id} → 204
       The owner's endpoint profiles (docs/code/agents.md, "Endpoints"): {name, kind: anthropic |
       openai | cliproxy | custom, base_url, providers?, url_env?, key_env?, key_ref?, sandbox_default?,
       updated_at}, oldest first. Never a key: one sent with a profile is dropped, not stored. The
       path names it (`[a-z0-9_-]`, at most 64); 400 for a spec `EndpointSpec::check` refuses (a URL
       that isn't http(s) or holds credentials, a custom endpoint naming no variable, …) and past 32
       per owner. `sandbox_default` on one takes it off the others. GET: the owner's tokens, agents
       too (404 others); PUT and DELETE: Code::manages (401 anonymous, 403 agent tokens and other
       owners; 404 deleting one that isn't there). Applying a profile to a machine is `endpoint.set`
       through …/commands, with the key sealed to that machine in the browser.

GET /v1/code/agents/{owner}/gateway → GatewayPlan | null
PUT /v1/code/agents/{owner}/gateway GatewayPlan → GatewayPlan
DELETE /v1/code/agents/{owner}/gateway → 204
       The owner's model gateway (docs/code/gateway.md): {host: MachineId, urls?: [{label, url}],
       members?: [GatewayMember], new_sandboxes?, fallback?, acknowledged_at, updated_at}. No key, ever.
       PUT: the host is one of the owner's machines (404 otherwise), `acknowledged_at` is when the owner
       read the notice (0: 400), URLs are http(s) without credentials (400); members sent with it are
       ignored (only joining and leaving change them), and another host while machines use the gateway is
       a 409 ("remove the machines first"). DELETE is a 409 while machines use it. The host's gateway
       itself is set up and removed with `gateway.*` commands. With none set up, GET answers `null`
       (200: no gateway is a state, not an error), and DELETE and the member routes are 404s.
POST /v1/code/agents/{owner}/gateway/members GatewayJoin {machine, url?, providers?} → GatewayPlan
       Joins a machine: the relay sends `gateway.client.issue {name, machine, seal_key}` to the host
       (the member's seal key from its last `hello`), then `endpoint.set` to the member with
       {id: "roll-gateway", name: "roll gateway on <host>", kind: gateway, base_url: url, providers,
       fallback: the plan's} and the host's sealed box as `key` unchanged, and records the member's
       client id. `url` is one of the plan's (the first when unset); the host may join itself on a
       loopback URL. `providers` unset: every provider the member reports. 400 a URL that isn't the
       plan's; 404 no plan, or not one of the owner's live machines; 409 a member that never said a
       seal key or has no providers, or a host that's gone. A failure of either daemon is recorded on
       the member (`error`) and answered with its status (400 invalid, 409 not found or conflict, 503
       offline or no answer, 422 otherwise); a member that refused the endpoint has its new key revoked
       again. A sandbox joins on its own on its first `hello` while `new_sandboxes` is set.
DELETE /v1/code/agents/{owner}/gateway/members/{machine} → GatewayPlan
       `endpoint.remove {endpoint: "roll-gateway"}` to the member (best effort: it may be gone or
       offline) and `gateway.client.revoke` to the host, then the plan without it. A host that doesn't
       revoke keeps the member, with `error`, and answers its status. 404 not a member.
       Every gateway route is Code::manages: 401 anonymous, 403 agent tokens, review apps and other
       owners; machine tokens are 401.

POST /v1/code/agents/{owner}/machines/{machine}/commands CommandRequest {id, command: Command} → Reply
       Forwarded to the machine's daemon; waits up to 30 s for its reply. `id` (1–128 characters) is
       the idempotency key, per sender: retrying with the same id never runs the command twice (the
       daemon keeps receipts by sender and id), a retry while the first waits shares its reply (another
       sender's command with that id waiting is a 409 conflict), and a link that drops and comes back
       within the wait gets the command again. Owner's tokens, agent tokens too (401 anonymous, 403
       others; 404 an unknown or revoked machine). The daemon is told who sent it: the owner's name,
       `agent:<name>`, or `admin`.
       `endpoint.set {endpoint, key?}` (owner only): 400 before it reaches the machine for a spec
       `EndpointSpec::check` refuses or a `key.sealed` that isn't a roll-seal-v1 box (the relay can't
       open one; it forwards it and keeps nothing). `endpoint.check` answers `endpoint_checked`.
       `gateway.*` (owner only; docs/code/gateway.md): 400 before it reaches the machine for
       `gateway.configure` settings `GatewaySettings::check` refuses (an unspecified listen address,
       an unknown upstream in a route, a key header), a `gateway.key` `key.sealed` that isn't a
       roll-seal-v1 box, or a `gateway.client.issue` `seal_key` that isn't 32 bytes of base64url.
       Replies: `gateway.client.issue` → `gateway_client {client, sealed}` (the key sealed to
       `seal_key` for the `roll-gateway` endpoint), `gateway.test` → `gateway_tested {checks}`,
       `gateway.status` → `gateway_status`, `gateway.usage` → `gateway_usage`, `gateway.log` →
       `gateway_log {entries}`, `gateway.log.body` → `gateway_log_body`; the rest `ok`. A machine
       that hosts no gateway answers 404 for everything but `gateway.configure` and `gateway.remove`.
       Agent tokens drive threads only (403 otherwise, docs/code/agents.md "Machines, login and
       tokens"): `thread.create` in `ask` or `auto_edit` (not `full_access`, the default),
       `thread.configure` only lowering the thread's modes, `thread.fork`, `thread.archive`,
       `thread.unarchive`, `message.send`, `message.unqueue`, `message.edit`, `message.reorder`,
       `message.promote`, `run.interrupt`, `thread.publish`,
       `diff.get`, `git.status`, and `fs.list`/`fs.read` in a project. The rest (`terminal.*`,
       `fs.write`, `fs.list` without a project, `request.respond`, `project.add`, `project.remove`,
       `project.update`, `project.pull`, `thread.delete`, `checkpoint.rollback`, `provider.refresh`,
       `machine.configure`, `endpoint.*`, `gateway.*`) needs Code::manages.
       `machine.configure {settings: MachineSettings}` replaces the machine's settings whole
       (docs/code/agents.md, "Machine settings"); the reply comes after the daemon's `hello` with them
       (Machine.info.settings). Settings that would carry a credential or an address (a variable named
       like a key, token, URL, endpoint, proxy or host), or break MachineSettings::check, are a 400
       here and never reach the machine.
       200 with the Reply, or Reply.Error {type: "error", code, message, error /* = message */} with
       not_found 404, invalid 400, conflict 409, provider 422, internal 502,
       machine_offline 503 (not connected), timeout 504 (no reply in time).
       A sandbox machine that is asleep, stopped by its owner, or on its way down is woken by any
       command (also `project.add` through …/projects and the MCP tools, which call this route): the
       sandbox is set running (Machine.sandbox.status "starting" on the stream) and the request is held
       until its daemon's link is up, at most ROLL_SANDBOX_WAKE_TIMEOUT (180 s), then the command goes
       out once and waits for its reply as usual (so allow for 180 + 30 s; a proxy in front of the API
       must too). A wake that runs out answers machine_offline "the sandbox is still waking up …; send
       the command again with the same id": the sandbox keeps waking, and the same id is safe to send
       again. A sandbox being deleted (or failed) never wakes: machine_offline at once.
       Commands on a roll code repo (docs/code/agents.md, "Agents in roll code"): `project.add` with
       `clone: {owner, repo}` and `thread.publish` in a project that is a roll code repo carry a grant
       for that repo to the daemon. The repo must be the machine's owner's (400 otherwise, 404 when it
       doesn't exist or the caller can't read it); publishing needs write access to it (403). A
       clone's grant only fetches; a publish's pushes only the thread's worktree branch (400 when that
       is the repo's default branch; a thread without a worktree gets none). `project.pull`, and
       `project.update` with `settings.auto_pull` (it pulls at once), in a roll code project carry a
       fetch-only grant too (docs/code/agents.md, "Project settings"); `project.pull` answers
       {type: "pulled", at, outcome: {status: "pulled", branch, from, to, commits} | {status:
       "up_to_date", branch} | {status: "skipped", reason, message} | {status: "failed", message}}.
       `thread.publish` answers Published {branch, commit, change: number | null /* the branch's open
       change */, url: string | null, landing?: Landing /* with land: true */};
       Landing = {status: "landed" | "queued" | "blocked" | "conflict" | "failed", message, blockers: string[]}:
       the land goes through POST /v1/code/changes/{id}/land with the caller's token (the merge queue on a
       queue repo, the landing rules otherwise), and a blocked or conflicting land is still a 200.

POST /v1/code/agents/{owner}/machines/{machine}/projects {id /* idempotency key */, repo: {name, description?,
       visibility?, default_branch?}, path?, name?} → Reply (200, as for commands)
       Creates the repo (POST /v1/code/repos on the caller's behalf: its 403/422 pass through; a 409, the
       repo already existing, is a retry and goes on), lands a README as the first commit if the repo has
       no history, and clones it onto the machine as a project
       (`project.add` with `clone`). Code::manages (401 anonymous, 403 agent tokens and others,
       404 an unknown machine, 400 without id or repo.name).
POST   /v1/code/agents/{owner}/threads/{thread}/work {work?: "wi_…" | "owner/repo#N", repo?: "owner/name",
       labels?, lease_s?, agent?} → WorkItem | 204
       The thread takes a work item: `work` (read as the caller: 404 if it can't), or the next open one
       claimed from `repo` (POST …/work/claim as `agent`, default the caller's agent or
       `agent:<machine name>`; 204 when none matches). It must be one of the owner's repos (400), and
       becomes the thread's `work`. 400 with neither; 404 an unknown thread. A claim the item has when
       the thread takes it is kept alive (renewed by its lease length once half is left) while the
       thread works: running, or idle under 10 minutes, not archived, its machine connected. Then
       it runs out like any other. A `thread.publish` links the branch's change to the item
       (PATCH /v1/code/changes/{id} {work_item} as the sender), so landing it completes the item.
DELETE /v1/code/agents/{owner}/threads/{thread}/work → 204: the thread lets go of it (the claim stays,
       no longer kept alive).
       Both: owner's tokens, agent tokens too (401 anonymous, 403 others).

Thread.change = {repo /* owner/name */, number, status: "checking" | "failing" | "blocked" | "ready" | "queued"
       | "landed" | "conflict" | "closed", url, detail: string | null} and Thread.work = {id, repo, number,
       title} are filled by the relay from roll code (the daemon never sets them) and follow the change
       as it moves, as new `entity` events on the stream.

Grants (`rlg_…`) only open git, for their one repo: fetch, and push for a publish. Every API route
answers 401 to one, even on an open daemon. They expire after 10 minutes and stop at once when the
machine is revoked.

GET /v1/code/agents/connect    the daemon's WebSocket (Authorization: Bearer rlm_…; only machine tokens,
       looked up on every connect: 401 otherwise). Without an upgrade: 200 MachineLogin {machine: Machine,
       owner}, the machine the token opens (`roll agent login --machine-token`). Text frames of
       DaemonFrame / RelayFrame JSON: hello → welcome {machine, owner, resume_from}, entities → ack,
       delta, command → reply, ping/pong (every 20 s; 60 s of silence closes). Frames up to 16 MiB.
       Close codes: 4400 protocol error (wrong version, no hello), 4401 revoked (stop, don't retry),
       4409 replaced by a newer link of the same machine, 1012 reconnect (the owner moved nodes).
DELETE /v1/code/agents/connect → 204   a machine revokes itself with its own token (`roll agent logout`):
       the same as DELETE …/machines/{machine}; 401 for any other token.
```

**Shares** are read-only copies of a thread behind a public link, `{web}/agents/s/<id>`
(docs/code/agents.md, "Shares").

```
POST   /v1/code/agents/{owner}/threads/{thread}/shares {subtree?: bool} → 201 ShareSummary
       Freezes the thread's records from the mirror (with `subtree`, the subagent threads it delegated to,
       recursively, at most 64 threads; forks are separate threads and stay out). Terminals, attachments'
       bytes and queued messages are left out; the machine's home directory and any /home/<user>,
       /Users/<user> or /root starting a path become `~` in every string. Over 4 MiB, tool outputs and
       diffs are cut to their ends (`trimmed: true`); still over is a 422, as is a 501st share.
       Code::manages; 404 when the mirror has no such thread.
GET    /v1/code/agents/{owner}/shares → [ShareSummary]   newest first; owner's tokens (agents too), else 404
DELETE /v1/code/agents/{owner}/shares/{id} → 204          Code::manages; the link stops working at once
GET    /v1/code/agents/shares/{id} → Share               no caller needed; `Cache-Control: no-store`; 404 unknown

ShareSummary = {id /* shr_ + 24 hex */, title, thread, subtree, url /* {web}/agents/s/<id> */, threads, runs,
                bytes, created_at, created_by}
Share = {id, owner, title, thread, subtree, machine: {name, os?}, created_at, trimmed,
         entities: [Entity] /* project, threads, then runs, requests, checkpoints and nodes as they happened */}
```

A share is a copy: later work on the thread, revoking its machine or deleting the thread leave it as
it was. `shares` (like `login`) is a reserved owner name, so `/v1/code/agents/shares/…` never names an
owner, and it isn't routed by owner in a cluster.

In a cluster every `/v1/code/agents/{owner}/…` request and every machine link of the owner run on the
node holding the lease `agents:<owner id>` (docs/code/cluster.md).

### Sandboxes

Machines roll hosts for an owner (docs/code/agents.md, "Sandboxes"): each is a container running
`roll agent up` with a machine of its own, so it also appears in `/machines`, `/state` and `/events`
with `Machine.sandbox` set. Every route needs Code::manages (401 anonymous, 403 agent tokens and other
owners). Requests record what the owner wants; the owner's reconciler makes the backend match (at
once for start, stop and delete; creates in the background) and every change of what the canvas
shows is a `machine` event. Times are milliseconds.

```
GET    /v1/code/agents/{owner}/sandboxes → {sandboxes: [Sandbox] /* oldest first */, sizes: [Size] /* the first is the default */,
       max /* per owner */, backend: "agent37" | "docker" | null /* null: sandboxes are off */}
POST   /v1/code/agents/{owner}/sandboxes {name?, size? /* a Size name */, env?: {NAME: "value"}}
       → 201 {sandbox: Sandbox /* status "creating" */, machine: Machine}
       env is passed to the sandbox only (e.g. ANTHROPIC_API_KEY, OPENAI_API_KEY): sealed with ROLL_SECRET_KEY
       until the backend has it, then dropped; never returned. At most 32 names (uppercase, digits, _),
       values one line of at most 4096 characters, 32 KiB in all; ROLL_*, AGENT37_*, HOME, USER, PATH,
       SHELL and IS_SANDBOX are reserved. 400 bad name, size or env; 422 at the owner's limit (deleting
       ones count until gone) or without ROLL_SECRET_KEY on the server; 503 sandboxes are off.
POST   /v1/code/agents/{owner}/sandboxes/{id}/start → Sandbox   desired "running" (asleep ones too: a wake now);
       409 while it's being deleted
POST   /v1/code/agents/{owner}/sandboxes/{id}/stop  → Sandbox   desired "stopped" (running time stops being billed;
       a command to it starts it again)
DELETE /v1/code/agents/{owner}/sandboxes/{id} → 204 the backend deleted it and its data, and its machine is
       revoked (machine_removed); 202 Sandbox ("deleting") while the backend is still at it (retried).
       Removing a sandbox's machine (DELETE …/machines/{machine}, or `roll agent logout` in it) deletes the
       sandbox the same way. 404 for an unknown or deleted sandbox.
GET    /v1/code/agents/{owner}/sandboxes/usage?month=YYYY-MM (default: this UTC month)
       → {owner, month, sandboxes: [{sandbox, name, size, hourly_cents, running_ms, running_hours, price_usd}],
          totals: {running_ms, running_hours, price_usd}}
       Running time as the reconciler saw it (it looks every 30 s), deleted sandboxes included; price_usd is
       hours × hourly_cents / 100, rounded to a hundredth of a cent (0.0001). 400 for a malformed month.

Sandbox = {id /* sbx_… */, name, machine /* mch_… */, backend, size: Size,
           status: "creating" | "starting" | "running" | "stopped" | "failed" | "deleting" /* = Machine.sandbox.status */,
           desired: "running" | "stopped" | "asleep" | "deleted" /* asleep: the reconciler stopped it while idle */,
           stopped_reason?: "idle" | "owner" /* while stopped: asleep, or the owner's stop */,
           error?: string /* the backend's last complaint */,
           created_at, updated_at, last_active_at? /* its machine last did something; it sleeps a window after */}
SandboxInfo = {backend, size /* a Size name */, status /* as Sandbox.status */, hourly_cents,
               stopped_reason? /* as Sandbox.stopped_reason */}   (Machine.sandbox)
Size = {name /* "standard" */, cpu, memory_gb, disk_gb, hourly_cents /* price per running hour, US cents */}
```

A `failed` sandbox stays until it's deleted (a refused create isn't tried again). One that stopped on
its own while it should run is started again. A running one goes to sleep (desired "asleep", status
"stopped", stopped_reason "idle") once its machine has done nothing for ROLL_SANDBOX_IDLE_STOP (5 min;
`0` keeps sandboxes up): no thread on it starting or running (a thread waiting on its user is idle),
no command for it and no terminal output. "starting" is a stopped sandbox on its way up again (a wake,
or start); its machine comes online once its daemon connects. Asleep, it isn't billed.

### Automations

Triggers, filters and steps that run the owner's agents and talk to the world (docs/code/automations.md).
The wire types are `agent_proto::automation` (Rust) and `apps/dashboard/src/lib/automations-api.ts`;
times are milliseconds. Reading (lists, runs, the catalog, dry runs) is the owner's tokens', agents
included (404 to everyone else); changing automations, their secrets, cancelling and retrying runs is
Code::manages (401 anonymous, 403 agent tokens); running one by hand is Code::manages, or an agent token
when the automation has `allow_agents`. Every change of an automation or a run is an `automation`,
`automation_removed` or `automation_run` event on the owner's stream (`GET …/events`, with a cursor, so a
reconnect catches up), and a `notify` step's notice is a `notice` event (no cursor), kept for later too.

```
GET    /v1/code/agents/{owner}/automations → [Automation] /* oldest first, deleted ones left out */
POST   /v1/code/agents/{owner}/automations AutomationSpec → 201 AutomationSaved
       400 a bad spec (the message says where: "steps.2.config.prompt: unclosed {{"), an unknown trigger
       source or step kind, a config its kind refuses, secrets used outside a webhook's header values;
       422 at 200 automations, or a webhook trigger without ROLL_SECRET_KEY on the server.
       A webhook trigger gets its URL and a secret (AutomationSaved.webhook_secret, `rlh_…`, shown once).
GET    /v1/code/agents/{owner}/automations/catalog → Catalog /* every trigger source and step kind, with
       JSON Schemas; integrations' too (needs_setup: not connected yet). Each field of a config's
       properties has "x-order": n, the order to show it in (JSON objects arrive sorted) */
POST   /v1/code/agents/{owner}/automations/test TestRequest → TestResult
       what the automation would do with the event: the trigger's and each filter's verdict, each step's
       config with its templates filled in. Runs nothing; secrets are never filled in.
GET    /v1/code/agents/{owner}/automations/runs?automation=&status=&before=&limit= → [AutomationRun]
       newest first, 50 by default, at most 200; `before` a run id (the next page)
GET    /v1/code/agents/{owner}/automations/runs/{run} → RunDetail {run, event}
GET    /v1/code/agents/{owner}/automations/notices?before=&limit= → [Notice]
       the notify steps' notices, newest first (the newest 200 per owner are kept), 50 by default, at most
       200; `before` a notice's `at` (the next page). For a dashboard that wasn't open when they came.
POST   /v1/code/agents/{owner}/automations/runs/{run}/cancel → AutomationRun /* "cancelled"; what its steps
       made only for the run (a sandbox) is cleaned up; a final run is answered as it is */
POST   /v1/code/agents/{owner}/automations/runs/{run}/retry → AutomationRun /* a failed or cancelled run goes on
       from the step that stopped it (its attempts start over); 409 for a run that isn't. The run gets
       `retried_at` (its time counts from there); each step that goes again keeps its error and a
       "Retried after the run failed: …" line in its log */
GET    /v1/code/agents/{owner}/automations/{id} → Automation
PUT    /v1/code/agents/{owner}/automations/{id} AutomationSpec & {revision?} → AutomationSaved
       replaces the definition; with `revision`, 409 {error, current: Automation} when it isn't the current one.
       Runs going on keep the definition they started with.
DELETE /v1/code/agents/{owner}/automations/{id} → 204 /* its runs going on are cancelled; its secrets dropped */
POST   /v1/code/agents/{owner}/automations/{id}/enable  → Automation
POST   /v1/code/agents/{owner}/automations/{id}/disable → Automation /* new events start nothing; runs going on go on */
POST   /v1/code/agents/{owner}/automations/{id}/run RunNow {input?, data?, key?} → 202 AutomationRun
       an event of source `manual`, kind `run`, data {…data, by, input}; the same `key` is the same run (200 then).
       Any trigger can be run by hand (its filters still apply: a run they refuse is "skipped"). `data` is
       the event's data as the automation's trigger sends it (an object, at most 256 KiB; 400 otherwise),
       so a code or integration automation's steps read `event.data.title` and the like; `by` and `input`
       are roll's, whatever `data` says. The dashboard starts it from the trigger's sample.
GET    /v1/code/agents/{owner}/automations/{id}/runs?status=&before=&limit= → [AutomationRun]
PUT    /v1/code/agents/{owner}/automations/{id}/secrets/{name} {value} → 204
       name: [A-Z0-9_], 1 to 64; value at most 8 KiB, sealed with ROLL_SECRET_KEY (422 without it) and never
       returned or logged; at most 20 per automation. Automation.secrets lists the names.
DELETE /v1/code/agents/{owner}/automations/{id}/secrets/{name} → 204
POST   /v1/code/agents/{owner}/automations/{id}/webhook/secret → AutomationSaved /* a new inbound secret, shown
       once; the old one stops working */
POST   /v1/code/agents/{owner}/automations/{id}/webhook  (no token: the signature is the credential)
       → 202 {event: "aev_…", duplicate: bool}
       Either `X-Roll-Signature-256: sha256=<hex HMAC-SHA256(secret, "{timestamp}.{body}")>` with
       `X-Roll-Timestamp: <unix seconds>` within 5 minutes (roll's own webhook signature), idempotent by
       `X-Roll-Delivery` (else the signature); or GitHub's `X-Hub-Signature-256` over the body with
       `X-GitHub-Delivery` as the key. The body (JSON, else text; at most 256 KiB) is `event.data.body`,
       the headers `event.data.headers` (signatures left out). 401 a bad or old signature, 404 no such
       automation or not a webhook trigger, 409 disabled, 413 too big.
```

### Linear

The owner's Linear workspace (docs/code/automations.md, "Linear"): connected by installing roll's
OAuth app as an agent (`actor=app`; needs `ROLL_LINEAR_CLIENT_ID`/`ROLL_LINEAR_CLIENT_SECRET` on the
server) or with a personal API key, and `ROLL_SECRET_KEY` either way (tokens are sealed, never
returned). The owner routes are Code::manages (401 anonymous, 403 agent tokens and other owners, 404 no
such owner). Times are milliseconds.

```
GET    /v1/code/agents/{owner}/linear → LinearStatus
       {connected, app: {oauth, webhook, webhook_url, redirect_uri, secret_key},
        connection: null | {kind: "oauth"|"api_key", workspace: {id, name, url_key, url}, user: {id, name},
          agent /* roll can be assigned and mentioned */, scopes, status: "ok"|"reauth", error, expires_at,
          created_at, created_by, settings: {repos: {TEAM: repo}, default_repo?},
          webhook: {mode: "app"|"workspace"|"manual", url, status: "ok"|"waiting"|"failing"|"needs_secret",
                    secret_hint, last_delivery_at, rejected_at, rejected_why}},
        deliveries: [{id, at, delivery, type, action, summary, result: "recorded"|"duplicate"|"ignored", events}]
          /* newest first, 20 (50 are kept) */}
DELETE /v1/code/agents/{owner}/linear → 204 /* revokes roll's OAuth token, or removes the webhook roll made
       for an API key (best effort), and forgets the connection; 404 not connected */
POST   /v1/code/agents/{owner}/linear/oauth {return_to?} → {url}
       Linear's authorize URL (scopes read,write,issues:create,comments:create,app:assignable,app:mentionable,
       actor=app, PKCE S256, a state good for 15 minutes and once). `return_to`: a dashboard path (default
       /dashboard). 400 not a path; 422 no OAuth app on the server, or no ROLL_SECRET_KEY.
GET    /v1/code/linear/oauth/callback?code&state[&error]  (no token: the state is the credential)
       → 303 to {web_url}{return_to}?linear=connected, or ?linear=error&reason=… (a used or expired state,
       Linear refused, the workspace is another owner's). Installs the "Linear agent" automation once.
PUT    /v1/code/agents/{owner}/linear/key {key} → LinearStatus
       Checks the key with Linear, then makes a webhook for Issues and Comments with a fresh signing secret
       (webhook.mode "workspace"); a key that may not (not an admin's) leaves mode "manual": make one in
       Linear to webhook.url and send its signing secret to …/webhook/secret. 400 a malformed key or one
       Linear refuses; 409 the workspace is another owner's; 422 no ROLL_SECRET_KEY.
PUT    /v1/code/agents/{owner}/linear/settings {repos?: {TEAM: "repo" | "owner/repo"}, default_repo?} → LinearStatus
       Which roll code repo a team's issues are in (events carry it as event.data.repo); replaces the
       settings whole (what's left out is cleared). 400 bad names; 404 not connected.
PUT    /v1/code/agents/{owner}/linear/webhook/secret {secret} → LinearStatus
       A hand-made webhook's signing secret (sealed; only its last four characters show). 400 malformed;
       404 not connected; 409 an OAuth connection (the app's secret is the server's).
GET    /v1/code/agents/{owner}/linear/teams → {teams: [{id, key, name}]} /* from Linear; 404 not connected,
       422 the connection needs connecting again, 503 Linear is unreachable or rate limiting */
POST   /v1/code/linear/webhook          (no token: the signature is the credential) roll's app's deliveries
POST   /v1/code/linear/webhook/{hook}   a connection's own webhook (API keys)
       → 200 {recorded, duplicates, ignored} — `Linear-Signature` is hex HMAC-SHA256 of the body under the
       signing secret; the body's webhookTimestamp must be within 60 s. 401 a bad or old signature (noted
       on the connection: webhook.status "failing"); 404 no such hook or no app webhook secret on the
       server; 400 not JSON or over 1 MiB. A workspace no owner has is 200 with `ignored`.
```

### Lookups

What a picker shows for an integration's things, so nobody types an id
(`crates/code-control/src/agents/lookup/`, `apps/dashboard/src/lib/lookup-api.ts`). The server
calls the integration with the owner's stored token; only ids, names, colours and https pictures
come back. `Code::manages` (the owner or an admin; agents and review apps get 403), routed like the
rest of `/v1/code/agents/{owner}`.

```
GET    /v1/code/agents/{owner}/lookup/{source}/{kind}?q=&parent=&cursor=&ids=&limit=
       → {items: [Item], next?: cursor, problem?: Problem}
       Item {id, label, key?, hint?, group?, avatar? (https), color? (#rrggbb), icon?}
         id is what a field stores; key another value naming the same thing (a team's key, a
         label's name), so values saved before pickers still show their row. icon names a kind
         (user, agent, channel, private, group, team, role, a Linear state's type).
       Problem {code: "not_connected" | "reconnect", message} | {code: "rate_limited", retry_after}
             | {code: "unavailable", message}  — a 200: the picker says it and takes a typed value.
       q ≤ 100 chars (filtered on the platform's side, or here for a whole list); parent: the
       rows' owner (a team for its states, a Slack workspace or Discord server); ids: up to 50
       stored values to resolve instead of listing; limit 1–100 (50).
       Cached per owner for 30 s; a rate limit holds the source off for the owner until its retry
       time (the cached page is still served with the problem). 400 a bad query or a kind that
       needs `parent`; 404 an unknown source or kind.
         code:   repos (the owner's; id = name, key = owner/name), agents (its agent tokens' names;
                 key = agent:<name>). Read from control.db, never cached.
         linear: teams, states (parent: team id or key; none: every team's, grouped), users,
                 labels (parent: the team's and the workspace's; groups aren't rows), projects,
                 cycles (parent required; not past), issues (q: a title, or an identifier ENG-12;
                 ids: ids or identifiers)
         slack:  through the install's sealed bot token. parent: a workspace's team id or install
                 id (one the owner installed, else 404); none: the owner's only workspace, or every
                 workspace's rows grouped by its name. Slack can't search, so a workspace's whole
                 list (≤ 2000 rows, 200 a call with Slack's cursor) is read once per 30 s and
                 searched, paged (cursor: an offset) and resolved here. A row's key is its name or
                 handle (`#general`, `@ada` resolve).
                 workspaces (the owner's installs: id the team id, key the install id),
                 channels (public, and private ones the bot is in; not archived; hint the member
                 count or "not a member"; icon channel/private), users (hint @handle, avatar
                 image_48; bots and deactivated people only resolve stored ids), usergroups (hint
                 @handle), targets (channels then people, grouped "Channels"/"People": a field
                 that takes either). A grant without channels:read, groups:read, users:read or
                 usergroups:read (installed before roll asked for them) is `reconnect`.
         discord: as roll's bot, only in servers the owner installed it in. parent: a guild id or
                 install id (else 404); none: the owner's only server, or every server's rows
                 grouped by its name. guilds (the owner's installs), channels (text, announcement,
                 forum; group: the category; read whole and searched here), roles (colour from
                 the role's; @everyone isn't one; a bot's own role only resolves), members (q: a
                 username or nickname prefix, searched at Discord; none: by user id, cursor the
                 last id; ids looked up one by one; bots left out; avatar from Discord's CDN;
                 needs the Server Members Intent, else `unavailable`).
         github: only the owner's GitHub-connected repos, with their installation's token (an
                 installation spans tenants). parent: a repo as the GitHub steps take it (app,
                 acme/app) or its GitHub name; another owner's or an unknown repo is a 404; none:
                 the first page of each connected repo (up to 5), grouped by repo. The cursor is
                 GitHub's page number; q and ids read up to 5 pages of 100 and keep what matches.
                 repos (roll's name, GitHub's as hint; no GitHub call), branches, labels (#colour,
                 description as hint), users (assignees), reviewers (People: collaborators, then
                 Teams), teams (org/slug; a user's repo has none), milestones (id: number),
                 workflows (id: the file name, key: GitHub's id), environments, issues (open
                 issues and pull requests; q or ids "#12" look the number up). No App or no
                 connected repo: not_connected; 401/403: reconnect, naming the permission;
                 403/429 with x-ratelimit-remaining 0 or retry-after: rate_limited (never waited out).
POST   /v1/code/agents/{owner}/lookup/{source}/{kind} {name, parent?, color?} → Item
       Make one where roll can (linear/labels: a team's when parent names one, else the workspace's;
       github/labels: on parent, or the owner's only connected repo; 409 GitHub has it already).
       The owner's cached pages of the source are dropped (connecting, updating or removing a
       Slack or Discord install drops that platform's too). 400 no name (≤ 80 chars); 422 not
       connected or a kind roll doesn't make (nothing of Slack's or Discord's).
```

## Chat ops

Slack, Discord and GitHub for the agents (docs/code/chatops.md). The owner's integrations are under
`/v1/code/agents/{owner}`, routed like the rest of it; the owner manages them and its agent tokens
get 403.

```
GET    /v1/code/agents/{owner}/integrations → {
         slack:   {configured, installs: [Install]},
         discord: {configured, installs: [Install]},
         github:  {configured, app_slug, install_url, repos: [{repo, github, url, connected, mirror}]},
         sealing: bool}                                 # ROLL_SECRET_KEY set (Slack needs it)
       Install = {id, team_id, team_name, bot_user_id, installer, members: [user id], anyone, created_at}
POST   /v1/code/agents/{owner}/integrations/{slack|discord}/install → {url}
       The platform's install URL with a one-time state (10 minutes). 422 when the platform (or, for
       Slack, ROLL_SECRET_KEY) isn't configured; 404 an unknown platform.
PATCH  /v1/code/agents/{owner}/integrations/{platform}/{id}  {members?: [user id], anyone?} → Install
       Who may drive roll from it besides the installer. 400 more than 200 ids or one that isn't
       letters and digits.
DELETE /v1/code/agents/{owner}/integrations/{platform}/{id} → 204
```

The platforms' endpoints, which any node answers (`cluster::classify`: global). Each one is 404 while
its platform isn't configured, and 401 on a missing, wrong or stale signature (more than five
minutes off).

```
GET  /v1/code/chat/slack/oauth?code&state      → 303 to {web}/dashboard/agents?owner=…&settings=integrations&integration=slack&connected=<team>
                                                  (or &error=expired | <why>, e.g. the workspace belongs to another owner)
GET  /v1/code/chat/discord/oauth?code&state     → the same for Discord (the guild from the token exchange, not the query)
POST /v1/code/chat/slack/events                 Events API: {challenge} for url_verification; else 200, recorded once per event_id
POST /v1/code/chat/slack/commands               /roll: 200 {response_type: "ephemeral", text}
POST /v1/code/chat/slack/interactions           buttons, selects, modals: 200 (empty)
POST /v1/code/chat/discord/interactions         200 with the interaction's answer: {type: 1} a ping, 4 an ephemeral
                                                line, 6 a deferred update, 9 a modal
```

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
                        answers JSON; notifications alone still 202); 429 when the caller already holds
                        its event streams (see "Stream limits" under Events)
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

The browser calls `/v1/code/*` directly: reads, SSE and raw blob links. rolld answers CORS for
every API at once: the dashboard's origins (`ROLL_DASHBOARD_ORIGIN`, by default any localhost
port on loopback), any requested header (`Authorization`, `Content-Type`), every response header
exposed (`x-roll-changeset`).

## Auth

- The local daemon is open, like the rest of the local API, unless `ROLL_CODE_TOKEN` or
  `ROLL_ADMIN_TOKEN` (rolld's operator token) is set. Both are admin tokens and work side by side;
  `GET /v1/code` says `auth.operator` for the operator's.
- Owner tokens are created with `POST /v1/code/owners/{owner}/tokens {name}` →
  `{token /* rlc_…, shown once */, id /* tok_… */, prefix /* the token's first 8 characters */, name, owner, created_at}`
  (the owner or admin; 401 anonymous, 403 another owner).
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
- Agent machine grants (`rlg_…`, minted by the relay inside a command; see Agents) are always
  scoped too: git for one repo, as `agent:<machine name>`, pushing at most one ref (the thread's
  branch: never the default branch, a tag or a delete), never the API (401).
- When `auth.required` is true, the dashboard shows a sign-in button. It keeps the token in
  localStorage, sends it as `Bearer`, and opens the prompt on any 401. The event stream uses
  `fetch`, not `EventSource`, so it can send the header too.
