# roll code — reviews

Every change can be reviewed by people, by external AI reviewers (Greptile, CodeRabbit,
Copilot, …) and by roll's own reviewer. Every review is the same kind of record: a verdict plus
inline threads. So all of them can count toward a repo's merge rules, and the merge queue gates
on them through `Code::mergeable`.

## Review model (`code-control`, `src/review/`)

The exact shapes are in [http-api.md](http-api.md#reviews).

```
Review  = {id, change, revision, reviewer: {kind: "user" | "agent" | "app" | "roll", name, app?},
           verdict: "approve" | "request_changes" | "comment", body, created_at,
           source: "roll" | "github", stale: bool /* true once the change has a newer revision */}
Thread  = {id, change, path, line, side: "new" | "old", revision, resolved, outdated,
           comments: [{id, author, body, suggestion: string | null, created_at, source}]}
```

- **Who reviews:**
  - A review app's token reviews as the app.
  - An owner token reviews as the owner, or as the agent named in `X-Roll-Agent` (or in a token
    named `agent:<name>`).
  - The admin token reviews as `X-Roll-Agent`, else `X-Roll-User`, else `admin`. On an open local
    daemon, every caller has the admin token.
  - Anyone signed in who can read the repo can comment. Approving or requesting changes needs
    write access, an app installed on the repo, or roll.
- **Verdicts:** each reviewer's latest approve or request_changes counts; a comment doesn't
  change it.
  - An approval doesn't count when it's from the change's author (unless
    `allow_self_approval`), stale (with `dismiss_stale`), or from an app or roll (unless
    `count_ai_approval`).
  - A request for changes blocks until that reviewer approves, or a writer (or the reviewer)
    dismisses it.
- **Suggestions** are GitHub-style ```` ```suggestion ```` blocks. `POST /threads/{id}/apply` turns
  one into a new revision of the change, attributed to whoever applied it, and resolves the
  thread. Only writers can apply. A suggestion for lines that changed since is refused (409).
- **Stale reviews:** a new revision marks earlier approvals stale; `dismiss_stale` controls
  whether they still count. Threads move to the new revision when their lines are unchanged:
  lines added above move them down, lines removed above move them up. A thread whose lines
  changed, or got a line inserted inside them, is marked `outdated`.
- **Review requests:** `POST /changes/{id}/review-requests` asks someone (`@name`, `agent:name`,
  `app:name`, `roll`) for a review of the current head. A required request adds a
  `review_pending` blocker until that reviewer reviews the head.
  - roll's reviewer calls `Code::request_review` / `Code::finish_review_request` and posts with
    `Code::submit_review(…, &Reviewer::roll(), …, &Origin::ROLL)`.
  - A required review app is asked for every new head automatically.

## Review rules

Rules come from repo settings (`PATCH /review-settings`) plus `.roll/review.yaml` at the head of
the default branch. They're never read from the change.

- The file can only tighten the repo's settings:
  - approvals take the larger count
  - the `require_*`, `block_on_request_changes` and `dismiss_stale` requirements add up
  - `count_ai_approval` and `allow_self_approval` hold only when both allow them
  - `auto_approve` rules from both apply
- A broken file falls back to the repo's settings and says why in `GET /review-settings`.
- Defaults: no approvals required, `block_on_request_changes` and `dismiss_stale` on, everything
  else off.

```yaml
required_approvals: 1            # counting user/agent/app/roll approvals
require_codeowners: true         # CODEOWNERS (or .roll/CODEOWNERS, GitHub syntax) must approve their paths
count_ai_approval: true          # can an AI approval satisfy required_approvals?
block_on_request_changes: true
dismiss_stale: true
auto_approve:                    # changes that need no human review
  - paths: ["docs/**", "**/*.md"]
  - comment_only: true           # only comments/whitespace changed (language-aware)
reviewer:                        # roll's own reviewer
  enabled: true
  mode: auto                     # auto | standard (CodeRabbit-style) | deep (Greptile-style) | max
  auto:                          # how auto picks a mode (see "Choosing the mode")
    deep_paths: ["crates/*/src/auth/**", "**/migrations/**"]
    max_paths: ["**/crypto/**"]
    standard_below_lines: 80     # small, low-risk changes get standard
    classifier: heuristic        # heuristic | model (a cheap model call scores risk first)
  speed: balanced                # fast | balanced | thorough  (see "Speed")
  prompt: |                      # custom instructions appended to roll's system prompt (cached)
    We care most about data loss and tenancy bugs. Be terse.
  prompt_file: .roll/review-prompt.md   # or keep the custom prompt in the repo
  model: null                    # override the provider's default model for this repo
  auto_review: true              # on open and on every revision (incremental)
  drafts: false
  profile: chill                 # chill | assertive
  ignore: ["**/*.lock", "vendor/**"]
  instructions:
    - path: "crates/**/*.rs"
      text: "Never size an allocation from a peer-sent count."
  rules: ["Every new HTTP route needs a test in tests/api.rs"]   # custom rules, checked on every review
```

`Code::mergeable` adds these blockers: `needs_approval`, `needs_codeowner` (with the owners
named), `changes_requested`, `unresolved_threads` (when required, outdated threads included) and
`review_pending`.

- **Auto-approval:** an auto-approved change needs no approvals or code owners, but requested
  changes and pending required reviews still block it.
  - `paths` must match every path the change touches.
  - `comment_only` uses a language-aware check. Every changed line must be blank or comment-only,
    and the code must be the same once comments are dropped, with strings kept and indentation
    kept where it's syntax.
  - Unknown languages are never comment-only.
- **A change's paths** are everything since its merge base with the default branch, so a
  trailing docs commit can't hide a code change.
- **CODEOWNERS** is GitHub's syntax, read from the first of `CODEOWNERS`, `.roll/CODEOWNERS`,
  `.github/CODEOWNERS` and `docs/CODEOWNERS`.
  - The last matching line wins, and a line without owners un-owns its paths.
  - `docs/*` owns `docs/a.md`, not `docs/sub/b.md`.
  - Owners:
    - `@name` is a user or an org token. For GitHub repos it's the GitHub login.
    - `@org/team` is the org's token (roll has no teams yet).
    - `agent:name`, `app:name` and `roll` work as named.
- **Landing straight on the default branch** (`/land`, `git push` to it or `refs/land/<it>`) on a
  repo whose rules require reviews opens a change instead, and says so (`review_required`).
  - Admins can `bypass`, which is audited as `review_bypass`.
  - On a queue repo the queue checks reviews before queueing and before landing a batch.

## External reviewers

- **GitHub-connected repos:** Greptile, CodeRabbit, Copilot and others review the PR on GitHub
  as usual. roll ingests `pull_request_review`, `pull_request_review_comment` and
  `pull_request_review_thread` webhooks into Reviews and Threads with `source: "github"`.
  - Bot accounts are `kind: app` with their login (`coderabbitai[bot]`), and people are users.
  - Only people who can push decide. Anyone can approve a public PR on GitHub, so an approval or
    change request from a user whose `author_association` isn't `OWNER`, `MEMBER` or
    `COLLABORATOR` (or is missing) is kept as a `comment`. It never counts toward
    `required_approvals`, and never blocks. Apps count only through `count_ai_approval`.
  - Webhooks are keyed by GitHub's ids: a redelivery changes nothing, and edits, deletions,
    dismissals and resolving update in place.
  - Inline comments that arrive before their review are linked to it when it arrives, and
    `pending` reviews are ignored.
  - Reviews made in roll go to the PR as one GitHub review, with their inline comments.
    Standalone threads become PR comments and replies become replies, posted once each and
    keyed, so replies on GitHub land in the right thread.
  - If GitHub refuses an App's approve or request-changes, the review is posted as a comment
    that says so.
  - roll's own webhooks are recognised by the App's bot login and ignored.
  - Resolving or unresolving a thread in roll does the same on GitHub, through GraphQL
    (`resolveReviewThread` / `unresolveReviewThread`). The thread's node id comes from GitHub's
    thread webhooks, or is looked up once by the thread's first comment. Each state is sent once,
    and resolutions that came from GitHub aren't sent back.
- **Native repos:** the review-app API. An owner registers a review app with `POST
  /v1/code/owners/{owner}/review-apps {name, webhook_url, events}`, which returns `{id,
  token, secret}`. The app then:
  - receives signed `change.opened`, `change.revised` and `thread.comment` webhooks, each
    carrying the diff, compare and blob URLs, scoped to the repos it was installed on
  - receives `review.requested` if it's `required`, and then owes every new head a review (a
    disabled or deleted app owes nothing)
  - posts reviews and comments with its token through the same `/reviews` and `/threads`
    endpoints people use
  - reads its repos, private ones included, and can't write code

  Any vendor can integrate this way.

  Deliveries are retried with backoff (8 attempts, from `review_app_retry_ms`, ×3 each, at most
  6 h) and logged. Webhook URLs must be public addresses unless the config allows private ones
  (an open local daemon does). The check runs at connect time, so DNS rebinding can't get around
  it, and redirects aren't followed. App tokens are scoped even on an open daemon.

## roll's reviewer (`code-review` crate)

A crate with no HTTP routes of its own. code-control drives it (`src/ai/`), and a second crate,
`code-index`, holds the codebase index. Routes and shapes are in
[http-api.md](http-api.md#rolls-reviewer).

**How a review runs.**

1. A change opens, gets a new revision or is marked ready. If the repo's reviewer is
   `enabled` and the change qualifies, a job is queued in `ai_jobs`. A change qualifies when:
   - `auto_review` is on
   - it isn't a draft, unless `drafts` is set
   - it's trusted or approved to run (fork PRs aren't reviewed on their own)
   - it isn't paused

   A queued review absorbs later revisions. When the reviewer is `required`, the change owes
   roll a review (`review_pending`) until roll finishes or gives up.
2. A runner with `ROLL_REVIEW_CONCURRENCY` slots (default 2) runs jobs. After a restart, jobs
   that were running start over.
3. The job reviews one of two diffs:
   - the change since its merge base with the default branch
   - after an earlier roll review, only what changed since that review's head, within the
     change's files (`incremental`; `full` or `@roll full review` reviews everything)

   Files matching `ignore` are dropped. So are binary files, and files past `max_files`; each is
   listed in "Not reviewed". When nothing is left, the job is `skipped` and nothing is posted.
4. It chooses the mode, the key and the price.
5. It builds the prompt: roll's system prompt, then the repo block. For `deep` and `max` it adds
   repository context from the index.
6. It runs the engine and posts a roll review with its inline threads. With `fast`, threads are
   posted as each file group finishes.
7. It attaches the `ai` record and writes a metered usage row.

**Failures.**
- A failed attempt is retried twice with backoff (30 s, then ×4).
- Bad keys, a missing provider and invalid settings aren't retried.
- When roll gives up, it posts a `comment` review saying why and stops owing the change a review.
  A broken key never blocks merges forever.

- **Providers:** the `Provider` trait has two implementations:
  - `anthropic`: the Messages API over raw HTTP, model `claude-opus-5-5`
    - `output_config.format` with a JSON schema for the review
    - `cache_control` on the stable prefix (system prompt, repo guidelines, learnings)
    - `fallbacks: "default"` with `anthropic-beta: server-side-fallback-2026-07-01`
    - handles `stop_reason` `refusal` and `max_tokens`
    - retries 429/529/5xx with `retry-after`
    - `/v1/messages/count_tokens` for chunking
  - `openai_compatible`: chat completions with a `json_schema` response format, for OpenAI,
    OpenRouter or self-hosted models.

  The model and effort can be set per repo.
- **Request rules (Anthropic, raw HTTP).**
  - Depth is always set explicitly as `output_config.effort`.
  - `thinking` is never sent (it's always on for Claude Opus 5.5; `disabled` and
    `budget_tokens` are 400s).
  - No forced `tool_choice` and no assistant prefill (both 400).
  - The answer is a JSON schema in `output_config.format`. Every object has
    `additionalProperties: false` and requires all its keys, enforced by `schema::check` in
    unit tests.
  - `fallbacks: "default"` is sent only to the models that support it, so not to Claude Haiku 5.5.
  - `count_tokens` gets the same `system` and `messages` without `output_config` or `fallbacks`.
- **Effort** follows mode and speed unless `effort` overrides it:

  | speed | standard | deep, max |
  | --- | --- | --- |
  | fast | low | low |
  | balanced | medium | high |
  | thorough | xhigh | xhigh |

  `fast` and the classifier use the secondary model (`claude-haiku-5-5` by default).
- **Keys:**
  - `hosted` is the operator's key, from `ROLL_REVIEW_PROVIDER`, `ROLL_REVIEW_ANTHROPIC_API_KEY`
    or `ANTHROPIC_API_KEY`, or `ROLL_REVIEW_OPENAI_*`.
  - `byok` is the owner's own key (`PUT /owners/{owner}/review-provider`).
    - It's sealed with AES-256-GCM under `ROLL_SECRET_KEY` (at least 32 characters), with the
      owner id as associated data.
    - The API returns only its last 4 characters, and it's never logged.
    - An owner's own key, when set, is used for all their reviews.
- **Modes:**
  - `standard`, CodeRabbit-style:
    - a walkthrough summary
    - a per-file change table
    - an optional mermaid sequence diagram when control flow changed
    - inline comments with severity (`bug | security | perf | style | nit`) and committable
      suggestions
    - incremental reviews: only the interdiff since the last reviewed revision
  - `deep`, Greptile-style: the same output, plus repository context from the codebase index.
    That means:
    - definitions and references of the changed symbols
    - callers and callees
    - related tests
    - similar code
    - the custom rules
    - a confidence score for each comment
  - `max`: `deep` plus a verification pass that re-checks every finding against the code and
    drops unconfirmed ones.
- **Choosing the mode (`auto`):** before a review, roll scores the change's risk and picks
  `standard`, `deep` or `max`. The score combines:
  - lines changed and the number of files and modules touched
  - paths: `deep_paths`, `max_paths`, CODEOWNERS-sensitive areas, migrations, auth, crypto
    and concurrency code
  - symbol fan-out from the codebase index: how many callers the changed public symbols have
  - whether tests changed alongside the code
  - the author's recent reverts and ejections from the merge queue
  - labels on the linked work item

  `classifier: model` adds one small, cheap model call that returns `{mode, reasons}`. The
  chosen mode and the reasons appear at the top of the review and in the metering record, so it
  is clear why a review cost what it did. A command can force a mode: `@roll review deep`.
- **Speed** sets latency against depth:
  - `fast`:
    - a smaller or faster model configured for the provider
    - low effort
    - files reviewed in parallel, one request per file group
    - no diagram or verification pass
    - comments posted as each file finishes
  - `balanced` is the default.
  - `thorough` uses higher effort, the full verification pass and wider retrieval.

  Separately, a stable prompt prefix keeps cache hits high across reviews of the same repo, and
  `speed` can be set per mode.
- **Custom prompt:** `prompt` or `prompt_file` is appended after roll's system prompt, inside the
  cached prefix. Per-path `instructions` and `rules` still apply. The repo's `AGENTS.md`,
  `CLAUDE.md` and `REVIEW.md` are included automatically when present.
- **Codebase index:** derived data per head changeset, updated incrementally per landed change:
  - symbol definitions and references, extracted lightly per language, no heavy parsers
  - a trigram index for code search
  - It also powers the `/search` API and the MCP `search` tool.
- **Chat and commands:** replying `@roll` in a thread gets an answer in context. Commands:
  `@roll review` · `@roll full review` · `@roll pause` · `@roll resume` · `@roll resolve` ·
  `@roll explain` · `@roll fix` (opens a revision with the suggested fix).
- **Who can command roll:** only writers. A command from anyone else is ignored, because replies
  cost the owner money. `review`, `pause`, `resume`, `resolve` and `don't flag` act at once.
  `explain`, `fix` and questions become `command` jobs. `fix` applies the thread's suggestion, or
  roll writes one, as a new revision; roll then reviews that revision incrementally.
- **roll approves only what it read.** The verdict follows the findings: blocking findings mean
  `request_changes`, other findings `comment`, none `approve`. An `approve` is lowered to
  `comment`, with the reason in the body, in two cases:
  - Files went unreviewed for any reason but the repo's own `ignore`: past `max_files`, binary,
    too large, or declined by the model. A change can't buy an approval by hiding code past the
    limit or by tripping a refusal.
  - An incremental review leaves roll's earlier findings open: it only read the new commits.

  Repo content, change text and comments are untrusted input to the model. Its answer is data:
  findings are checked against the diff, no command is ever taken from it, and an approval counts
  toward `required_approvals` only with `count_ai_approval` (off by default).
- **Learnings** are created in three ways:
  - `@roll don't flag …` (or `ignore …` / `remember …`)
  - someone replies to a roll thread with a reason and resolves it (one learning per thread)
  - a roll review is dismissed with a reason

  Learnings from these are repo-wide. `POST …/learnings {text, path}` adds one scoped to a glob.
  All of them go in the cached repo block, sorted by id, so the prefix stays stable until a
  learning changes. They're listed and deleted with `/v1/code/repos/{o}/{r}/learnings`.
- **The codebase index (`code-index`)** follows the default branch.
  - It's stored in `<home>/code/index/<repo>.db`. After its first build, each landing is applied
    as a diff in the background. The first build happens on first use (a deep review, a search),
    one file at a time, one repo at a time.
  - For each file it keeps the symbol definitions (Rust, TS/JS, Python, Go, Java/Kotlin, C/C++,
    Ruby and PHP patterns), identifier references with their first few lines, and lowercased
    trigrams. Contents aren't stored: search confirms each candidate on the file.
  - These files aren't indexed:
    - binary
    - over 1 MiB
    - minified
    - vendored or lock files
  - Deep context covers the changed symbols (those defined on added lines):
    - their uses outside the change, with snippets
    - their other definitions
    - related tests (by symbol or by file name)
    - similar code to the largest added block
  - Fan-out for `auto` is the number of files that use a changed symbol.
- **Large changes are never silently truncated.** Files are chunked by token count. Anything
  skipped (ignored, binary, too large) is listed in the walkthrough.

## Pricing and metering

Every roll review records its provider, model, input/output/cache tokens, files reviewed and
mode, and is metered to its owner each month.

| | roll-hosted | bring your own key |
|---|---|---|
| standard | $0.10 per reviewed file, min $0.25 per review | $0.02 per review platform fee + your provider bill |
| deep | 3× standard | $0.05 per review |
| max | 10× standard | $0.15 per review |
| chat reply / command | $0.05 | free |
| public repos | 100 standard reviews / month free | free |

- **People:** a human author never pays more than $24 per month for standard reviews, matching
  CodeRabbit Essentials ($24 annual, $30 monthly) and below Greptile Pro ($30 per seat). Deep and
  max reviews are always billed per review.
- **Agents** pay per review and are never seat-capped. 40k merges a day is a usage workload, not
  a seat count.
- **Spending caps:** each owner sets `monthly_cap_usd` and `on_cap: "pause" | "byok_fallback" |
  "standard_only"`. Caps apply to hosted reviews only: a review whose estimated price would take
  the month over the cap does one of three things:
  - `pause`: it isn't run, and a comment review says so
  - `byok_fallback`: it runs on the owner's own key, or pauses without one
  - `standard_only`: a `deep` or `max` review runs as `standard`, and standard reviews continue

  Chat replies and commands count against the cap too; at the cap they're answered without a
  model call (with `byok_fallback`, on the owner's key when there is one). Jobs running at the same
  time can't overrun the cap together: each reserves its estimated price before calling the model
  and releases it once metered, and the check counts metered spend plus what running jobs hold. A
  review is estimated on every file it sends and metered on the files it read, so it never costs
  more than it reserved. A public repo's free reviews are reserved the same way: a running review
  holds the free one it counts on, and is metered as free exactly when it got one. Reservations
  live in the daemon's memory, so this holds for one node.
- **Who counts as a person** for the human cap: the change's author, unless the author is one of
  these:
  - an `agent:…` or `…[bot]` name
  - `roll`
  - the author of a change whose work item an agent claimed
- **Provider cost** is recorded per model: Claude Opus 5.5 at $4/$20 per million tokens with cache
  reads at $0.20, Claude Haiku 5.5 at $0.10/$0.50, and cache writes at 1.25× input. Other models
  are set with `ROLL_REVIEW_MODEL_RATES=model=in/out/cache_read,…`; without rates the cost is
  `null`.
- **Pricing endpoint:** `GET /v1/code/pricing` returns these rates and the competitor reference
  points (Greptile Pro $30/seat with $1/$3/$10 per base/plus/apex credit; CodeRabbit $24–30/dev
  plus $0.25 per file over the limit), like the Redis pricing comparison. `GET
  /v1/code/owners/{owner}/usage` returns month-to-date usage.
- Rates are server config (`ROLL_REVIEW_PRICE_*`), not hard-coded.
