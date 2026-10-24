# roll code — architecture

**Keep the code rolling.**

roll code is source control built for agents. A repository either lives natively on roll
(`git clone https://roll.sh/<owner>/<repo>.git`, viewable at `https://roll.sh/<owner>/<repo>`)
or is connected to a GitHub repository. Either
way, agents work through roll: they claim work items, read and write code through an API (no
checkout needed), land changes at tens of thousands of merges per day, and get a serverless
preview with branched Postgres and Redis for every change. Everything is Rust.

The design follows Anthropic's Scion ("Redesigning source control", 2026-10), which is built on
the ideas of Piper and Mononoke/Bonsai, and adapts it to a multi-tenant serverless product:

- **Clients are online.** The server owns history, merging and rebasing. Clients are thin.
- **Source control as a service.** Every client operation is also an HTTP call, so most agents
  never materialize a repository.
- **Landing costs O(size of the change), never O(size of the repo).** There are no packfiles,
  no repo maintenance and no fully materialized repo on the server.
- **Git is an interop format, not the storage format.** `git clone/fetch/push` and the GitHub
  mirror are served from data that is derived from changesets.

## Shape

```
  agents (HTTP API, MCP)   git clients (smart HTTP)    GitHub (App: webhooks, mirror push)
            │                        │                               │
            └───────────────┬────────┴───────────────────────────────┘
                            │
                ┌───────────┴────────────┐
                │ code-control            │ repos, changes, work items, checks, previews,
                │  HTTP API /v1/code      │ events (SSE), GitHub sync, git routes
                └───┬──────────┬──────────┘
                    │          │
        ┌───────────┴──┐   ┌───┴───────────┐    ┌──────────────────────┐
        │ code-store    │   │ code-git      │    │ code-github          │
        │ changesets,   │   │ pkt-line,     │    │ App JWT, tokens,     │
        │ lander, git   │   │ packs, smart  │    │ webhooks, REST       │
        │ derivation    │   │ HTTP srv+cli  │    │ client, event mapping│
        └───┬──────┬────┘   └───────────────┘    └──────────────────────┘
            │      │
     SQLite meta   pg_storage::Objects (S3 or local dir): blobs
```

Previews and checks run as containers through the same driver that runs Postgres computes.
Previews branch the repository's linked roll Postgres project and roll Redis database.

## Crates

| crate          | owns                                                                                   | network I/O |
| -------------- | -------------------------------------------------------------------------------------- | ----------- |
| `code-store`   | data model, SQLite metadata, blob store, git object derivation, lander, 3-way merge, reads, materialize | object storage only |
| `code-git`     | git wire protocol: pkt-line, pack read (with deltas) and write, smart HTTP server (upload-pack v2, receive-pack) and client (fetch for import, push for mirror). Generic over a `GitBackend` trait. | HTTP client |
| `code-github`  | GitHub App auth (RS256 JWT, installation tokens), webhook verification, REST client, mapping GitHub events to roll records | HTTP client |
| `code-control` | control plane: repos, changes, work items, checks, previews, events, API router; adapts `code-store` to `code-git::GitBackend` | yes |

`rolld` mounts `code_control::api::router`. `roll code …` (roll-cli) and `/dashboard/code` both
use the HTTP API.


## Namespaces and public URLs

Every repository has a full name, `<owner>/<repo>`, exactly as on GitHub.

- **Owners** are users or organisations and share one global namespace. Owner names:
  - are 1–39 characters of `[a-z0-9-]`
  - can't start or end with `-` or contain `--`
  - are case-insensitive, stored lowercase, and first come, first served
  - can't be reserved words: every top-level route of roll.sh (`dashboard`, `api`, `docs`,
    `login`, `new`, `settings`, `explore`, `pricing`, …) is reserved, see
    `code_control::names::RESERVED`
  - can be checked live with `GET /v1/code/names/{name}`
- **Repo names**:
  - are 1–100 characters of `[A-Za-z0-9._-]`
  - aren't `.` or `..`, and don't end in `.git`
  - are unique case-insensitively within their owner
  - keep their display case
- **Renames** keep a redirect from the old full name, as GitHub does, until the old name is
  reused.
- **Visibility**:
  - A `public` repo is readable by anyone at `roll.sh/<owner>/<repo>` and over git.
  - A `private` repo needs a token.
  - Writes always need a token. Over git, the token is the password and any username works.
- **Public URLs** on roll.sh are served by the Next app:
  ```
  roll.sh/<owner>                                  profile: repos, activity
  roll.sh/<owner>/<repo>                           README, files, live landing feed
  roll.sh/<owner>/<repo>/tree/<rev>/<path>         directory
  roll.sh/<owner>/<repo>/blob/<rev>/<path>         file (highlighted)
  roll.sh/<owner>/<repo>/commits/<rev>             first-parent log
  roll.sh/<owner>/<repo>/changeset/<id>            changeset + diff
  roll.sh/<owner>/<repo>/changes[/<id>]            changes (≈ PRs)
  roll.sh/<owner>/<repo>/work                      work items (≈ issues)
  roll.sh/<owner>/<repo>.git/…                     git smart HTTP (rewritten to rolld /git/…)
  ```
  The dashboard (`/dashboard/code`) is where you manage repos. The public pages are where you
  read them.
- **Status codes**: a missing owner or repo answers a real 404, and a renamed repo answers a 308
  to its new name with the rest of the path kept. Pages stream, and a streamed response is
  already committed to 200, so `apps/dashboard/src/proxy.ts` checks existence before rendering.

## Data model (`code-store`)

### Objects and ids

- **Blob**: file content. Its id is the git blob id (SHA-1 of `blob <len>\0<content>`), so git
  imports dedupe and git serving needs no translation. Stored zstd-compressed in object storage
  at `code/<repo_id>/b/<id[0..2]>/<id>`. Blobs are uploaded when a changeset is created, never
  during landing. Opened with `pg_storage::Objects::new_durable`, a local directory fsyncs each
  blob, and the directories it creates, before the write returns. That way a blob the metadata
  references survives power loss. `Objects::new` doesn't fsync. S3 writes are durable when they
  return, so S3 behaves the same either way.
- **Changeset** (Bonsai): the canonical unit of history.
  ```
  Changeset {
      parents: Vec<ChangesetId>,          // first parent is the mainline
      author, committer: Signature,       // raw git identity line incl. timestamp + tz
      message: Bytes,                     // exact bytes
      extra: Vec<(String, Bytes)>,        // gpgsig, encoding, mergetag … for byte-exact git round-trip
      files: BTreeMap<Path, FileChange>,  // relative to the FIRST parent only
  }
  FileChange = Modify { blob, mode: Regular|Executable|Symlink|Gitlink } | Delete
  ```
  `ChangesetId` = SHA-256 of a canonical binary encoding (shown as `cs_` + 12 hex chars,
  addressable by any unique prefix). A changeset never points at a full tree, which keeps
  creating it O(change).
- **Generation**: `max(parent generations) + 1`, stored with each changeset. Plus a skip-list
  pointer for O(log n) ancestry queries.

### Derived data

Derived data is computed from a changeset and its parents' derived data, recursively, eagerly
on refs and lazily elsewhere. Clients never see the internals.

- **git**: the git tree (incrementally, from the first parent's tree: only the trees on paths
  that changed are rewritten) and the git commit. Mapping tables go both ways
  (changeset ↔ git commit). Imports from git round-trip byte-exactly. That's required so the
  GitHub mirror stays fast-forward.
- **path index**: for each ref, `path → generation that last changed it`. It makes the land
  conflict check O(files in the change).
- **path history** (log of a file) and **blame** are computed lazily and cached. Blame
  (`Store::blame`) walks the first-parent changesets that touch the path and diffs each against
  its first parent (`git blame --first-parent`). It stops after 2,000 such changesets and says
  so (`complete: false`). The result is cached per (head, path).

### Metadata

SQLite (`<home>/code/code.db`), WAL mode: repos' storage records, changesets, git mapping, git
trees (small, hot), which blobs a repo has (and their sizes), annotated tags, refs, the path
index and per-minute landing counts. A ref update is a compare-and-swap on one row. There's one
writer connection and a pool of reader connections, so reads don't queue behind writes. The
schema is plain SQL so it can move to roll Postgres for multi-node.

**Durability** (`code-store/src/durable.rs`). Commits use `synchronous=NORMAL`: they append to the
WAL without an fsync, which survives the process crashing but not the machine. So a landing (a
lander batch, `Store::advance`, a ref update) is acknowledged and published only after the WAL is
fsynced past its commit. That's group commit: one `fdatasync` covers every commit made before it
started, from any writer, and it runs outside the writer's lock. Checkpoints run on a background
thread with its own connection every 50 ms, so their fsyncs never happen inside a commit that
holds the writer lock; the writer only checkpoints itself past 8,192 WAL pages, as a backstop.
Blobs are as durable as the object store: S3 is, but the local directory store doesn't fsync.

Paths are UTF-8. An import rejects a tree with a non-UTF-8 file name. A commit whose bytes
don't come back from derivation (unusual header order, legacy tree modes such as `100664`) keeps
them verbatim in the changeset (`git_raw`). Its trees are stored as imported, so serving stays
exact.

## Landing: how 40,000 merges a day work

40k merges per day averages 0.46 merges per second. Busy hours might reach 5–20 per second.
That's easy as long as landing touches only metadata and costs O(files in the change).

Each `(repo, ref)` has one **lander** task. It's an in-process actor fed by an mpsc channel.
The critical section is short and does no I/O except one SQLite transaction per batch. CI is
never inside it.

For each batch the lander drains up to N queued requests. For each request, with change `C`
based on `B` and the ref at `H`:

1. **Overlap**: the files of `C` whose path index entry on the ref is newer than `gen(B)`.
   Only a base on the ref's first-parent chain, at or above the index's floor, can use the
   index. Any other base compares the entries at `B` and `H` directly. Generations on a merged
   side branch say nothing about what the mainline changed. A non-fast-forward ref move resets
   the index and raises the floor.
2. **No overlap** (the common case): rebase trivially. The new changeset gets parent `H` and
   the same file changes. Nothing is read.
3. **Overlap**: a 3-way merge per file (base at `B`, ours, theirs at `H`). It reads three blobs
   and writes the merged blob. If any file conflicts (content, delete/modify, mode,
   file/directory), the request is rejected with the conflicting paths and the current head,
   so the agent can fix it and retry.
4. The lander derives the git tree and commit incrementally and advances the in-memory head
   and path index.

The lander also handles these cases:

- A change whose base is the head fast-forwards: the change itself becomes the head.
- For a stack, the first-parent chain down to the ref is rebased in order, up to 256 changesets.
- Merge commits can only fast-forward. A merge that needs a rebase is rejected.
- A rebase drops `gpgsig` and `mergetag` headers.
- Merged blobs are uploaded together before the batch commits.

At the end of the batch, one SQLite transaction writes the changesets, trees, mappings and
path index rows and CASes the ref from the batch's starting head. Waiters get their results,
and events go out on the repo's broadcast channel, which feeds the dashboard landing feed and SSE.

On a laptop the target is >1,000 merges per second, which means 40,000 merges in under a
minute. `crates/code-store/tests/throughput.rs` measures it with hot-file overlap: 64
concurrent agents, about 9% of changes needing a 3-way merge. Run it with
`CODE_THROUGHPUT_N=40000 cargo test --release -p code-store --test throughput -- --nocapture`.

Measured on 2026-10-22 on a 16-core Linux host that other builds were also using (load 6–11),
with local object storage, 40,000 changes each, the old and new store run alternately:

| where the store lives | merges per second | before (no fsync, inline checkpoints) |
| --------------------- | ----------------- | ------------------------------------- |
| tmpfs                 | 883–911           | 668–806                               |
| real disk (SATA SSD, RAID 0) | 448–496    | 314–365                               |

Every one of these landings was fsynced before it was acknowledged; the "before" ones weren't.
Moving checkpoints off the writer's lock gained more than the group-commit fsync costs. An earlier
run on an idle 18-core host reached ~1,650 merges/s on tmpfs; this host was shared, so compare
the columns with each other, not with that.

The single SQLite writer is the limit. Agents creating changesets compete with the lander for it.

Multi-node later: the ref CAS keeps correctness, and a lease per `(repo, ref)` picks the node
that runs the lander.

### Gating: post-merge or the merge queue

A repo's `land_policy` picks one of two ways to keep the default branch healthy:

- **`post_merge`** (the default, like Scion): land as soon as there's no textual conflict, then
  run checks on the default branch continuously; landings during a run coalesce into one more
  run on the latest head. When checks fail, roll bisects from the last green head to find the
  changeset that broke them, opens one P0 `broken-main` work item, and opens a revert change.
  With `[main] auto_revert = true` the revert lands right away; otherwise an agent claims the
  item and lands the revert or fixes forward.
- **`queue`**: nothing lands on the default branch untested. See [Merge queue](#merge-queue).

## Merge queue

With `land_policy = "queue"`, every way of landing on the default branch goes through the queue:
`POST …/land` and `POST /changes/{id}/land` answer `202 {status: "queued", position, queue_id,
change}`. A `git push` to the default branch (or `refs/land/<default>`) is rejected with
`queued as change #N (position P)` plus `remote:` lines that say how to watch it, because the
ref only moves once the change is tested. Other branches and refs aren't affected. An admin can
pass `bypass: true` to land directly; every bypass is written to the repo's audit log
(`GET …/audit`).

**Order.** Entries wait in two lanes, `urgent` before `normal`, FIFO within a lane. The batch
being tested is always first. Re-queueing a change puts it at the back of its lane.

**Batches** (`crates/code-control/src/queue.rs`). Each repo has one worker, with one batch in
flight (speculative depth 1):

1. It takes up to `batch_size` waiting changes. Any that stopped being mergeable (see
   blockers below) are ejected with the reason.
2. It rebases them one after another onto the default branch's head on a scratch ref,
   `refs/roll/queue`, with the lander's usual merge semantics. A change that conflicts with the
   ones ahead of it is ejected right away, with its paths, and the batch goes on without it.
   Refs under `refs/roll/` are internal: never listed, never counted as merges, never published.
3. It runs the queue's checks once, on the tip: `[queue] checks`, or every check `roll.toml`
   declares. A check only counts against the batch if it passes on the head the batch was built
   on. So a check that's already red doesn't eject everyone, and its fix still gets through.
4. **Green**: the whole batch lands with one compare-and-swap (`Store::advance`, which counts
   and publishes every changeset as its own landing). The tip becomes the repo's last green
   head.
5. **Red**: it bisects. Every prefix of the chain is already a rebased tree, so it tests the
   midpoints to find the first change that breaks a check. The changes before it land, it's
   ejected with the failing checks and their output, and the rest go back in line, keeping their
   places. Two culprits in a batch are found one after the other.

The batch size adapts: halved after a red batch, doubled after a green one, never above
`batch_size`. Dequeueing, revising or closing a change in the batch makes the worker throw the
batch away and rebuild it. Just before landing, every change is checked again: still queued, at
the head that was tested, and still mergeable (a required check that went red meanwhile sends
the batch back in line, and the change is ejected when it's rebuilt). If the default branch
moves under a batch (an admin bypass), the compare-and-swap fails and the batch is rebuilt on
the new head. Pausing stops new batches; the one being tested finishes. A step that fails (the
runner erroring, say) puts the batch back in line and is retried with a growing delay.

**Restarts.** Queue state lives in control.db. When a new process starts (`Code::start_queues`,
called once the runner is plugged in), batches that were being tested go back in line and are
tested again. Entries that were `landing` are checked against the default branch: if their tip
is there, they landed; otherwise they go back in line.

**Blockers.** `Code::mergeable(change)` returns every reason a change can't be queued or land
yet, as `{kind, message}`. Every landing rule adds blockers there. Today they are:

- `state`: it already landed, or it's closed.
- `draft`: it's a draft (`PATCH /changes/{id} {draft}`).
- `untrusted`: it came from someone without write access (a fork's pull request). A writer
  approves running its current head with `POST /changes/{id}/approve-run`; a new revision needs
  approving again.
- `checks`: the change's own checks haven't all passed. That's the repo's `required_checks`
  (any source, external CI included) plus, with `[queue] require_change_checks`, the queue's
  checks.

Settings always come from `roll.toml` on the default branch's head, never from the change being
landed, so a change can't loosen the rules it's checked against.

```toml
[queue]
checks = ["test", "lint"]    # what a batch must pass (default: every [checks.*])
batch_size = 8               # most changes tested together, 1-64 (default 8)
timeout = 3600               # seconds a batch may test for (default 3600)
require_change_checks = true # a change's own checks must pass before it's queued

[main]
auto_revert = true           # post_merge repos: land the revert of whatever broke main
```

**Throughput.** One batch tests up to `batch_size` changes at once, so the queue lands as many
changes per test run as the batch holds. `tests/queue.rs` puts 2,000 changes through it, with
2% of them breaking a check and a fake runner. Every other change lands exactly once, in queue
order, on a linear default branch. In a debug build this takes about a minute, at around 35
merges per second; most of the time goes into writing the tree out for each test run.

## Changes, work items, checks, previews (`code-control`)

- **Change** (≈ a PR): `chg_…`. It has a title, a body, an author (agent or human), a base, a head
  changeset (the tip of a stack), revisions, a state (`open | queued | landed | conflict | closed`),
  checks, a preview, an optional work item and an optional `github_pr`.
- **Work item** (≈ an issue): `wi_…`. It has a title, body, labels, priority, a source
  (`native` | `github#123`) and a state (`open | claimed | review | done | closed`). Agents
  `claim` the highest-priority open item and hold a lease, which they keep alive with
  heartbeats. The lease expires back to `open`. That's queue semantics, so many agents can pull
  work safely.
- **Checks** (`crates/code-preview`): runner checks come from `roll.toml`.
  - Each check runs as `sh -c <run>` in its own container. The tree is copied in, never
    bind-mounted, and the container gets memory, CPU and process limits and a timeout. The
    timeout covers the run, not creating the container (pulling its image has its own 10
    minutes), so a timeout can never leave a container behind that appeared after removal.
  - The tail of the output becomes the check's summary.
  - Every new head of an open change runs the checks. A result for a head the change has
    already moved past is dropped.
  - Post-merge: landings on the default branch run the checks on the new head. Landings that
    arrive during a run coalesce into one more run on the latest head.
  - A failure opens one P0 `broken-main` work item. It closes itself when checks pass again,
    unless an agent has claimed it.
  - At most `--code-runner-concurrency` checks run at once (default 2).
  - External checks are reported through the API.
- **Previews**: `roll.toml` `[preview]` sets an image, a command and a port, and optionally a
  roll Postgres project and a roll Redis database to branch.
  - Every change gets a preview record and a URL, `http://chg-<id>.<preview domain>`, served
    on the API port.
  - The first request branches the data as `preview/<change>` and starts the container. The
    request is held for up to 60 s; after that the visitor gets a "starting" page that
    refreshes.
  - The data branches are injected as `DATABASE_URL` / `REDIS_URL`, plus `PORT`,
    `ROLL_REPO`, `ROLL_CHANGE` and `ROLL_CHANGESET`.
  - After 10 minutes without requests the container stops. The data branches stay until the
    change lands or closes; then the preview and its branches are deleted.
  - `auto = false` creates previews only on request (`POST /changes/{id}/preview`).

```toml
# roll.toml
[checks.test]
image = "rust:1"
run = "cargo test"
timeout = 900          # seconds (default 600, max 3600)
memory = 2048          # MiB (default 1024); cpus = 1.0; env = { KEY = "value" }

[preview]
image = "node:22"
run = "pnpm i && pnpm dev --port $PORT"
port = 3000
postgres = "prj_…"     # roll Postgres project to branch
redis = "rdb_…"        # roll Redis database to branch
auto = true            # a preview for every change (default); they cost nothing until visited
```

## Agents, integrations and work trees

- **MCP** (`code-control/src/mcp.rs`, [mcp.md](mcp.md)): `/v1/code/mcp` is an MCP server
  (Streamable HTTP, JSON-RPC 2.0) with 22 tools: search, read, tree, log, blame, changesets,
  compare, changes, land, work items, comments, reviews, the queue and roll's reviewer.
  - Each tool is a request to the HTTP API router with the caller's own headers, so authorization
    and response shapes are exactly the routes'.
  - Sessions are bound to the token that started them, and browser `Origin`s are limited to roll's
    own.
- **Webhooks and commit statuses** (`code-control/src/webhooks.rs`):
  - Writers register URLs for any of the event-feed events, plus `ping`. Deliveries share the review
    apps' queue: signed, retried, logged, redeliverable and SSRF-guarded.
  - External CI (TeamCity, Jenkins, Buildkite, GitHub Actions-style runners) posts GitHub-shaped
    commit statuses by git commit. Each status becomes a check on the open changes at that head, so
    it gates the merge queue's `required_checks` like roll's own runner. The combined state per
    commit is readable back.
- **`@roll` on GitHub:** commands in a pull request's conversation (`issue_comment`) and in inline
  review comments run like native ones. They run only for users GitHub reports as `admin`,
  `maintain` or `write` (the collaborator permission API); everyone else is treated as a reader.
- **Search at any revision:** the index follows the default branch. `/search?rev=` searches the
  files that differ between the default head and `rev` directly at `rev`, and lets the index
  answer for the rest.
- **Work trees** (`code-control/src/checkout.rs`):
  - The merge queue and the checks runner lease one of up to 4 persistent trees per repo
    (`<home>/code/worktrees/<repo>/<n>`) for each job.
  - A tree moves to the job's changeset by applying the diff from where it was
    (`Store::checkout_delta`). That's O(files changed), not O(repo), which is what makes testing a
    new batch tip cheap.
  - Any error, or a tree this process hasn't written yet, means a fresh `materialize`.
  - When all 4 trees are busy, the job gets a throwaway checkout.
  - Runners copy trees into their containers, so nothing writes into a leased tree.
  - On the 2,000-change queue test (2% culprits, batch size 8), the queue went from 25 to 74–124
    merges/s (two runs on a shared machine).

## GitHub-connected repositories

When a user creates a repository, they choose **Create on roll** (native) or **Connect GitHub**.
Setting up the App is described in [github-app.md](github-app.md); the code is in
`crates/code-control/src/github/`.

- **Import**:
  - roll fetches every branch and tag over git with the installation token, then ingests them
    into changesets in chunks, parents first, with progress in `Repo.import`.
  - The round-trip is byte-exact, so commit ids match GitHub's.
  - Then open issues become work items and open PRs become changes.
  - The default branch comes from GitHub. Visibility follows GitHub unless the request sets it.
- **roll is the landing authority.** GitHub's merge APIs can't sustain this volume, because of
  content-creation secondary rate limits. So roll lands, and the mirror (`github/mirror.rs`,
  decided by `code_github::Mirror`) pushes the default branch to GitHub:
  - one fast-forward push carries every commit since the last one
  - it debounces 2 s and pushes at least every 10 s while landings keep coming
  - 200 landings take a handful of pushes
  - `Repo.github.mirror` reports `ok | pending | error | paused`, `behind` and the last push
  - Recommended: branch protection on `main` that allows only the roll App to push.
- **Inbound** (webhooks):
  - Verified with `X-Hub-Signature-256` and deduplicated by `X-GitHub-Delivery`. A failed
    delivery can be redelivered.
  - Pushes to other branches update the branch and open or revise its change. Deleting a
    branch closes its change, unless a PR owns it. Tags follow GitHub's.
  - PRs into the default branch become changes. A same-repo PR adopts its branch's change.
    PRs into other branches are ignored, because roll lands on the default branch. Title,
    body and draft follow the PR (`edited`, `ready_for_review`, `converted_to_draft`); a PR
    going back to draft takes its change out of the merge queue.
  - A PR from a fork becomes an untrusted change (`trusted: false`). Its author has no write
    access, so roll doesn't run its checks or start its preview on its own: both run the PR's
    code with the repo's data. A writer can still start a preview.
  - A PR closed on GitHub closes the change; reopening it reopens the change. One merged on
    GitHub marks it landed, and the push brings the commits. Either way the change leaves the
    merge queue.
  - Issues create, update and close work items.
    - Labels `P0`–`P3` set the priority.
    - Reopening puts the item back in the queue.
    - Comments on issues are ignored. `@roll` commands in a PR's conversation are run for
      writers (see [reviews.md](reviews.md)).
  - Uninstalling or suspending the App, or removing the repo from it, disconnects the repo and
    pauses the mirror. Reinstalling resumes it.
- **A direct push to the default branch on GitHub** is ingested as landed history:
  - When roll has nothing newer, it's a fast-forward.
  - When both sides moved, roll creates a merge whose first parent is roll's head and whose
    second is GitHub's commit. That keeps roll's mainline first-parent and lets GitHub
    fast-forward to it.
  - When both sides changed the same file, or history was rewritten, the mirror pauses with
    the paths in `mirror.error` until someone reconciles.
  - roll's own mirror pushes coming back (`<slug>[bot]`) are ignored.
- **Outbound** (`github/notify.rs`, driven by the control plane's event feed, once each):
  - Landing a change from a PR comments with the landed commit and closes the PR. It's closed,
    not merged, because roll already put the commit on `main`.
  - Finishing a work item from an issue comments and closes the issue (`completed` or
    `not_planned`). Nothing goes back for state that came from GitHub.
  - Checks on a PR's change become check runs on its head commit.

## HTTP API (`/v1/code`)

`{owner}/{repo}` is the full name. Revisions accept a ref name, a changeset id or prefix, or a git
commit id. The exact response shapes are in [http-api.md](http-api.md).

```
GET    /v1/code                                    info + stats (merges today, merges/s, queue depth)
GET    /v1/code/names/{name}                       {available, reason?: "taken" | "reserved" | "invalid"}
GET    /v1/code/names/{owner}/{repo}               the same, for a repo name
GET    /v1/code/owners · POST {name, kind, display_name?}
GET    /v1/code/owners/{owner}                     profile + repos (public only without a token) · PATCH
POST   /v1/code/owners/{owner}/tokens              {name} → token (shown once) · GET: list (no secrets)
DELETE /v1/code/owners/{owner}/tokens/{id}         revoke
GET    /v1/code/repos?owner=                       list
POST   /v1/code/repos                              {owner, name, visibility?, description?, source: "native" | {github: {installation_id, owner, repo}}, default_branch?}
GET    /v1/code/repos/{owner}/{repo}                       detail (head, stats, clone urls, github link)
PATCH  /v1/code/repos/{owner}/{repo}                       {name?, description?, visibility?, land_policy?, required_checks?, default_branch?}
DELETE /v1/code/repos/{owner}/{repo}

GET    /v1/code/repos/{owner}/{repo}/refs
GET    /v1/code/repos/{owner}/{repo}/tree/{rev}/{*path}    directory listing
GET    /v1/code/repos/{owner}/{repo}/blob/{rev}/{*path}    raw file
GET    /v1/code/repos/{owner}/{repo}/readme/{rev}?path=    rendered README
GET    /v1/code/repos/{owner}/{repo}/blame/{rev}/{*path}   first-parent blame
GET    /v1/code/repos/{owner}/{repo}/paths/{rev}           every file path (the file finder)
GET    /v1/code/repos/{owner}/{repo}/log?rev=&path=&limit=&cursor=
GET    /v1/code/repos/{owner}/{repo}/changesets/{id}       metadata + file diff
GET    /v1/code/repos/{owner}/{repo}/compare/{base}...{head}
POST   /v1/code/repos/{owner}/{repo}/changesets            {base, message, author?, files: {path: {content | content_b64 | delete: true, mode?}}}
POST   /v1/code/repos/{owner}/{repo}/land                  {base, message, files…} or {changeset} → landed | conflict (fast path for agents)

GET    /v1/code/repos/{owner}/{repo}/changes?state=
POST   /v1/code/repos/{owner}/{repo}/changes               {title, body, changeset | (base + files), work_item?}
GET    /v1/code/repos/{owner}/{repo}/changes/{number}      by number, #number or id
GET    /v1/code/changes/{id}
POST   /v1/code/changes/{id}/revise                {changeset | (base + files)}
POST   /v1/code/changes/{id}/land                  {bypass?, lane?} → landed | conflict | 202 queued
POST   /v1/code/changes/{id}/close
POST   /v1/code/changes/{id}/checks                {name, status, url?, summary?}
POST   /v1/code/changes/{id}/preview  · DELETE

GET    /v1/code/repos/{owner}/{repo}/work?state=&label=
POST   /v1/code/repos/{owner}/{repo}/work                  {title, body, labels, priority}
POST   /v1/code/repos/{owner}/{repo}/work/claim            {agent, labels?, lease_s?} → item | 204
GET    /v1/code/repos/{owner}/{repo}/work/{number}         by number, #number or id
GET    /v1/code/work/{id} · PATCH
POST   /v1/code/work/{id}/heartbeat · /release · /complete

GET    /v1/code/repos/{owner}/{repo}/events                SSE: landed, change, work, check, preview, queue, review, thread, push

GET    /v1/code/repos/{owner}/{repo}/webhooks · POST       repo webhooks (the same events)
GET    /v1/code/repos/{owner}/{repo}/webhooks/{id} · PATCH · DELETE
POST   /v1/code/repos/{owner}/{repo}/webhooks/{id}/ping
GET    /v1/code/repos/{owner}/{repo}/webhooks/{id}/deliveries
POST   /v1/code/repos/{owner}/{repo}/webhooks/{id}/deliveries/{did}/redeliver
GET    /v1/code/repos/{owner}/{repo}/statuses/{rev} · POST commit statuses from external CI

GET    /v1/code/repos/{owner}/{repo}/queue                 the merge queue: batch, entries, stats, settings
POST   /v1/code/repos/{owner}/{repo}/queue/pause · /resume
GET    /v1/code/repos/{owner}/{repo}/audit                 bypasses, pauses, approved runs (writers)
POST   /v1/code/changes/{id}/enqueue {lane?} · DELETE      queue / dequeue · PATCH {lane}: change lanes
POST   /v1/code/changes/{id}/approve-run                   run an untrusted change's head
PATCH  /v1/code/changes/{id}                               {title?, body?, draft?}

GET    /v1/code/github                             app configured? install url
GET    /v1/code/github/installations               repos the App can see (to connect)
POST   /v1/code/github/webhook                     X-Hub-Signature-256 verified
GET    /v1/code/github/setup                       the App's setup URL: back to the dashboard

POST   /v1/code/mcp · GET (405) · DELETE              MCP server (Streamable HTTP; mcp.md)

                                                   reviews, threads, review apps and roll's reviewer:
                                                   http-api.md ("Reviews", "roll's reviewer")

GET    /git/{owner}/{repo}.git/info/refs?service=…  smart HTTP
POST   /git/{owner}/{repo}.git/git-upload-pack
POST   /git/{owner}/{repo}.git/git-receive-pack
```

Over git, a push to the default branch is a land request (on a queue repo: a queue request; see
[Merge queue](#merge-queue)). It fast-forwards when it can.
Otherwise the lander rebases the pushed commits onto the head, merging overlapping files on
the server. If a file conflicts, the push fails and the message names the conflicting paths.

git itself refuses to push a branch that isn't a fast-forward of what it last fetched. So
agents land with `git push origin HEAD:refs/land/main` (`refs/land/<branch>` for any branch).
That ref is never stored, so there's nothing to fetch first, and the push lands from whatever
base the agent had. A push to any other branch updates that branch and creates or revises its
change; deleting the branch closes the change.

### git server memory

The git server (`code-git`) never buffers a pack whole. A push's pack is parsed as it arrives
(`pack::Parser`): only a few bytes of input are kept, and entries are inflated as their bytes
come in. A fetch's pack is written and sent entry by entry, in frames of at most 64 KiB.

All requests on a node share one memory budget (`code_git::MemoryBudget`). These
`ServerConfig` fields set it:

| field | default | meaning |
| ----- | ------- | ------- |
| `memory_budget_bytes` | 1 GiB | everything all requests hold together |
| `max_request_memory_bytes` | ¾ of the budget | the most one request may hold |
| `memory_wait` | 30 s | how long a request waits for memory before it's busy |
| `memory_budget` | none | a shared `MemoryBudget` to use instead of the fields above |

Each request takes a lease and charges it, as it goes, for what it actually holds:

- **Push (receive-pack).**
  - Each body chunk is charged as it's read (not the `Content-Length`) and released once the
    parser has consumed it.
  - Every inflated entry is charged before its buffer grows, plus 160 bytes of bookkeeping.
  - Every delta result is charged as it grows, and each delta is released once applied.
  - Thin-pack bases fetched from the backend are charged until resolving ends.
  - The objects stay charged until `GitBackend::receive` returns.
- **Fetch (upload-pack).**
  - The request body (wants and haves) is charged as it's read.
  - The plan is charged at 64 bytes per object.
  - The frames in flight are charged at about 650 KiB.
  - Objects are read in batches sized from the last batch's average object size (about 8 MiB
    each, 1–256 objects). A batch is charged as soon as it's read. A batch too big for one
    request's share is dropped and read again one object at a time.
  - Each entry is charged at its worst-case compressed size while it's written.
- **Advertisements and `ls-refs`** aren't charged beyond their request body.

The lease is RAII. Everything is released when the request finishes, fails, or is dropped
because the client went away. Dropping it also cancels any charge still waiting on a blocking
thread.

Big requests can't starve small ones. These rules apply:

- A request that would hold more than `max_request_memory_bytes` fails at once, without
  waiting.
- Once a request holds more than a small allowance (a quarter of the remainder, up to 64 MiB),
  it can only grow while the node's total stays under `max_request_memory_bytes`. The last
  quarter of the budget is kept for small requests, so negotiation, `ls-refs` and small pushes
  keep flowing while a large push holds most of it.
- If every request that holds memory is waiting for more, none can finish. The youngest of
  them fails at once (wait-die), so the older ones, which have done more, go on.

A request that can't get memory fails cleanly and leaves nothing behind:

- **Push.** The rest of the body is read and dropped without being held. The client then gets
  report-status `unpack the git server is busy …` or `unpack this request needs more than N
  bytes …`, with `ng` for every ref, and git prints `remote unpack failed`. Nothing reaches the
  backend. Running out before the commands have been read also gives this report, because up to
  64 KiB of commands are read uncharged. Past that the push gets a 503.
- **Fetch.** Running out before the response starts gives `503` with `Retry-After: 5`. Once the
  pack has started, the client gets a side-band error.

With the defaults, a push can hold at most 768 MiB. That's less than `Limits::max_unpacked_bytes`
(2 GiB), so a push that unpacks to more fails unless the budget is raised. In a cluster the
budget is per node.

Measured with 8 concurrent pushes, each 200 MiB of incompressible blobs. The load harness is
`cargo run --release -p code-git --example push_load`:

- Before the budget, the server peaked at 3.2 GiB RSS, and under a 2 GiB memory cap it was
  OOM-killed in under a second.
- With the budget, it peaks at about 820 MiB, cap or no cap. Four pushes land and four get the
  busy report within about 4 s.

With 8 concurrent 80 MiB pushes, all of them land: the peak is 1.32 GiB before and 678 MiB
after.
