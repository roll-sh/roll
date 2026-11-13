# roll — agent guide

Monorepo for roll, serverless infrastructure. Use pnpm for the web side (`apps/*`, `packages/*`) and a Cargo workspace for the Rust side (`crates/*`).
`apps/dashboard/AGENTS.md` has the dashboard's TanStack Start rules (routes, loaders, server functions, env).

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
- Colours come from the CSS variables in `apps/dashboard/src/globals.css` (Tailwind classes such as `bg-panel`, `text-muted`, `border-line` and `text-accent`) and from `@roll/brand/tokens` in JS.
  - Don't hard-code hex values in components.
- Type is Geist Sans for UI and Geist Mono for ids, code and numbers.
  - Headlines use tight tracking (`-0.04em` to `-0.05em`) and weight 600.
  - Body text is 13–14px in the dashboard.
- Icons are `lucide-react` at 14–16px with a 1.5 stroke.
- Spacing is generous. Panels are flat (`bg-panel`, 1px border, radius 8–12) with no drop shadows.
- **Exception: the agents surface and the dashboard's frame** (popup, `/dashboard/agents`, desktop
  Agents window; the dashboard shell, its projects sidebar and the project canvas). At the user's
  request they have layered surfaces, soft shadows on floating things, glass popovers and
  Linear-style weights. Their rules are in `docs/code/agents-design.md`. Everything else above still
  applies there (accent, no orange, Horizon, motion).

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
  - React 19 and TanStack Start (Vite, file routes in `apps/dashboard/src/routes`, SSR).
  - Run `pnpm typecheck` before you finish. The route tree (`src/routeTree.gen.ts`) is
    generated by `vite dev` and `vite build` and committed; rebuild after adding a route.
  - Brand components are client components that take `size`, `state`, `interactive` and `accent` props.
- **Rust:**
  - Edition 2024. Declare dependencies in the root `Cargo.toml` `[workspace.dependencies]`.
  - `crates/pg-proxy` is the Postgres wire proxy. `crates/pg-control` is the control plane. `rolld` is the daemon and `roll-cli` is the `roll` CLI.
  - Run `cargo build`, `cargo test` and `cargo clippy` before you finish.
  - rolld runs the products in `ROLL_PRODUCTS` (`crates/rolld/src/products.rs` decides; the old
    `--no-*`/`--*-only` flags are shorthands). A product's new top-level API prefix goes in
    `Product::prefixes`: off products' 404s and the metrics read it. Access lines and EMF metrics
    are `crates/rolld/src/logs.rs`: log route patterns, never a path, query or header.
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
  - Branches (`branch.rs`) share chunk objects by key across streams, so an object under one stream's prefix may be another stream's data. Delete a chunk object only when the catalog says no row references it: the keys `Store::delete_chunks`, `replace_chunks`, `delete_topic` and `delete_stream` return from the same transaction. Never `delete_prefix` a stream's or topic's objects (`Broker::sweep` skips referenced keys).
  - Local disk is a cache: closed partitions drop their local copies (`evict_on_unload`), and `local_disk_bytes` caps open ones. Never drop a segment that isn't fully uploaded or that holds an open transaction (`keep_local_from`): transaction state is rebuilt from local segments.
  - On the fleet (`fleet.rs`, the broker README's "On the fleet") a stream has one owner (lease `stream:cluster:<id>`) and only it opens the stream's partitions; connections are spliced to it after SASL and HTTP is forwarded. Every catalog write that belongs to a stream is fenced (`Store::fenced`: the stream's generation checked in the same transaction), and object keys written there carry the tenure. Acknowledgements wait for the owner's write-ahead log object in object storage (`LogConfig::remote`): never move a high watermark past what object storage has, and never trust a node's local copy of a stream it just took over.
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
    - the fleet (ownership, takeover, handoff, fencing): `tests/cluster.rs`
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
  - `crates/roll-cluster` is membership, leases and forwarding for every product, on a backend
    (Memory, Postgres on the database's clock, or DynamoDB with no clocks compared);
    `code-control/src/cluster.rs` wires roll code in (docs/code/cluster.md). New products take
    leases from rolld's handle (`crates/rolld/src/cluster.rs`). In a cluster every request naming a repo, change, thread
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
  - Any URL a tenant chooses is fetched through a client with `roll_net::PublicOnly` (one crate,
    `crates/roll-net`; code-control and the model gateway re-export it) and no redirects, plus
    `roll_net::literal_private_host`, because IP literals skip the resolver. That covers webhooks,
    review apps, owners' own provider `base_url` (`ROLL_CODE_PROVIDER_ALLOW_PRIVATE` to allow),
    gateway upstreams and MCP clients' metadata documents. Bound what you read from it.
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
    one. `Code::emit` queues an event's deliveries before it returns; a state change that matters
    promises its event in its own transaction (`webhooks::Promises`, `webhook_outbox`) and emits
    it with `Code::emit_kept`. Landings come from
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
    `apps/dashboard/src/lib/reserved-names.ts` must stay identical. Every new top-level route
    (`src/routes`) and file in `public/` is a reserved name. A test on each side enforces this.
  - Public pages look the owner, repo or share up in their route's `beforeLoad`/loader, before
    anything renders (`apps/dashboard/src/lib/public-pages.ts`: `notFound()` for a real 404,
    `redirect` 308 for a renamed repo). Repo pages get the repo from the `$owner/$repo` frame's
    context; keep the check there, not in each page.
  - After touching anything code-related, run `pnpm test:code` (about a minute warm). Each part
    skips itself when nothing it covers changed. By default it runs:
    - clippy and the code crates' tests on SQLite, including a small merge-queue throughput run
    - `roll code` against a live API
    - typecheck and the dashboard's and desktop's tests (shared with `test:agents`)

    The slow suites are opt-in. Run them before a PR to `main` merges, and after touching the
    subsystem they cover:
    - `--docker`: the stores on Postgres (after touching SQL, `code-sql` or the cluster), checks
      and previews in real Docker (after touching `code-preview`), and the cluster as real
      `rolld` containers (after touching `roll-cluster` or `cluster.rs`)
    - `--stress`: 40,000 merges in release and 2,000 changes through the merge queue (after
      touching the lander, `Store::advance` or the queue)
  - Local gates share one cargo package set (`scripts/lib/gate.sh`) so nothing builds twice. A
    new gate or suite uses it rather than its own `cargo test -p` calls.
  - A test is added when it guards behaviour a person relies on and nothing else covers it: one
    per flow, not one per fix, and never for copy, layout, focus or animation details. The merge
    gate (`.github/workflows/ci.yml`) must finish in under a minute.
    `docs/code/test-coverage.md` maps features to tests; update it when you add or remove either.
    `code-control/tests/coverage_map.rs` and `roll-cli`'s `every_code_command_is_in_the_coverage_map`
    fail when a route, MCP tool, event or `roll code` command has no row there, or a row names a
    test that doesn't exist. Put new tests in the matching file:
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
    - the cluster in one process: `code-control/tests/cluster.rs`; as real `rolld` containers
      in Docker (kills, rolling restarts, Postgres restarts under load):
      `rolld/tests/code_cluster_e2e.rs` (`pnpm test:code --docker`; the image is `deploy/code/Dockerfile`)
    - event stream limits: `code-control/tests/streams.rs`
    - the cluster (several nodes in one process: routing, takeover, joining, draining):
      `code-control/tests/cluster.rs`
    - two nodes racing on one control database: `code-control/tests/concurrency.rs`
- **Access and MCP (scoped tokens, docs/permissions.md, docs/mcp.md):**
  - `roll_auth::scope` is the model (rules of product × resources × actions, `allow` or `ask`);
    `crates/roll-access` is the store, the catalogue and the one evaluator, which rolld's gate hands
    every request bearing a scoped (`rlt_`) or OAuth (`rlo_`) token. Products never check scopes
    themselves: they see `roll_auth::Operator` plus a `roll_auth::Grant` (ask `Grant::allows` for a
    second resource inside one request). Don't build a second permission system.
  - Every route rolld serves needs a row in `roll-access/src/catalog.rs` (one action on one
    product, how to find its resource, which answer fields are credentials) or a reason to stay the
    operator's; `rolld/tests/auth.rs` fails otherwise. A new product adds its name to
    `scope::PRODUCTS`, its rows, and a resolver in `rolld/src/access.rs` when its ids aren't in the
    path (or take names).
  - Grant secrets carry a checksum (`roll_access::checksum_ok`); a leaked one is revoked, never
    just reported (`Access::revoke_leaked`: roll code's landings, GitHub's secret scanning).
  - Grants are looked up on every request; never cache them.
  - After touching access or MCP, run `pnpm test:code` (it runs roll-auth, roll-access, roll-mcp
    and roll-net with roll code's suites; `--docker` runs them on Postgres too) and
    `cargo test -p rolld --test auth` (every route classified). Read-only must be enforced by the
    product (Postgres runs `read_only` SQL as a role that can only read), not by parsing.
  - roll's MCP server is `crates/roll-mcp` (`/v1/mcp`): a tool is one route (`tools.rs`), called
    through the gated router, its hints from the catalogue; list it in docs/mcp.md (a test checks).
    Never call a service directly from a tool.
- **Projects (the dashboard's workspace):** `crates/roll-workspace` stores projects and where each
  product resource sits on its canvas (docs/workspace.md); the resources themselves stay in their
  products. The dashboard reads it through `lib/workspace-api.ts` (change both together), joins it
  with every product's own API in `lib/workspace-resources.ts`, and holds it in `lib/workspace-store.ts`
  for the sidebar, Home and `/dashboard/projects/$id`. A new resource kind needs its row in all three.
- **Apps (roll apps: hosting the user's own code; docs/apps/README.md):**
  - `crates/apps-build` detects how to build a tree and writes the Dockerfile (reads files only);
    `crates/apps-control` is the store, the deploy queue, the runtimes (`runtime/`: Docker, a fake
    for tests), the waking proxy (`public.rs`, `live.rs`) and `/v1/apps`; rolld wires it in
    `crates/rolld/src/apps.rs`. `docs/apps/http-api.md` is the contract with
    `apps/dashboard/src/lib/apps-api.ts` and `roll-cli/src/apps.rs`: change them together.
  - A service is the workspace resource kind `service`; its owner is the holder of
    `apps:service:<id>`, and every `/v1/apps/services/{id}/…` route runs there (`Cluster::dispatch`).
    Put the service id, and the environment where an action targets one, in the path: the
    permissions catalogue (feature/roll-mcp) classifies routes by pattern, and production deploys
    are `deploy`, not `write`. Don't add token or scope checks in roll apps.
  - Deployments are immutable: a rollback deploys an old image again, never rebuilds. An
    environment's active deployment moves only through `Store::switch_active` (a compare-and-swap).
  - Customer code never runs on a fleet node: instances and builds go through the `Runtime` and
    `Builder` traits. Everything made is named `roll-apps-…` and labelled `sh.roll.apps=<service>`.
    Variables are sealed at rest, go to containers as Engine API `Env` values (a 0600 env file
    only over the CLI; never argv), and are
    returned in the clear only by `…/variables/values`.
  - An idle service costs nothing: only running instances are metered. Keep cold starts measured
    (`pnpm test:apps --docker` prints them; the numbers live in docs/apps/README.md, "Measured").
  - After touching anything apps-related, run `pnpm test:apps` (`--docker` for real Docker). New
    tests go in `apps-control/tests/api.rs` (in process, fake runtime) or `tests/docker.rs`.
- **Accounts (sign-in to roll.sh):** `crates/roll-accounts` (docs/accounts.md: invites, email
  codes, GitHub, Google, TOTP), mounted by `crates/rolld/src/accounts.rs`. Sessions are hashed
  cookies that rolld's gate (`roll_auth::check`) resolves on every request, never cached, with
  `X-Roll-CSRF` on writes; it puts `roll_auth::SignedIn` on the request (an admin's also carries
  `Operator`). A product that serves users reads `SignedIn`; everything else stays operator-only
  (403 for users). Never log or echo a code, token or seed. Tests: `roll-accounts/tests/flows.rs`
  (fake providers, file outbox) and `rolld/tests/auth.rs`.
- **Secrets (roll secrets: a project's secrets and environment variables):**
  - The design and the HTTP contract are `docs/secrets.md`; `apps/dashboard/src/lib/secrets-api.ts`
    is its client (change both together). `crates/roll-secrets` is the store, crypto and router
    (`/v1/secrets`); `crates/rolld/src/secrets.rs` mounts it and implements the link providers;
    `roll secrets …` and `roll run` are `crates/roll-cli/src/secrets.rs`. Runtime clients:
    `packages/secrets` (TypeScript) and `sdk/go/secrets` (Go, standard library only); keep their
    surfaces the same, and a sealed box test vector from one opening in the other.
  - Values are encrypted with the project's data key, sealed by `ROLL_SECRET_KEY`, and bound to
    their row (project, secret, version) as associated data. A value never goes in a log, a
    `Debug`, an error, an audit row, a change's or request's diff, or a command line: `Value`
    redacts itself, and containers get env values by name (`code-preview/src/docker.rs`).
  - Only the `reveal` routes return values. Every read of values and every write is audited.
    Anything that touches a second environment (a reference, an import, a base, a promote's
    source) checks the caller through `Caller::allows`; checks, sandboxes and previews resolve
    untrusted (`Secrets::untrusted`, `"untrusted": true`): never a protected environment.
  - Signed-in people manage their own projects' secrets (`OwnProjects`; others' projects 404). A
    canvas proves nothing about who owns what's on it: anything acting on a resource through a
    project asks `roll_workspace::Workspace::trusts` (secrets' `members`, `counts_for_repo`, roll
    previews), never membership alone. What a person made (tokens and their tokens, identities,
    syncs) acts only while they can sign in and still reach the project (`maker_reaches`).
  - Workload identities (`identities.rs`) log CI in with its own OIDC token: an issuer is a URL a
    tenant chose, fetched only through rolld's guarded client (`Deliver::get`), and a shared issuer
    (GitHub, GitLab) needs a subject that names one repo.
  - roll-secrets knows no product: a resource is linkable through a `LinkProvider` that rolld
    registers, calling the product's own routes in process. A product's credential is never its
    shared password when it can issue keys of its own. Issuing and revoking happen outside the
    database transaction; a write closure only touches the database.
  - Read-then-writes are one `code_sql` write (SQLite or the system database's `secrets` schema,
    shared by every node); background work claims each item in a write before acting on it.
  - Tests: flows in `crates/roll-secrets/tests/api.rs` (also on Postgres with
    `ROLL_CODE_TEST_DATABASE`), two nodes in `tests/nodes.rs`, the CLI in
    `crates/roll-cli/tests/secrets.rs`, dashboard logic in `apps/dashboard/src/lib/secrets-*.test.ts`.
    No test calls a real product or platform: use fake providers.
- **Agents (roll code agents: the agent harness on a canvas):**
  - The design is in `docs/code/agents.md`. It works like T3 Code (MIT; behavioural reference
    for threads, runs, the execution-node tree, requests, checkpoints, forks, handoffs and
    delegation), with every backend piece in Rust and the UI on a canvas in the dashboard.
  - `crates/agent-proto` is the wire (no I/O). `crates/agent-host` is the daemon behind
    `roll agent up` (orchestrator, SQLite store, git worktrees and checkpoints, PTYs, the
    orchestrator MCP, the relay link, and the providers in `provider/{codex,claude,acp}`).
    `crates/code-control/src/agents/` is the relay and `/v1/code/agents` (machines, device login,
    the mirror, SSE, commands, grants, sandboxes, shares). `apps/dashboard` has them in the
    header of every page, opening as a popup (canvas, conversations, new agent; no nav page), and
    filling the window at `/dashboard/agents`; `apps/desktop` is the Electron shell (roll in its
    main window, the agents in an Agents window).
  - The daemon owns the truth for its machine; the relay only mirrors full records (upserts keyed
    by id, newest `seq` wins) and forwards commands. Never make the relay decide thread state.
  - Change a wire type in `agent-proto`, then regenerate the fixtures
    (`ROLL_FIXTURES=write cargo test -p agent-proto`) and mirror it in
    `apps/dashboard/src/lib/agents-api.ts`; the fixture tests on both sides fail until they agree.
  - Command ids are idempotency keys: a daemon never runs one twice. Commands that write records
    reply only after those records are stored.
  - Adapters own no roll ids: they turn the provider's native protocol into `ProviderEvent`s and
    are tested by replaying recorded transcripts through a fake binary. No test calls a real model
    or needs a provider login.
  - The daemon runs the user's own installed provider CLIs with their own logins. Never read, copy,
    relay in the clear or pool a provider credential (no subscription proxying). Endpoints (a base
    URL and a key: CLIProxyAPI, a gateway) can be set from roll per machine (docs/code/agents.md,
    "Endpoints"): the browser seals the key to that daemon's `seal_key` (`lib/agents-seal.ts`), the
    relay only checks the box's shape and forwards it (never stores, logs or returns a key), and only
    the daemon opens it, keeps it in `agent.db` (0600) and gives it to the processes of the providers
    that use the endpoint (never terminals, records or `hello`). Owner endpoint profiles on the relay
    never hold a key, and nothing is shared across owners. Keys may also come from `config.toml`, a
    sandbox's sealed env, or a 1Password reference the daemon reads on the machine.
  - Machine tokens (`rlm_…`) only open the relay link; code access is a per-command, one-repo,
    short-lived grant (`rlg_…`). Both are looked up on every use, never cached.
  - Checkpoints use a private `GIT_INDEX_FILE` under `refs/roll/checkpoints/`: never touch the
    user's index, HEAD or working files outside a rollback the user asked for.
  - Canvas motion follows the rules above: one canvas engine, calm accent pulses only on busy
    work, reduced motion honoured.
  - The agents surface has its own design system, `docs/code/agents-design.md` (the user's
    request, overriding "quieter, no shadows" inside it only):
    - Linear's inverted L: chrome flush on the base, the canvas an inset panel, a floating sheet.
    - A near-black ladder, with tokens under `[data-agents]` in globals.css.
    - Translucent hairlines with a top highlight, and deep shadows on what floats.
    - Use its tokens (`bg-canvas`, `bg-sheet`, `shadow-pop`, `inset-shadow-highlight`, `glass`…),
      never raw colours or shadows.
    - The accent stays `#4c8dff`. No orange, no decorative gradients. The Horizon mark is in the
      header.
    - The conversation (transcript, work log, composer, requests) behaves and moves like T3
      Code's chat (popups scale from their trigger, disclosures animate their height, sheets
      slide in, 150–300 ms, reduced motion honoured); its components name the T3 files they
      follow.
  - The gateway (docs/code/gateway.md): `crates/model-gateway` is roll's model gateway (Messages,
    Chat and Responses in; the owner's API-key upstreams out; translation, failover, client keys,
    usage). One machine's daemon runs it in-process (`agent-host/src/gateway.rs`), managed only by
    `gateway.*` commands (no management HTTP API); members join through the relay
    (`code-control/src/agents/gateway.rs`) with a client key sealed straight to them. Never: a
    consumer-subscription login as an upstream, a client presented as another, or another key of
    the same provider after a 429. Every place a machine uses it shows `GatewayNotice` (its
    requests go out from the gateway's computer).
  - Chat ops (docs/code/chatops.md): `crates/code-chat` is the Slack and Discord wire (no roll
    types); `crates/code-control/src/chatops/` holds installs, the action queue, bindings, and the
    `slack`/`discord`/`github` triggers and steps. A platform's request is verified (signature and
    timestamp), then recorded in `chat_actions` by the platform's own id, before it's acknowledged.
    Answers and steers act as the automation that bound the chat thread, through the API
    (`Code::automation_api`), never by calling the relay directly. Only the install's members may drive
    roll from a workspace. GitHub comments count only from `OWNER`/`MEMBER`/`COLLABORATOR`, and a
    fork's pull request counts only after `approve-run`. Tests: `code-control/tests/chatops.rs`,
    `github.rs` (`automations_*`), `roll-cli/tests/agent.rs::a_slack_button_answers_the_fake_providers_approval`.
  - After touching anything agents-related, run `pnpm test:agents` (its Rust, web and end-to-end
    parts each skip themselves when nothing they cover changed; the end-to-end run builds the
    dashboard only when the dashboard changed; `--unit` skips the end-to-end
    run). New tests (see the rule above: one per behaviour nothing else covers) go in
    `agent-host/tests/{orchestrator,threads,git,terminal,mcp,relay,codex,claude,acp}.rs`,
    `code-control/tests/{agents,agents_code}.rs` (+ `routes.rs` for the auth matrix),
    `roll-cli/tests/agent.rs`, `apps/dashboard/src/**/agents-*.test.ts(x)` and
    `apps/desktop/src/main/*.test.ts` (fakes in `apps/desktop/test/`); map them in `docs/code/test-coverage.md`.
- **Storage (S3-compatible):**
  - `crates/s3-proto` is the wire protocol with no I/O: SigV4, SigV4a and SigV2 (headers, presigned, aws-chunked with trailers, POST policies), SSE headers, XML, checksums, ranges and conditionals, S3 error codes.
  - `crates/s3-store` is the metadata (SQLite plus a durable log in object storage), branching, refcounts, trash, lifecycle, keys, metering, the blob backend (`aws-sdk-s3` or a local directory) and the GC/scrub jobs. It has no HTTP.
  - `crates/s3-server` is the S3 HTTP API. `crates/s3-control` is `/v1/storage`, used by `roll storage …` and `/dashboard/storage`. `crates/s3-select` is S3 Select's SQL and CSV/JSON engine (no I/O, bounded memory).
  - Directory buckets (S3 Express) are ordinary roll buckets named `<base>--<zone>--x-s3`; appends write a new blob (old bytes copied server-side, then the new ones) and commit it only if the object didn't move. Renames move a blob's reference in one transaction.
  - The design is in `docs/storage/architecture.md`. Reliability comes first; keep these invariants:
    - One customer object is one backend blob. Never pack objects together, and never compact or rewrite a blob.
    - Blobs are immutable. Every upload gets a fresh blob id; never reuse one.
    - Acknowledge a write only after its metadata log commit is in object storage. A failed commit fails the writes in it and fences the node.
    - Never delete a blob that any `objects`, `versions` or `trash` row references. A version under Object Lock is never deleted for good. Deletes go to the trash (`recover_days`), then unreferenced blobs wait in a 24 h quarantine. The orphan scan only removes blobs with no metadata, no active upload and an age over 24 h. The scrub only reports.
    - Writes to a bucket with branches push the old state down to the branches in the same transaction.
    - Never size an allocation from a count or length a peer sent. Stream bodies, and cap XML bodies.
    - SSE-C and SSE-KMS are the backing object store's: pass them through on the blob's requests, never re-encrypt bytes on S3, and never store a customer key (only its MD5). An encrypted write the backend doesn't confirm fails and keeps nothing.
    - A change S3 can notify about records its events in its own transaction (`events::emit`), and the dispatcher only delivers rows `Store::due_events` returns (they're durable). Replication between roll buckets shares blobs (a new reference), never copies them; replication to a target outside roll copies the bytes and the target must match the blob's CRC32C before the version counts as replicated. A replication target's secret is only ever stored sealed.
    - Temporary credentials (STS) are never stored: a session token is AES-256-GCM-sealed claims under the deployment key in `state` (`s3-store/src/sts.rs`). Never log or `Debug`-print a secret, session token, JWT or MFA seed; never echo a token in an error. A session never gets more than its role's identity policies allow within its boundary and session policies, except what a resource policy grants the session itself (AWS's rule). Only fetch OIDC keys from issuers in a role's trust, over HTTPS, with timeouts and size caps.
    - Accounts and IAM (`s3-server/src/{identity,access}.rs`, `iam/`, `s3-store/src/{accounts,iam}.rs`) evaluate exactly as AWS does: explicit deny wins; same account, identity OR resource grant; cross account, both. Roll permission levels act as attached policies. An MFA code is accepted once (the device's last step is stored before the request goes on).
    - Anything that decides access goes through that one evaluator (`Identity::verdict`, `Policy::evaluate`, `access::combine`): the policy simulator and the dashboard's signed-in requests too. Never copy it.
    - Principals in resource and trust policies must exist when put, and are kept with their unique ids (`principals.rs`): read and evaluate a policy through `principals::rewrite`, so a deleted principal matches nobody.
    - Passwords are only ever stored as argon2id hashes (`password.rs`); never log, echo or `Debug`-print a password or its hash. Failed MFA codes and passwords are counted durably with relaxed writes (`s3-store/src/throttle.rs`); only count attempts at things that exist.
    - SAML (`sts/saml.rs`, `s3-proto/src/xmldsig.rs`): no DTDs or entities; exactly one assertion, a child of the response; a signature's reference must name its own parent by a unique ID, with exactly enveloped-signature then exclusive c14n; keys come only from the provider's metadata. Read the assertion from the node that was verified.
    - A dashboard session (`console.rs`) is a bearer token of an IAM user's console session only; every control-plane operation it makes is authorized as the S3 action it stands for, and an operation without one is the operator's.
  - After touching anything storage-related, run `pnpm test:storage`. It skips itself when nothing relevant changed and runs real S3 tools in Docker (`amazon/aws-cli`, `rclone/rclone`, MinIO's `mc` from `bitnamilegacy/minio-client`, `d3fk/s3cmd` for SigV2).
  - `pnpm test:storage-aws` runs `crates/s3-server/tests/aws.rs` against LocalStack in Docker (no AWS account): SSE-KMS with the backing bucket on LocalStack, notifications to SQS/SNS/Lambda/EventBridge, replication to buckets outside roll, Object Lambda. Run it after touching SSE, notifications, replication targets or Object Lambda.
  - `pnpm test:s3-conformance` runs the S3 conformance suite (ceph/s3-tests, pinned) in Docker against a fresh server (`crates/s3-server/examples/serve.rs`) and compares the outcome with `tests/s3-conformance/expected.txt`, which lists every test known not to pass and why. It fails on a regression and on an unexpected pass: after fixing something, remove its lines. Run it after changing S3 behaviour; it skips itself when nothing relevant changed. `pnpm test:s3-conformance-localstack` runs it with the backing bucket on LocalStack S3 and KMS (SSE-KMS, `ROLL_S3_SSE_S3_AT_REST`); a line whose category ends in `@local` or `@localstack` only applies to that backend.
  - On many nodes (`s3-server/src/cluster.rs`, docs/storage/architecture.md "Many nodes") only the holder of the lease `storage:meta` opens the store; every node's S3, developer-API and `/v1/storage` routes forward to it, and its background jobs run only there. A forward carries the client's path, query and `Content-Length` untouched (SigV4 signs them) and never buffers a body past roll-cluster's 4 MiB. Takeovers start from a standby's `Follower` copy; fencing stays the log's create-only entries, never the lease.
  - The gateway cache (`s3-server/src/cache.rs`) is keyed by blob id and chunk: never invalidate it or add TTLs for correctness. The CDN (`s3-server/src/cdn.rs`, `s3-store/src/cdn.rs`, the Worker in `apps/cdn-worker`) purges through the outbox. After touching either, run `pnpm test:storage-cdn` (the Worker in workerd/Miniflare in Docker, purges to a mock Cloudflare API).
  - Put new tests in the matching place:
    - protocol vectors and parsers: unit tests in `s3-proto`
    - metadata, durability, branching and GC: `crates/s3-store/tests/`
    - the S3 API through the AWS SDK: `crates/s3-server/tests/` (notifications, replication and inventory: `tests/events.rs`)
    - real tools: `crates/s3-server/tests/tools.rs` (through a node that doesn't own the metadata)
    - several nodes (forwarding, takeover, handoff, fencing, events across a takeover): `crates/s3-server/tests/cluster.rs`
    - `/v1/storage`: `crates/s3-control/tests/`
  - Set `ROLL_E2E_S3_BUCKET` (plus `ROLL_E2E_S3_ENDPOINT` and AWS credentials) to run the store and server tests against S3.
- **NoSQL (ScyllaDB / Cassandra compatible):**
  - The design is in `docs/nosql/`. Start with `contracts.md`.
  - The target is 1:1 client-visible behaviour with `scylladb/scylla:2026.3.3` and `cassandra:5.0.9`. Each database has a dialect (`scylla` or `cassandra`); where the two products differ, gate on `Dialect`, never on a guess.
  - `nosql-proto`, `nosql-cql` and `nosql-storage` don't depend on each other. `nosql-engine` joins them, and `nosql-server` / `nosql-control` sit on top.
  - You may read Apache Cassandra's source (Apache-2.0) and port it, naming the Cassandra class in the module doc. Never read ScyllaDB's source; learn its behaviour from its docs and the image.
  - ScyllaDB's `test/cqlpy` and `test/alternator` suites are downloaded at test time by `scripts/nosql-conformance.sh`. Never commit their content; only test ids go in the baselines.
  - After touching anything nosql-related, run `pnpm test:nosql`. A conformance test that passes on the reference must pass on roll, and the baseline may only shrink.
  - Put new tests in the matching file: differential scripts in `crates/nosql-server/tests/compat/*.cql`, drivers in `tests/drivers.rs`, persistence in `tests/storage.rs`, `branches.rs` and `restore.rs`, several nodes sharing databases in `crates/nosql-control/tests/cluster.rs`.
  - On a fleet a database is loaded only on the holder of its lease `nosql:db:<id>`, and every manifest write is a compare-and-swap (`nosql_storage::manifest::swap_manifest`): never `put` a manifest. The design is "On a fleet" in `docs/nosql/architecture.md`.
  - SSTable backup imports: format parsers are unit-tested in `crates/nosql-sstable-import`, imports end to end in `crates/nosql-control/tests/import_sstables.rs`. Its fixtures are made by `tests/fixtures/generate.sh` on the references (`imp_` keyspaces; `cleanup.sh` drops them and their snapshots).
- **Search (Elasticsearch / OpenSearch compatible):**
  - The design is in `docs/search/` (start with `README.md` and `architecture.md`; cross-crate
    interfaces in `contracts.md`). The target is 1:1 client-visible behaviour with Elasticsearch
    9.5.5, plus an OpenSearch 3.9 dialect: gate on `Dialect`, never on a guess.
  - `search-analysis`, `search-codec`, `search-index`, `search-aggs` and `search-painless` are the
    engine (no I/O); `search-store` is object storage (WAL, segments, manifests, branches, GC);
    `search-es` is the REST API; `search-server` the multi-tenant node; `search-control`
    `/v1/search`, metering and the fleet; `search-yaml` the YAML REST runner.
  - On a fleet a cluster is loaded only on the holder of its lease `search:cluster:<name>`
    (`search_control::cluster`), and every request for it runs there (`Cluster::dispatch`). Writes
    are fenced by the ownership markers in object storage (`Store::claim_next`), never by the
    lease's epoch, and acknowledged once their WAL batch is in object storage. A call about one
    cluster under `/v1/search/clusters/{id}/` runs on its owner; put new ones there.
  - Elasticsearch's YAML suites and module sources are downloaded at test time into `target/`
    (they aren't Apache-licensed): never commit them. Only test ids go in
    `crates/search-yaml/baseline/`, and a pass count only goes up (`docs/search/compatibility.md`
    holds the minimums).
  - After touching anything search-related, run `pnpm test:search` (it skips itself when nothing
    relevant changed; `--unit` skips Docker). Put new tests in the matching place: engine
    behaviour as unit tests in its crate, REST behaviour as fixtures checked against Elasticsearch
    (`search-es/tests/`), the node in `search-server/tests/`, `/v1/search` and the fleet in
    `search-control/tests/{control,cluster}.rs`, the CLI in `roll-cli/tests/search.rs`.
- **Local state:**
  - `.roll/` holds SQLite and branch data. Never commit it.
  - Test Postgres with `docker run --rm postgres:18-alpine psql "postgres://...@host.docker.internal:55432/main?options=endpoint%3D<ep>"`, because there is no local psql.
