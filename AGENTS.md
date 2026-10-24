# roll — agent guide

Monorepo for roll, serverless infrastructure. Use pnpm for the web side (`apps/*`, `packages/*`) and a Cargo workspace for the Rust side (`crates/*`).
`apps/dashboard/AGENTS.md` has extra Next.js rules (Next 16, which differs from what you may know).

## Brand

- The name is **roll**, lowercase. Never write "roll.sh" in a logo, lockup or UI title.
- roll code's tagline is **keep the code rolling**. It's lowercase in UI strips and footers, and starts with a capital at the start of prose.
- **Horizon is THE logo.** It is a dome cut into three horizontal bands that sits on a base line.
  - It is `HorizonMark` / `RollLogo` in `@roll/brand`.
  - The same mark is used in the header, sidebar, favicon, avatar and every other place where the logo appears. Don't invent variants.
  - It has to be recognisable at 16px.
- **The dot globe (`RollMark`) is a hero/status visual, not the logo.** Use it big: hero sections, loading screens and status. Never put it in a header.
- Geometry lives in `packages/brand/src/geometry.ts`.
  - Never redraw the mark by hand.
  - For static output, use `horizonSvg`, `avatarSvg` and `markSvg`.
  - After changing geometry, regenerate the assets with `pnpm --filter @roll/brand export`.

## Aesthetics

- The style is minimal and monochrome, Vercel-like but quieter.
  - Dark-first: `#000` background, `#ededed` foreground, with hairline borders (`--line`, `--line-2`).
  - Light mode is a full override, not an afterthought.
- **Accent is a subtle blue, `#4c8dff`**, and is used sparingly:
  - one accent dot on the globe
  - active or busy states
  - the primary text link
  - Never use orange. Never use gradients for decoration.
- Colours come from the CSS variables in `apps/dashboard/src/app/globals.css` (Tailwind classes such as `bg-panel`, `text-muted`, `border-line` and `text-accent`) and from `@roll/brand/tokens` in JS.
  - Don't hard-code hex values in components.
- Type is Geist Sans for UI and Geist Mono for ids, code and numbers.
  - Headlines use tight tracking (`-0.04em` to `-0.05em`) and weight 600.
  - Body text is 13–14px in the dashboard.
- Icons are `lucide-react` at 14–16px with a 1.5 stroke.
- Spacing is generous. Panels are flat (`bg-panel`, 1px border, radius 8–12) with no drop shadows.

## Motion and interactivity

The product should feel alive: dots react to the cursor and the marks "think". Every animation must still be calm and must honour reduced motion.

- **States.** These are shared by every mark: `idle | thinking | deploying | success | error | sleep`.
  - Drive them through `MarkStateProvider` (`useMarkState().play(steps, then)`) so the header logo, hero globe and dashboard all react together.
  - Don't run a separate local animation for these states.
- **Horizon (logo):**
  - Solid at rest.
  - It dissolves into dots when the pointer comes close or when the state is busy.
  - Effects per state:
    - thinking: a horizontal sweep
    - deploying: rows rise
    - success: a ripple
    - error: red plus a small jolt
    - sleep: alpha 0.45
  - Below 56px it uses one row of dots per band plus a 0.2 ghost silhouette, so the shape always reads. Keep hover working at small sizes.
- **Globe (`RollMarkEngine`):**
  - Spring physics.
  - The pointer repels and glows nodes (`reach` controls the radius).
  - Clicks send a burst ripple.
  - It can morph into the horizon formation.
- **DotField** is the background grid. The cursor makes it swell and glow, clicks send ripples, and it sweeps while thinking. Use it behind heroes and "network" cards, never behind dense text.
- **AI "thinking" UI** (`components/ask.tsx`):
  1. The mark switches to `thinking`.
  2. Step lines appear with the `.shimmer` text effect.
  3. The answer streams in with a `.caret`.
  4. The mark plays `success`.
- **Rules for every animation:**
  - Use canvas plus `requestAnimationFrame` for anything with more than a handful of moving parts, and CSS for simple fades (`.rise`).
  - Pause offscreen with IntersectionObserver.
  - Use `useReducedMotion()`. When it is on, render the static final frame, but still reflect state changes.
  - Resolve CSS colours through `resolveColor` (Tailwind v4 emits oklch). Engines cache colours, so call `setOptions` when the theme changes.
  - Ease with springs or ease-out curves. Keep durations at 150–300ms for UI and around 1–2.4s for state loops. Avoid bounce and avoid flashing.
  - Passing `undefined` to engine `setOptions` must not reset an option. Keep that filter.

## Code conventions

- **TypeScript:**
  - React 19 and Next 16 (`cacheComponents`, Turbopack).
  - Run `pnpm typecheck` before you finish; it runs `next typegen` first.
  - Brand components are client components that take `size`, `state`, `interactive` and `accent` props.
- **Rust:**
  - Edition 2024. Declare dependencies in the root `Cargo.toml` `[workspace.dependencies]`.
  - `crates/pg-proxy` is the Postgres wire proxy. `crates/pg-control` is the control plane. `rolld` is the daemon and `roll-cli` is the `roll` CLI.
  - Run `cargo build`, `cargo test` and `cargo clippy` before you finish.
  - After touching anything Postgres-related, run `pnpm test:pg` (Docker e2e; it skips itself when nothing relevant changed). New Postgres features need an e2e test in `crates/pg-control/tests/e2e.rs`.
- **Streams (Kafka-compatible):**
  - `crates/stream-log` is the partition log. It holds Kafka record batches v2 in segment files, group-commit fsync (the high watermark is the durable end), idempotent producer state and transactions, plus hooks for tiering. It does no network or object-storage I/O.
  - `crates/stream-broker` is the broker:
    - the Kafka wire protocol (`handlers/`, one module per API family; supported versions are in `handlers/mod.rs` `APIS`)
    - group and transaction coordinators, SASL tenancy, quotas
    - tiering to `pg_storage::Objects`, retention and compaction (`compaction.rs`)
    - the HTTP API (`http/`): the control plane, a connectionless data plane, and Confluent REST v3
  - Produce and fetch never decode records: batches go to disk and back to clients as received. Only compaction, timestamp lookups, compacted-topic key checks and the HTTP API decode them, through `records.rs`.
  - Compaction rewrites uploaded chunks into new objects with `.c<generation>.kb` keys and swaps them in atomically, keeping offsets. Never reuse a chunk key.
  - Local disk is a cache: closed partitions drop their local copies (`evict_on_unload`), and `local_disk_bytes` caps open ones. Never drop a segment that isn't fully uploaded or that holds an open transaction (`keep_local_from`): transaction state is rebuilt from local segments.
  - Encoding fails if a response sets a field outside its version range. Gate every field on `ctx.version`. `tests/protocol.rs` encodes every API at every version.
  - Never size an allocation from a count or length a peer sent; bound it by the bytes actually present. `kafka-protocol` is vendored with bounded decoders (`vendor/kafka-protocol/ROLL.md`), and the broker tests fail any allocation over 1 GiB (`tests/common`).
  - After touching anything stream-related, run `pnpm test:stream`. It skips itself when nothing relevant changed and runs the Java tool tests in Docker (`apache/kafka:4.1.0`).
  - Put new tests in the matching file:
    - wire behaviour: `tests/protocol.rs`
    - librdkafka: `tests/rdkafka.rs`
    - Java clients and tools: `tests/java.rs`
    - HTTP: `tests/http.rs`
    - storage: `tests/storage.rs`
    - compaction: `tests/compaction.rs`
  - Set `ROLL_E2E_S3_BUCKET` (plus `ROLL_E2E_S3_ENDPOINT` and AWS credentials) to run the broker tests against S3.
  - `roll stream …` in `crates/roll-cli` and `/dashboard/streams` both use the HTTP API.
- **Code (source control, "keep the code rolling"):**
  - `crates/code-store` is storage. It holds Bonsai-style changesets (the files changed relative to
    the first parent), the per-ref lander with 3-way merges, git derivation, and blobs in
    `pg_storage::Objects`. Its only network I/O is object storage.
  - `crates/code-git` is the git wire protocol: pkt-line, packs, and a smart HTTP server and
    client. It is generic over `GitBackend`.
  - `crates/code-github` is the GitHub App: auth, webhooks, REST, event mapping and mirror
    scheduling.
  - `crates/code-control` is the control plane: owners, repos, changes, work items, the git
    adapter and the `/v1/code` API.
  - control.db goes through `crates/code-sql` (SQLite on one node, Postgres in a cluster; SQL in
    SQLite's dialect). A read-then-write (numbering, claims, queue positions) is one `db.write`
    closure; write closures may re-run, so they only touch the database: emit, cache and notify
    after `write` returns. Never ignore a failed statement inside a write (it ends a Postgres
    transaction): use `ON CONFLICT`, or check first.
  - `crates/code-preview` runs `roll.toml` checks and previews in containers.
  - `crates/code-cluster` is membership, repo leases and forwarding; `code-control/src/cluster.rs`
    wires it in (docs/code/cluster.md). In a cluster every request naming a repo, change, thread
    or work item runs on the repo's owner: put new such routes under a routed prefix
    (`cluster::classify`). A repo's background work runs only on its owner: check
    `Code::owns_repo` before side effects, run long work under `Code::while_owner`, and resume it
    from `Code::resume_repo` (or `Code::subscribe_ownership`), never by scanning every repo at
    startup. On a single node all of this is a no-op.
  - Landing must stay O(change). Never read the full tree, scan history, or do repo-wide work
    inside the lander. CI never sits in front of it.
  - git is derived. Imports must round-trip byte-exactly, so commit ids match and the GitHub
    mirror stays fast-forward. `tests/roundtrip.rs` guards this.
  - Never size an allocation from a count or length in a pack or pkt-line. Bound it by the bytes
    present and `Limits`.
  - Pushed trees are hostile: they can repeat names, and they can hold symlinks. Anything that
    writes a tree to disk goes through `Store::materialize`, which refuses an entry that already
    exists. Read files from a materialized tree with `symlink_metadata` first, as `roll.toml` is.
  - Code from outside the repo (a fork's PR, `trusted: false`) never runs on its own, because
    checks and previews run with the repo's data. A writer approves one head at a time
    (`approve-run`). GitHub installations span tenants, so listing or connecting them is
    admin-only.
  - On a `queue` repo, only tested trees land on the default branch. Batches are built on
    `refs/roll/queue` and land with `Store::advance` (one compare-and-swap). Any other way onto
    the default branch is an admin `bypass` and goes in the audit log. Refs under `refs/roll/`
    are internal: never list, count or publish them.
  - Landing rules are blockers: add a rule to `Code::blockers`, not a check in one endpoint.
    Rules read `roll.toml` from the default branch's head, never from the change being landed.
  - Reviews (`crates/code-control/src/review/`, docs/code/reviews.md) are one record for
    people, agents, review apps, GitHub reviewers and roll. Their rules add blockers through
    `Code::blockers`, and they read `.roll/review.yaml` and CODEOWNERS from the default branch's
    head, never from the change. The file can only tighten the repo's settings. A change's paths
    are everything since its merge base, never just its last commit.
  - roll's own reviewer is `crates/code-review` (providers, prompts, modes, pricing; no routes),
    `crates/code-index` (symbols, references and trigram search per repo, following the default
    branch) and `crates/code-control/src/ai/` (jobs, metering, routes).
    - Anthropic has no Rust SDK, so `provider/anthropic.rs` speaks raw HTTP. Always set
      `output_config.effort`. Never send `thinking` (`disabled` and `budget_tokens` are 400s on
      `claude-opus-5-5`), forced `tool_choice` or an assistant prefill. Answers are
      `output_config.format` JSON schemas that pass `schema::check`. Check `stop_reason` before
      reading `content`. Send `fallbacks: "default"` only to models that support it.
    - Keep the system prompt's repo block byte-stable: it's the cache prefix. Anything that
      varies per review goes in the user message.
    - Never truncate a diff silently: whatever isn't reviewed goes in `skipped`.
    - Owners' keys are sealed (`ROLL_SECRET_KEY`) and never returned or logged.
    - No test calls a real model API: use the mock servers in `code-review/tests/engine.rs` and
      `code-control/tests/reviewer.rs`.
  - Review app tokens (`rla_…`) are always scoped to their repos, even on an open daemon.
    Webhooks only go to public addresses unless the config allows private ones
    (`ROLL_CODE_WEBHOOK_ALLOW_PRIVATE`, local development only).
  - Any URL a tenant chooses is fetched through a client with `review::apps::PublicOnly` and no
    redirects, plus a literal-host check, because IP literals skip the resolver. That covers
    webhooks, review apps and owners' own provider `base_url`
    (`ROLL_CODE_PROVIDER_ALLOW_PRIVATE` to allow). Bound what you read from it.
  - Owner-management routes check `Code::manages`/`check_manage`, not `owns`/`check_write`:
    agent tokens (`agent:<name>`) work code but never manage the owner, and review only as their
    own agent.
  - Verdicts from outside decide only when the author can: GitHub reviews only from
    `OWNER`/`MEMBER`/`COLLABORATOR`, and roll's own approval only for a review that read every
    file (`code_review::engine::guard_approval`). Model output is untrusted data.
  - Globs from callers (search `path`, CODEOWNERS, review.yaml) are matched with the table in
    `glob.rs`/`pattern.rs`. Never match `**` recursively.
  - Repo webhooks (`src/webhooks.rs`) and review apps share one delivery queue (`review_deliveries`;
    targets are `app_…` or `wh_…`): same signature, retries, SSRF guard and log. Don't add a second
    one. Events reach webhooks through `Code::emit` over an unbounded channel; landings come from
    the store's durable land log (`Store::land_log`, written in each landing's transaction) after a
    persisted cursor. Never drive anything that must not miss a landing from the broadcast
    channel: it drops under lag. Never deliver inline from a request.
  - A broken default branch opens `broken-main` items through `Code::report_broken_main` and
    closes them through `Code::main_green` (`src/broken.rs`), whether roll's runner or an external
    status noticed it. Runner items have no `status:` label; status items have `status:<context>`.
    Don't open items any other way.
  - A token is looked up on every request (API, git, MCP). Don't cache a token→caller mapping:
    revocation must take effect at once.
  - The MCP server (`src/mcp.rs`) calls the API router on the caller's behalf, so tools can't
    drift from the routes. Add a tool by mapping it onto a route, never by calling services
    directly. A land that conflicts or is blocked is a result (`status`), not an error.
  - The runner, the queue and previews get trees from `Code::checkout`: leased work trees moved with
    `Store::checkout_delta`. Never write into a leased tree, never `remove_dir_all` it, and
    fall back to a fresh `materialize` on any error. `checkout_delta` follows the same
    hostile-tree rules as `materialize`. Hold a lease only while reading or copying the tree
    (previews copy it into their container and let go). `Code::open` clears what a previous
    process left under `code/worktrees` and `code/previews`.
  - `docs/code/http-api.md` is the contract between `code-control` and
    `apps/dashboard/src/lib/code-api.ts`. Change both together.
  - The reserved owner names in `crates/code-control/src/names.rs` and
    `apps/dashboard/src/lib/reserved-names.ts` must stay identical. Every new top-level route in
    the Next app is a reserved name. A test on each side enforces this.
  - Public pages stream, so `apps/dashboard/src/proxy.ts` checks that the owner or repo exists
    before rendering. That gives real 404s and 308s for renamed repos. Keep the check there,
    not in the pages.
  - After touching anything code-related, run `pnpm test:code`. It skips itself when nothing
    relevant changed, and it runs:
    - the dashboard's vitest suite
    - the Docker preview tests
    - 40,000 merges in release
    - 2,000 changes through the merge queue
  - Every feature gets a test. `docs/code/test-coverage.md` maps features to tests; update it
    when you add either. `code-control/tests/coverage_map.rs` and `roll-cli`'s
    `every_code_command_is_in_the_coverage_map` fail when a route, MCP tool, event or `roll code`
    command has no row there, or a row names a test that doesn't exist. Put new tests in the
    matching file:
    - storage and landing: `code-store/tests/{lander,store,roundtrip,advance}.rs`
    - the merge queue, reverts and bisecting: `code-control/tests/queue.rs` (fake runner)
    - work trees: `code-store/tests/checkout.rs` and `code-control/tests/checkout.rs`
    - blame: `code-store/tests/blame.rs` (against `git blame --first-parent`)
    - webhooks, commit statuses, blame and search over HTTP: `code-control/tests/webhooks.rs`
    - MCP: `code-control/tests/mcp.rs`
    - git protocol: `code-git/tests` (`http.rs` for wire-level behaviour)
    - HTTP contract: `code-control/tests/{api,contract}.rs`
    - git end to end: `code-control/tests/{git,git_more}.rs`
    - GitHub: `code-control/tests/github.rs` and `code-github/tests`
    - checks and previews: `code-preview/tests/service.rs` (fake runner) and `docker.rs` (real
      Docker)
    - the CLI: `roll-cli/tests/code.rs`
    - dashboard logic: `apps/dashboard/src/**/*.test.ts` (vitest, node)
    - dashboard components: `apps/dashboard/src/**/*.test.tsx` (vitest, Testing Library, jsdom;
      helpers in `src/test/`)
    - HTTP statuses and the auth matrix for every route: `code-control/tests/routes.rs`
    - event stream limits: `code-control/tests/streams.rs`
    - the cluster (several nodes in one process: routing, takeover, joining, draining):
      `code-control/tests/cluster.rs`
    - two nodes racing on one control database: `code-control/tests/concurrency.rs`
- **Local state:**
  - `.roll/` holds SQLite and branch data. Never commit it.
  - Test Postgres with `docker run --rm postgres:18-alpine psql "postgres://...@host.docker.internal:55432/main?options=endpoint%3D<ep>"`, because there is no local psql.
