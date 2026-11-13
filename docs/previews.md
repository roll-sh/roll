# roll previews

A preview is a full copy of a workspace project for one change: every service deployed at the
change's head, a branch of every data resource, and a secrets environment wired to the
branches. It costs close to nothing while nobody looks at it.

- Code: `crates/roll-previews` (the store, the reconciler, the router and the routing of preview
  hosts), mounted by `crates/rolld/src/previews.rs`. rolld also holds each product's branch
  adapter (`Brancher`), because only rolld has every product's control plane.
- Client: `apps/dashboard/src/lib/previews-api.ts` (change it and this contract together),
  `roll preview …` (`crates/roll-cli/src/preview.rs`).
- Tests: `crates/roll-previews/tests/` (the reconciler and the router in process, with fake
  products), `crates/roll-cli/tests/preview.rs`, `apps/dashboard/src/**/previews-*.test.ts(x)`.

## How others do it

| Product | A preview is | Data | Idle cost | Cleanup |
| --- | --- | --- | --- | --- |
| Vercel | The frontend at a URL per commit and per branch, plus a PR comment and a deployment status. | Not included. It points at shared databases, or at a Neon branch through an integration. | Functions scale to zero. | Retention policy. |
| Railway PR environments | The base environment's services duplicated per PR. "Focused" environments deploy only what the PR touches. | New, empty database services, or a template. | Every service runs and bills by the minute, unless app sleeping is on. | When the PR closes. |
| Render preview environments | A `render.yaml` blueprint per PR, with plan overrides and an `initialDeployHook` that seeds. | A fresh, seeded instance per preview. | Full instances for the preview's life. | Close, or `expireAfterDays`. |
| Neon | A copy-on-write Postgres branch from any point in time. | Copy-on-write. | Compute stops after 5 minutes; storage is billed for the branch's changes only. | Branch expiry, or the API. |
| Supabase branching | A branch per PR that runs the repo's migrations and `seed.sql`. | No production data. | A full instance per branch. | When the PR closes. |
| Heroku review apps | `app.json` add-ons and a `postdeploy` script. Config vars are inherited from the pipeline. | New, empty add-ons. | Dynos (eco dynos sleep); add-ons bill from creation. | Stale after N days, and on close. |
| Qovery | An environment cloned per PR on your Kubernetes cluster. | Container databases cloned empty or seeded. | Start/stop schedules and an inactivity stop. | When the PR closes, and a TTL. |

Every one of them either leaves the data out, or copies or seeds it per preview and pays for
idle instances. Only Neon branches cheaply, and only Postgres. roll previews:

- **Branch every product, cheaply.** Postgres, Redis, buckets and NoSQL branch from their
  parent, and so do streams, queues and tasks (see [Branches](#branches-per-product) for how
  each one branches; search joins when it lands). A branch of a 100 GB project starts in
  seconds and stores only what the preview writes, except where the table below says
  otherwise.
- **Put the whole preview to sleep, not just the app.** Services stop after `sleep_after`
  without a request. Postgres computes, Redis, NoSQL, queues and search unload on their own
  after 5 minutes idle. A sleeping preview pays only for bytes. The first request to any of its
  URLs wakes it and waits for it.
- **Use real data, safely.** Branches come from the resources in the project (production or
  staging), as `branch` (everything), `schema` (structure, no rows) or `skip`. Production secrets
  are never inherited unless the project allows it. Fork pull requests need `approve-run`.
- **Keep one record for every surface.** The same preview drives the roll code change, the
  GitHub PR (a comment that updates in place, a commit status and a deployment), the
  dashboard's Previews tab and `roll preview`. Each shows the cost.
- **Converge instead of scripting.** A preview is desired state plus observed state, and one
  reconciler moves the second towards the first. A crash halfway through 8 branches carries on
  where it stopped and never leaves an orphan.

## Lifecycle

```
                   ┌──────────── push (head moves) ─────────────┐
                   ▼                                             │
 change opens ─▶ creating ─▶ ready ─(no requests for sleep_after)─▶ sleeping ─(request)─▶ ready
 PR opens        │  ▲          │                                     │
 roll preview    │  └─ reset ──┤                                     │
 create          ▼             ▼                                     ▼
              failed ◀── a step failed (retried with backoff; the error is on the preview)
 fork PR ─▶ awaiting_approval ─(approve-run on that head)─▶ creating

 landed / merged / closed / TTL / DELETE ─▶ deleting ─▶ (gone)
```

- **Creation**: the reconciler watches the open changes of every repo in a project with
  previews on. That covers roll code changes, and GitHub PRs, which are changes with
  `github_pr`.
  - A change gets a preview when the project's rules match it: its branch matches `branches`
    (globs, `*` by default), it is not a draft unless `drafts`, and it is trusted or its head is
    approved.
  - With `paths` or `ignore_paths` set (gitignore-style patterns, as CODEOWNERS and
    `.roll/review.yaml` read them), a change also has to touch a path that `paths` matches (any
    path, when it's empty) and `ignore_paths` doesn't: Render's build filters, Vercel's ignored
    build step. A change's paths are everything since its merge base with the default branch,
    as reviews read them, so a change that later only touches ignored paths loses its preview.
    Negation (`!pattern`) is refused; narrow `paths` instead. A path that can't be read counts
    as a match, so one change's diff never holds up the project's other previews. A preview made
    by hand (`roll preview create --change`) stays whatever the rules say.
  - `manual` (Render's manual mode) turns that around: only a change whose title asks with
    `[preview]` gets one.
  - The change's title can say otherwise (Render's `[skip render]`): `[skip preview]` (or
    `[no preview]`) skips it, and deletes the preview it has; `[preview]` asks for one the
    `branches`, `drafts` or path rules would leave out. A fork still waits for `approve-run`. GitHub
    pull requests carry their title into the change, so the same words work there. So do labels:
    `no-preview` skips and `preview` asks (a GitHub pull request's, or a roll change's through
    `PATCH /v1/code/changes/{id} {labels}`); a skip, from the title or a label, wins.
  - `roll preview create --repo acme/web --branch feat-x` (or `POST /v1/previews`) makes one for a
    branch with no change. It lives until it is deleted or its TTL passes.
- **Update**: every new head redeploys the services of the change's repo at that head. Services
  from the project's other repos run at their own branch of the same name when they have one (a
  feature split across a front end and a back end previews as one, as Signadot, Bunnyshell and
  Qovery do), else at their default branch's head, and are redeployed only when that head moves.
  This is Railway's "focused" environment, done the cheap way. Data
  branches stay (they hold the preview's own writes) unless the change resets them.
  - With `ignore_paths` set, a push whose every changed path (since the head the services run)
    is one it matches, and isn't `roll.toml`, redeploys nothing (Render's skipped deploys): the
    preview stands for the new head (its status, comment and `roll preview wait`) and says which
    one it `runs`. `paths` alone never skips a deploy: another service of the repo may run what
    it leaves out. The next push that touches anything else, measured from what runs, redeploys.
- **Earlier pushes** (Vercel's per-push URLs): when a change's own service is redeployed at a new
  head, the head it ran stays reachable at `<service>-<head>-<slug>`, on the preview's same data,
  behind the same link, asleep until opened; the last 3 per service (an older one's container
  goes). For comparing before and after. A trusted change's only (an approval is for one head),
  `roll.toml` services only (roll apps runs its own), and old code on migrated data can break.
  They're on the sheet, `roll preview show`, the change and the pull request comment.
- **Warm after a push**: once a new head is deployed, the change's own services are woken (after
  the pass, a few at a time), so the first visit, usually the pull request comment's link,
  doesn't wait for a cold start; they sleep again after `sleep_after` unused. Not a preview
  someone put to sleep, and only on the node that holds its lease. About $0.002 a push here;
  `warm: false` turns it off.
- **Sleep**: a service with no requests for `sleep_after` (10 minutes by default) stops. A
  request to any URL of the preview wakes it. The request is held for up to 60 s; after that the
  visitor gets a "starting" page that refreshes. Data products sleep on their own and wake on
  connect.
- **Reset**: `reset_every` (`0` off, or hours) re-branches every data resource from its parent's
  current state on a schedule. `POST /v1/previews/{id}/reset` does it now, for all resources or
  the ones named.
- **Deletion**: when the change lands or closes, or after `ttl` without a push or a visit
  (7 days by default). `DELETE /v1/previews/{id}` deletes it at once. Deletion removes, in this
  order: services, the secrets environment, then every data branch. Each step is idempotent, and
  the preview row goes last, so a crash restarts the deletion.
  A day before a preview goes (its TTL, its keep or a branch preview's fixed expiry running out;
  a quarter of a shorter TTL ahead), the change, its pull request comment, the sheet and its
  activity say when, and how to keep it; using it again takes the warning back when using it
  pushes the time back.
  A change's preview deleted by its TTL or by hand stays deleted until the change's next push
  (`dismissed(change_id, head)`).
- **Limits**:
  - `max_previews` (10 by default): previews beyond it wait as `queued`, oldest change first,
    until one is deleted.
  - `max_usd_month` (no cap by default) is the project's monthly budget for previews. Once what
    they've cost this month reaches it, new previews wait as `queued` (saying why), and the
    running ones carry on. Previews deleted this month still count (`spend`, a per-project,
    per-month ledger). When the month's cost passes 80% and then 100% of it, each of the
    project's repos gets a `budget` event (webhooks, the event stream and automations) with
    `{budget: {project, project_name, percent, spent_usd_month, max_usd_month}, dashboard_url}`,
    once per level per month and budget (raising the budget alerts afresh). It's recorded after it's queued, so a
    crash repeats one rather than losing it, and every live preview's activity says so.
  - `max_bytes` (10 GiB by default) is a cap on the bytes a preview's branches own. A preview
    over the cap goes to sleep with `error`, and resetting it brings it back under.

### Converging

Every preview row holds the desired state (`active`, or `deleted` once it should go), what was
made so far (each resource's branch id, each service's deployed revision), and the observed
state. A pass of the reconciler:

1. **Discovers.** For each project with previews on, it reads the open changes of its repos
   (`code.db`), adds a preview for each change that matches the rules and has none, and marks
   `deleted` the previews whose change landed or closed, or whose TTL has passed.
2. **Converges each preview that isn't settled**, in order. Each step finds what exists before it
   makes anything:
   1. A deleted preview runs its deletion (above).
   2. A fork's head that isn't approved stops the preview's services and waits as
      `awaiting_approval`. Its data branches stay.
   3. Every data resource without a branch is branched, as `pv-<preview id>` on its parent. The
      name is the idempotency key: the adapter looks the name up under that parent first, and
      checks that it is a child of that parent. A crash between "branched" and "recorded" finds
      the branch the next time.
   4. A due reset runs.
   4b. On fresh data (anything just branched or reset), the project's `seed` runs once:
       `sh -c <run>` in `<image>`, in the change's tree, with the preview's secrets (resolved
       as services get them) and the branches' variables on top, like a check. Use it to anonymise production rows, run migrations or load fixtures. It runs
       before services deploy; a failure fails the preview with the output's tail, and is
       retried.
   4c. At each new head (and again on fresh data), the project's `release` runs the same way:
       migrations, like Heroku's release phase or Render's `preDeployCommand`. Services deploy
       only after it succeeds; a failure fails the preview and leaves its services on what they
       ran.
   4d. Once per head, the Postgres branches' schema is compared with their parents' (see
       "Schema changes").
   5. The secrets environment `pv-<preview id>` is ensured (base, overrides, resource URLs).
   6. Every service whose revision is not the one it should run is redeployed (the change's head
      for its repo; for the others their branch of the same name, else their default head).
   7. The preview becomes `ready` (or `sleeping`), and the change and the PR hear about it.
   Every write that asks for a converge (a push, a delete, a reset, a refresh) bumps the row's
   `generation`. A converge settles a row only if its generation hasn't moved since it started,
   so a delete or reset that lands mid-converge is never lost. A branch the product made whose
   id never got recorded (a crash, a timeout) is found by its name when the preview is deleted.
3. A step that fails records its error on the preview (`failed`) and is retried with backoff
   (15 s × 2ⁿ, at most 10 minutes). Nothing else is undone.

The reconciler runs every 15 s, and right away when roll code emits a `change` or `landed`
event (the broadcast is only a nudge, so a dropped event costs at most one interval). In a
cluster, discovering previews is one node's job (`singleton("previews")`). Each preview has a
lease of its own (`preview:<id>`), placed by rendezvous so previews spread over the nodes, and
the lease's holder converges it, runs its services and serves its hosts. The other nodes
forward its hosts, and its `sleep`, `wake` and `logs`, to that holder, and a node that loses a
preview's lease stops its containers. Only the reconciler writes a preview's resources and
services. The API writes only the desired state (and asks the reconciler to run).

## Branches per product

Branch names are `pv-<preview id>` (a bucket's is `<parent>-pv-<preview id>`, since bucket names
are global). Branch adapters live in `crates/rolld/src/previews.rs` and implement
`roll_previews::ports::Brancher`. They call this node's own API over loopback with the operator
token, like `roll` does, so each request runs where the product keeps the resource (a Redis
database's or a stream's lease, storage's metadata owner) wherever the reconciler runs.

| Kind | Branch | Reset | Sleeping cost | Notes |
| --- | --- | --- | --- | --- |
| `postgres` | `pg_control::Control::create_branch` from the project's `main`. A running parent is copied by the file system between `pg_backup_start` and `pg_backup_stop` (`cp --reflink=auto`): blocks are shared on XFS and btrfs (the fleet's gp3 volumes are XFS), a plain copy on ext4 | In place (`POST /v1/projects/{id}/branches/{branch}/reset`): the same branch, endpoint and `DATABASE_URL`, its own history started again | Its local data directory until the preview has been idle an hour (then evicted), then its own base backup in object storage | Copy-on-write where the file system reflinks; a full directory on ext4 (this machine). |
| `redis` | `redis_control::Control::branch` | Delete and branch again | Nothing in memory; its manifest and the segments it pins | Copy-on-write in storage. Loading replays the parent into memory. |
| `bucket` | `s3_store::Store::branch_bucket` | Delete and branch again | The objects it wrote, plus the parent's writes pushed down to it | One row to create. Credentials are a write key scoped to the branch. |
| `nosql` | `nosql_control::Control::branch` (`schema_only` for `schema`) | `nosql_control::Control::reset`: same id, endpoint and credentials | The SSTables it wrote | Zero copy. Masking policies are inherited. |
| `stream` | `branch`: `POST /v1/streams/{s}/branches` on the parent's owner, a new stream with the parent's topics and configs whose partitions share the parent's chunks as of the branch (same offsets, nothing copied) and its committed group offsets. `schema`: a new stream with the parent's topics and configs, no records | Delete and branch again | The chunks it wrote (`own_bytes`; what it shares stays the parent's) | Zero copy. Retention, compaction and deletes on either side delete only objects no stream references, so a branch outlives its parent. A transaction open at the branch is aborted in it. See the broker's README, "Branches". |
| `queue` | A vhost with the parent vhost's definitions (exchanges, queues, bindings, policies), no messages | Delete and branch again | Nothing (a vhost suspends after 5 minutes) | A branch never holds production messages. |
| `tasks` | A new tenant with no messages and no schedules | Delete and create again | Its own SQLite file | Copied schedules would call production endpoints, so they never are. |
| `search` | `search_control::Control::branch` (zero copy) | Delete and branch again | Segments it wrote | Joins when roll search lands (`feature/serverless-search`). Its adapter is behind the same trait. |

Safety rules every adapter follows:

- **It never deletes the parent.** `delete` deletes only an id the preview recorded. Before
  deleting, it checks that the id is a child of the recorded parent and has the preview's branch
  name. Anything else is refused and reported.
- **It branches only resources in the preview's project.** A preview can't name a resource
  outside its project.
- **No connection string is stored where the product can give it again.** Postgres, Redis and
  NoSQL URLs and passwords are read from the product each time a service starts, so a rotated
  password reaches the next start. Streams, queues, tasks and bucket keys show their secret only
  once: the adapter keeps it with the branch (`resources.kept`), never returns it from the API,
  and mints a new one if a crash lost it.

## Services

A service is something the preview runs and serves over HTTP. There are two backends, both
behind the `Apps` trait:

- **roll apps** (`feature/roll-apps`, docs/apps/). When the project has roll apps services, they
  are the preview's services.
  - Each gets an ephemeral environment `pv-<preview id>`: `PUT …/environments/pv-…`
    `{branch: null, ephemeral: true}`.
  - The branches' URLs and the project's overrides are set as its variables.
  - It is deployed at the change's git commit (or its image, for an image service), with an
    `Idempotency-Key` of `pv-<id>@<sha>`, so a retry after a crash returns the same deployment.
    When the environment already runs that commit (a reset's new URLs), it's redeployed: the
    same image with the new variables, no build.
  - roll apps injects the roll secrets environment of the same name, builds once, scales to
    zero, and answers on its own hostnames (`<slug>.up.<site>`). A preview's service URLs are
    those.
  - Deleting the preview deletes the environments.
- **`roll.toml` containers**, when the project has no roll apps services. The services are the
  project's repos whose `roll.toml` has a `[preview]` section, named after the repo
  (`[preview] name = "api"` overrides that). Each runs in the container `roll.toml` describes,
  through code-preview's `Isolation`: Docker here, a Fargate task on the fleet.
  - With `health = "/healthz"` in `[preview]`, a service counts once it answers 2xx/3xx there
    (Render's and Railway's health checks), asked once per deploy (it wakes the service once). A
    service that answers 500 fails the preview with the status and the first line it said, so
    `roll/preview` and `roll preview wait` don't pass on a broken page; the retry asks again.
    A converge waits for it a few seconds at most: one still installing or booting is asked
    again on a later pass (for up to 20 minutes), so no converge slot waits on a build. roll
    apps services are checked by roll apps.
- The tree is copied into the container, never bind-mounted.
- Each service answers at `<service>-<slug>.<domain>`.
  - `<slug>` is `pr<number>` for a GitHub PR, `c<number>` for a roll code change, or the branch
    name for a branch preview, then `-<project name>`.
  - Example: `web-pr482-shop.preview.roll.site`.
  - Labels are cut to 63 characters, and a clash gets the preview id's last 4 characters.
  - The domain is rolld's preview domain (`--code-preview-domain`), served on the API port like
    roll code's own previews.
- A service gets this environment, later entries winning:
  1. its `roll.toml` `[preview] env`;
  2. the resolved secrets environment (`pv-<preview id>`, inheriting its base);
  3. the project's preview `env` overrides;
  3a. the project's per-branch variables (`env_for`: a branch glob → variables, every glob that
     matches the preview's branch, in the globs' order; Vercel's per-branch preview variables);
  3b. the preview's own variables (`PUT /v1/previews/{id}/env`, the sheet, `roll preview env`):
     Vercel's branch-scoped variables. Setting them redeploys its services; names roll or the
     preview's branches set (`ROLL_*`, `PORT`, `DATABASE_URL`, …) are refused, so they can't
     point a preview at other data;
  4. the branched resources' URLs:

     | Resource | Variables |
     | --- | --- |
     | Postgres | `DATABASE_URL` |
     | Redis | `REDIS_URL` |
     | Bucket | `S3_BUCKET`, `AWS_ENDPOINT_URL_S3`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` |
     | NoSQL | `CASSANDRA_CONTACT_POINTS`, `CASSANDRA_USERNAME`, `CASSANDRA_PASSWORD` |
     | Stream | `KAFKA_BOOTSTRAP_SERVERS`, `KAFKA_SASL_USERNAME`, `KAFKA_SASL_PASSWORD` |
     | Queue | `AMQP_URL` |
     | Tasks | `ROLL_TASKS_URL`, `ROLL_TASKS_TOKEN` |
     | Search | `ELASTICSEARCH_URL` |

     With two resources of a kind, the first one takes the plain name and the others add
     `_<NAME>` (`DATABASE_URL_ANALYTICS`).
  5. `ROLL_PREVIEW`, `ROLL_PREVIEW_URL`, and `ROLL_PREVIEW_URL_<SERVICE>` for each service, so
     services find each other; `PORT`, `ROLL_REPO`, `ROLL_CHANGE` and `ROLL_CHANGESET`.
- **Shared images and caches.** Images are pulled once per node. Each repo has one package cache
  (a Docker volume `roll-previews-cache-<repo>`, at `/roll-cache`), shared by every preview of
  the repo. npm, pnpm, yarn, bun, pip, uv, cargo and go are pointed into it, so the second
  preview installs from what the first downloaded. The cache goes with the repo's last preview.

## Secrets

A preview's secrets are an environment of the project's secrets (`feature/roll-secrets`,
docs/secrets.md) called `pv-<preview id>` (an ephemeral environment, labelled `preview`), inheriting from `secrets.base` (`development` by
default) with the project's `env` overrides and the branched resources' URLs on top.

- **Protected values stay out of preview code.** A preview resolves its secrets as `untrusted`
  (roll secrets' flag): nothing of a protected environment resolves, whether directly, by
  reference, by import or through the base. The preview gets its own overrides and links. Only
  a project with `secrets.allow_production` and a trusted change resolve without it.
- **Production secrets are never inherited unless the project allows it.** A base named
  `production` or `prod` is refused unless `secrets.allow_production` is true, and a fork's
  preview never gets them, whatever the setting says.
- rolld's adapter talks to roll secrets' API (`feature/roll-secrets`). A project with no secrets,
  or a node without roll secrets, gives its previews no environment: a service then gets the
  overrides and resource URLs only.
- **The environment is untrusted itself** unless the project allows production and the change is
  trusted: it's made with `"trusted": true` only then (roll secrets refuses a protected base to
  an untrusted environment whoever reads it, so roll apps deploying it as `app:<service>` gets no
  more than the preview would). Turning `allow_production` on or off makes a new environment.
- **One idempotent call.** Each converge ensures it with `PUT …/environments/pv-…` (roll secrets'
  ensure: made if missing, `replace: true` so its own keys are exactly the overrides and branch
  URLs, a removed override doesn't linger; an unchanged repeat records nothing). A different base
  or trust answers 409, and the environment is made again.
- **It expires by itself.** `ttl_minutes` is a day, renewed by each converge (a preview converges
  every few minutes while it exists), so a teardown that never happened leaves it for a day at
  most; roll secrets then deletes it and revokes its leases.
- **Keys that don't resolve are said.** A reference to nothing, or to a protected value an
  untrusted preview may not read, leaves the key out (a check would fail on it); the preview's
  activity names those keys once, again whenever the set changes.
- The environment is deleted with the preview.

## Schema changes

A reviewer of a change with a migration wants to see what it did to the database without opening
the preview (Neon's GitHub integration posts a schema diff). Once per head, after the `release`
(or seed) has run, rolld reads each Postgres branch's catalog and its parent's through the
endpoints' query route: one line per table, view, sequence, column (type, nullability,
default), index, constraint, enum, function and extension in the user's schemas. The two sets
are compared (`roll_previews::schema::diff`), and `+ …`/`- …` lines, at most 200, go on the
preview. The pull request comment folds them under "Schema changes", the dashboard's preview
sheet and `roll preview show` list them. A catalog query is milliseconds; it runs only for projects
with a `seed` or `release` (what changes a schema before services run), and reading the parent's
starts its compute if it sleeps, once per push. Object names come from the change's own
migrations (a fork's too), so each line is made printable once, where the diff is made:
control characters escaped (a newline in a name can't fake a line, an escape sequence can't reach
a terminal), lines cut at 300 characters, 200 lines at most; the comment keeps them inside a fence
they can't close. A failure to read is
logged and read again on the next converge; it never fails the preview.

## Safety

- **Forks.** A change with `trusted: false` (a PR from a fork) gets a preview only for a head a
  writer approved (`approve-run`, roll code's rule). A new push un-approves it. Its services
  stop, and it waits as `awaiting_approval` with its data branches kept. This also fixes roll
  code's own previews: the container started for a visit now refuses an unapproved head.
- **Structure-only means structure-only.** `schema` is accepted only for kinds that can do it:
  Postgres, NoSQL, streams, queues and tasks. A `schema` rule for Redis or a bucket is refused.
  A structure-only Postgres branch is emptied offline, before it exists. A throwaway server with
  no network runs on the copied directory and, in every database but `template0`:
  - truncates every table, unlogged ones included;
  - empties materialized views;
  - unlinks large objects;
  - drops user mappings, so foreign tables reach nowhere.

  It then checkpoints, stops and removes the WAL from before the checkpoint. No endpoint,
  receiver or history ever sees the rows, and a reset does the same before its swap.
  With `data: "schema"` those kinds are left out unless a rule says `branch`, so a project that
  asked for no rows never gets a full copy by default.
- **Nothing serves data that isn't ready.** A reset first stops the preview's services. A
  service doesn't start while any of its preview's data is being branched, reset, masked or
  seeded. A mask that fails is retried before its branch counts as ready.
- **Rules reach existing previews.** Saving a project's rules makes every preview converge again:
  - a resource whose mode changed is branched again in its new mode;
  - services whose variables or access changed are redeployed.
- **Masked columns.** `masking` masks columns of a preview's branches each time one is made or
  reset, before anything serves it; the parent is untouched.
  - Postgres (`"postgres:<ref>": {"schema.table.column": rule}`): the columns are rewritten in
    the branch's `main` database. Rules: `null`, `redact` (`***`), `hash` (`md5` of a random
    per-run salt and the value), `email` (`user-<12 hex>@example.invalid`, from the same salted
    hash, so within a branch the same input gives the same output and joins on it still match),
    `fixed:<value>`. One transaction rewrites every masked column, with names and values quoted
    whatever they hold, `search_path` pinned to `pg_catalog, pg_temp`, `standard_conforming_strings`
    on, and the masked tables' user triggers switched off and then put back exactly as they were
    (`ALWAYS` and `REPLICA` ones included), so no audit trigger copies the old values into
    another table and nothing fires outward. Then each masked table's `VACUUM (FULL, ANALYZE)`
    (maintenance runs as the table's owner with a safe `search_path`), every materialized view
    refreshed (as its owner), and a checkpoint.
    - A masked branch is never rewritten once the preview's code has run in it: a reset, or rules
      that change, branch it again (a new endpoint, masked before any service is pointed at it).
      So nothing still connected reads the unmasked rows, and nothing the preview's code left in
      it (a trigger, a function) runs inside a superuser's rewrite.
    - Every statement roll runs as the superuser on a branch pins `search_path` to
      `pg_catalog, pg_temp`, and the schema diff reads the branch as the branch's own role:
      functions the preview's code planted (a `public.format`, a database-wide `search_path`)
      never run with the superuser's rights. Measured: a planted `public.format` that would
      make the role a superuser leaves it as it was.
    - A mask that fails (a rule a column's type can't take) fails the preview and is retried;
      nothing serves the branch meanwhile. Large tables can take long: a mask request has the
      adapter's 600 s budget.
    - **The preview's code never signs in as the superuser.** Each branch gets a role of its own
      from pg-control (`POST /v1/projects/{id}/branches {app_role: true}`, or
      `…/branches/{branch}/app-role` for one that has none; named as the branch, its own
      password; the branch then answers with an `app_connection_uri`): it owns the app's
      database, schemas and objects, so migrations and seeds work, but it isn't a superuser and
      holds none of the roles that read files or other sessions' statistics. So it can't read the
      branch's WAL (where the pages from before the rewrite are), files, or another database, and
      the parent's endpoint doesn't know it (the role is made on the branch, after the branch
      point, and made again, same password, after an in-place reset). The preview's
      `DATABASE_URL` is that role's; masking, the schema diff and resets run as the superuser
      through rolld's own query route, which a preview's code has no token for.
      `crates/pg-control/tests/e2e.rs` (`branches_are_isolated_copies`) checks it. The `VACUUM (FULL, ANALYZE)` after the rewrite also
      clears TOAST and recomputes the statistics the role can read for its own tables. Measured:
      the role reads and migrates its data, `pg_read_binary_file` and `pg_ls_waldir` are denied,
      and the parent's endpoint refuses it.
  - NoSQL: column masks (Cassandra 5's mask functions) added to the branch's read policy, on top
    of what it inherits.
  - Other products: a `seed` anonymises the rows instead.
- **Queues branch without what moves messages in.** Shovels and federation upstreams (and the
  `federation-upstream*` keys of policies) aren't copied, so a branch can't pull production
  messages or carry the production broker's credentials.
- **A fork's head is checked where it runs.** A converge deploys only when the change's head is
  still the head it read (else it converges again on the new one), and a container starts only
  at the head that was checked.
- **Production by name, conservatively.** Any base containing `prod`, or named `prd`, `live`,
  `main` or `master`, counts as production. Changing the base re-creates the preview's secrets
  environment on the new one.
- **Data.** A branch is a child of the parent. Deleting the branch never touches the parent (the
  products' refcounts and garbage collection guarantee it, and the adapter checks parentage
  before it deletes).
- **Production secrets** are covered above.
- **Private by default.** A preview runs on branches of real data, so with `access: "link"`
  (the default) its URLs answer only to a request that carries the preview's access token.
  - The links roll hands out (`share_url`: the change, the pull request comment, the dashboard,
    `roll preview show`) carry the token as `?roll_access=…`. A request with it gets an
    `HttpOnly`, `SameSite=Lax` cookie for that preview and is redirected to the same URL without
    the token, so the token doesn't stay in the address bar or in `Referer`.
  - Anything else gets a 401 page. It links "through roll": the project's Previews tab with
    `?preview=<id>&go=<service>`. A person signed in to roll is sent on to the service with its
    link, so a reviewer who comes from the pull request's plain URL gets in, and the token is
    never public. `access: "public"` opens a project's previews to anyone who
    can reach the domain.
  - The token is 120 random bits per preview, compared in constant time.
  - The token never goes where people outside roll can read it. The change, the pull request
    comment, statuses, deployments and repo webhooks get the plain URL (which answers 401 to
    anyone without the link). The dashboard and `roll preview` hand out `share_url`.
  - The redirect only ever goes to a path on the preview's own host.
  - **Automation** (end-to-end tests in CI, Vercel's "Protection Bypass for Automation"): a
    project can have one bypass secret (`roll preview bypass <project> --rotate`, or the rules'
    Automation field). A request with it in the `x-roll-preview-bypass` header reaches any of
    the project's private previews, with no cookie or redirect. The service never sees the
    header. The secret is shown once, kept only as its SHA-256, and compared in constant time.
    Rotating it stops the old one at once. A fork's preview (`trusted: false`) never opens to
    it: CI that visited one would hand the secret to the fork's code. Send the header only to the
    preview's own origin (`page.route` on the base URL, never `extraHTTPHeaders`, which a page can
    send anywhere). CI gets each preview's plain URL from the GitHub deployment
    (`deployment_status`), so the secret is the only thing it has to keep:

    ```yaml
    on: deployment_status
    jobs:
      e2e:
        if: github.event.deployment_status.state == 'success' && startsWith(github.event.deployment.environment, 'preview/')
        runs-on: ubuntu-latest
        steps:
          - uses: actions/checkout@v4
          - run: npx playwright test
            env:
              BASE_URL: ${{ github.event.deployment_status.environment_url }}
              # in a fixture: page.route(new URL(baseURL).origin + '/**', r => r.continue({
              #   headers: { ...r.request().headers(), 'x-roll-preview-bypass': process.env.ROLL_PREVIEW_BYPASS } }))
              ROLL_PREVIEW_BYPASS: ${{ secrets.ROLL_PREVIEW_BYPASS }}
    ```

    Services on roll apps get the secret's SHA-256 with the link's token (`access: {token,
    bypass_sha256}`); their proxy checks it the same way. Rotating or removing the secret makes
    every preview of the project converge again, so they get the new one.
  - Services on roll apps get the same token (`PUT …/environments/pv-… {access: {token}}`), and
    roll apps' waking proxy checks it the same way: the token once, then a host-only cookie,
    else 401 without waking anything.
- **Who.** `/v1/previews` sits behind the operator token like every product's API. Scoped tokens
  (`feature/roll-mcp`) classify these routes: `GET` is `read`; create, refresh, sleep and wake are
  `write`; reset and `DELETE` are `delete`. `Store::environment_of(kind, ref)` tells the
  evaluator which preview and project a branched resource belongs to, so a "previews only" token
  reaches the branches and never the originals.

- **Share links** (Vercel's shareable links): a link for someone outside the team
  (`POST …/shares {hours}`, the sheet's Share, `roll preview share`) on a token of its own, open
  until it ends (1–720 hours) or is revoked, while the preview's own link stays as it is. Only
  the token's SHA-256 is kept, so the link is shown once. Its visitor gets a cookie for that
  preview that lasts no longer than the link, checked against the store at every request, so a
  revoked or ended link stops at once. A share visitor gets no feedback button (they have no
  roll account to post with) and never the project's bypass.

## Cost

A preview's cost is the sum of what its branches and services use, priced with each product's
own rates (the same ones its usage page shows).

- **Sleeping**, per month: the bytes each branch owns, at its product's storage rate. Compute
  costs nothing.
- **Active**, per hour: the services' containers (memory GiB-hours), Postgres compute units
  while connected, and Redis memory while loaded.
- Every preview shows `cost: {sleeping_usd_month, active_usd_hour, month_to_date_usd}`, and the
  project shows the total.

### Measured on this machine

`scripts/preview-cost.sh` runs a rolld of its own (ports 27xxx, home in `~/.cache`). It:

1. makes a project with a repo (`roll.toml` `[preview]`: `python -m http.server`, `health = "/"`, 128 MiB,
   0.25 CPU) and a Postgres project, a Redis database, a bucket, a NoSQL database, a queues
   vhost, a stream with a topic and a tasks tenant;
2. puts 20 MiB of objects and 20,000 1 KiB keys into the parents;
3. opens a change, writes 1 MiB and 1,000 keys into the preview's branches, and opens the
   preview through its link (without it: 401);
4. sets a variable of the preview's own, pushes a docs-only revision with `ignore_paths:
   ["docs/"]`, then one that touches code, and puts `[skip preview]` in the title and takes it
   out again;
5. sleeps it and lands the change.

These are the numbers from 2026-11-12 and 2026-11-13 on ai-server (36 threads, ext4, load 25–35
from other sessions, debug build). It also checks, each run: the feedback button on the page and its script, the link access (401 without it,
the bypass secret's header, a wrong secret refused), the NoSQL and Postgres masks, the service's
Postgres role, the schema diff after a migration, a pushed log line, `roll preview wait`, a
preview's own variable, a kept build and `[skip preview]`.

| What | Measured |
| --- | --- |
| Change opened → preview ready (7 branches, 1 service deployed) | 4.6–6.8 s with Postgres when its parent's compute is running (a branch of a running parent is a file-system copy under the backup API); 9.6–16.8 s when the parent has to start first; 1.2–2.1 s without Postgres |
| The bucket branch: parent's bytes / its own after writing 1 MiB / objects it sees | 20 MiB / 1 MiB / 21 |
| The Postgres branch's data directory (a full copy on ext4; the parent's: 80–94 MiB) | 62–80 MiB |
| Postgres `users.email` in the branch / in the parent (rule `email`) | `user-c58bca0f6be4@example.invalid` / `ada@example.com` |
| The service's Postgres role / reading the WAL with it | the branch's own (`br_<id>`) / permission denied |
| First request (cold start of the service's container, image cached) | 3.6–5.1 s (13.6 under the heaviest load) |
| Next request (warm) | 13–27 ms |
| The service's container while running | 12–30 MiB of 128 MiB, 0.01% CPU |
| rolld while the preview runs (its Redis branch and NoSQL database loaded) | +86 to +106 MiB RSS |
| Containers left once it sleeps | 0 |
| **Cost asleep** | **$0.0006–0.0007 a month** once idle for an hour (the Postgres branch evicted to a ~4 MB base backup; Redis $0.0006); $0.006 a month in its first idle hour (the directory) |
| **Cost running** | **$0.012 an hour**: the container's 128 MiB, $0.002; Postgres at 0.25 CU, $0.010; Redis memory |
| A preview's own variable (`roll preview env FEATURE_FLAG=on`) / `DATABASE_URL` as one | seen by the running service / refused |
| `ignore_paths: ["docs/"]`, a docs-only push, then one to `index.html` | the services keep the earlier head (`runs`) while the preview reports the new one and `roll preview wait` passes; the next push redeploys (the page shows it) |
| `[skip preview]` in the title, then without it | the preview removed, then a new one |
| After landing | every branch gone, every parent intact |

So a hundred sleeping previews of this project cost about 7 cents a month, and an hour of
someone clicking through one costs about a cent.

**With roll apps** (a scratch merge of `feature/roll-apps`, `wip/previews-on-apps`): a repo with
a Dockerfile as a roll apps service, plus a Redis database. Since 2026-11-13 a preview is ready
only once its deploy is live.

| What | Measured |
| --- | --- |
| Change opened → preview ready, its deploy live (a Docker build of the change's commit) | 2.3–4.0 s |
| First request through roll apps' waking proxy | 382–444 ms |
| The Redis the service reached | the preview's branch, not the parent |
| After landing | the environment gone (404), nothing left: no containers, images, volumes or networks |

**With roll secrets** (`wip/previews-on-secrets`): the service saw development's `API_KEY`, not
production's; the preview's environment was untrusted and expiring in 24.0 h; an override
dropped from the rules left it; a reference to a missing key was named in the activity; nothing
was left after landing. **With roll search** (`wip/previews-on-search`): the branch read the
parent's document, its own writes stayed in it, and it was gone after landing.

What dominates, from most to least:

- **Postgres while connected.** A compute at its minimum size.
- **The Postgres branch's local directory on ext4** in the hour before it's evicted: a full copy
  (see `## Next`). An hour after a preview's last request, with nothing running, its branches are
  told it's idle: a Postgres branch is evicted (its directory goes to object storage as a base
  backup, and the next connection restores it). That's what keeps a sleeping preview's Postgres
  cheap on ext4.
- **The service's container while it runs.**
- **A Redis branch.** It reports its loaded dataset, which includes the parent's keys.
- **Bytes the preview writes.** Everything else branches copy-on-write and pays only for those
  (streams included: a stream branch shares its parent's chunks).

### Feedback on the preview page

Vercel Comments, Netlify's drawer: people reviewing a preview leave notes on its pages, and they
land on the change.

- The preview's proxy adds `<script src="/__roll/feedback.js" defer>` before `</body>` of a
  page load (`Sec-Fetch-Dest: document`; not a fetch, a fragment or an iframe), only for a
  visitor who came in through roll (the preview's access cookie) and when the project's
  `feedback` rule is on (the default). It asks the service for the page uncompressed and adds
  the tag as the page streams (streamed rendering stays streamed, memory stays one chunk);
  the page is then `Cache-Control: private` with no `ETag`. Anything compressed, not HTML or
  not a 200 passes through untouched. The script is served from the preview's own host, so a
  `script-src 'self'` policy allows it.
- The script (no dependencies, in a shadow root) shows a small button; it picks an element,
  takes a note, and opens the preview in roll's dashboard with the page's path (never its query,
  which can hold a token)
  (`/dashboard/projects/{project}/previews?preview={id}&fb_path=…&fb_selector=…&fb_note=…`). The
  preview's origin never holds a roll credential: the person, signed in to roll there, reads
  the note in the preview's sheet and posts it (nothing is posted from the link alone).
- roll posts it as a `comment` review on the change ("**Feedback on the preview** · `/checkout`
  linked to the preview · `button.pay`", then the note), which reaches the pull request like any
  review. The page's own names are kept to one inert line each.
- The button also lists what's been said about the page already (Vercel's comment pins): the
  proxy answers `/__roll/feedback.json?path=…` for the same visitors only (link access, through
  roll), from the change's feedback reviews (`roll_previews::notes` reads back the shape the
  dashboard posts and nothing else; dismissed ones don't show), the latest 50. Each note says
  who and when, and outlines its element on hover; all of it is text, never HTML.
- Previews deployed on roll apps go through apps' own proxy, which doesn't add it yet.
- Public previews don't get it (anyone could see the button), and a branch's preview has no
  change to post on.

## Data model

`code_sql` like the workspace: SQLite at `<home>/previews.db` on one node, or the system
database's `previews` schema shared by every node. Every read-then-write is one `Db::write`
closure.

A converge holds its preview with a lease in `next_at` (5 minutes; a crash is retried when it
ends). A request meanwhile (a variable removed, a push, a keep, a rules change) sets `next_at` to
0, which bumps `generation`, and the converge settles only if `generation` didn't move, so the
request is converged next, never lost. A deploy a backend accepted but hasn't finished counts as
sent: what it carried is recorded, so a change back to how it was before is sent again.

```sql
settings(project_id TEXT PRIMARY KEY, json TEXT NOT NULL, updated_at INTEGER NOT NULL)
previews(id TEXT PRIMARY KEY, project_id TEXT NOT NULL, slug TEXT NOT NULL UNIQUE,
         repo TEXT NOT NULL, change_id TEXT UNIQUE, branch TEXT, number INTEGER, title TEXT NOT NULL,
         github_pr INTEGER, head TEXT NOT NULL, trusted INTEGER NOT NULL,
         desired TEXT NOT NULL,            -- active | deleted
         state TEXT NOT NULL,              -- see Preview.state
         error TEXT, attempts INTEGER NOT NULL, next_at INTEGER NOT NULL,
         asleep INTEGER NOT NULL, reset_requested TEXT, last_reset_at INTEGER,
         last_active_at INTEGER NOT NULL, expires_at INTEGER,
         created_by TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL)
resources(preview_id TEXT NOT NULL, kind TEXT NOT NULL, parent TEXT NOT NULL, mode TEXT NOT NULL,
          ref TEXT, state TEXT NOT NULL, error TEXT, bytes INTEGER NOT NULL, updated_at INTEGER NOT NULL,
          PRIMARY KEY (preview_id, kind, parent))
services(preview_id TEXT NOT NULL, name TEXT NOT NULL, repo TEXT NOT NULL, host TEXT NOT NULL UNIQUE,
         revision TEXT, deployed TEXT, state TEXT NOT NULL, error TEXT, memory_mb INTEGER NOT NULL,
         active_ms INTEGER NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY (preview_id, name))
dismissed(change_id TEXT PRIMARY KEY, head TEXT NOT NULL, at INTEGER NOT NULL)
spend(project_id TEXT NOT NULL, month INTEGER NOT NULL, usd REAL NOT NULL, PRIMARY KEY (project_id, month))
activity(preview_id TEXT NOT NULL, at INTEGER NOT NULL, seq INTEGER NOT NULL, message TEXT NOT NULL,
         PRIMARY KEY (preview_id, seq))   -- newest 200 per preview
```

- A preview id is `pv_` followed by 12 random characters of lowercase base32. Times are unix
  milliseconds.
- `resources.ref` is null until the branch exists. `bytes` is refreshed every 5 minutes.

## HTTP contract

Bodies are JSON. Every error is `{"error": string}`. A signed-in person who isn't an admin sees
only the previews of their own workspace projects (as the workspace scopes them): another
owner's preview or project is a 404. Tokens (their scopes are roll MCP's catalog), the operator
and admins see every project.

| Route | Body | Answer |
| --- | --- | --- |
| `GET /v1/previews?project=&state=` | | `Preview[]`, newest first |
| `POST /v1/previews` | `{project, repo, branch? , change?}` | 201 `Preview` (or 200 with the existing one) |
| `GET /v1/previews/{id}` | | `Preview` |
| `DELETE /v1/previews/{id}` | | 202 `Preview` (`desired: "deleted"`) |
| `POST /v1/previews/{id}/refresh` | | `Preview`: redeploy now |
| `POST /v1/previews/{id}/sleep` | | `Preview`: stop its services now |
| `POST /v1/previews/{id}/wake` | | `Preview`: start its services now |
| `POST /v1/previews/{id}/reset` | `{resources?: ["postgres:prj_…"]}` | `Preview`: re-branch from the parents |
| `GET /v1/previews/{id}/screenshot` | | `image/png`: the first service's page at the preview's head (404 when it has none); `?previous=1` the one before, `?view=phone` at a phone's width, `?base=1` the default branch's preview's, `?page=/pricing` another of `github.pages`, `?diff=base` or `?diff=previous` where it differs from that one or the one before (the page faded, changed pixels marked red) |
| `GET /v1/previews/{id}/screenshot.png?exp=&sig=` | | `image/png` behind a signed, expiring link, without credentials (403 otherwise): the pull request comment's image |
| `GET`/`POST /v1/previews/{id}/shares` | `{hours}` (1–720) for a new one | `Share[]` (never a token), or 201 `Share` with `url`, the only time the link is shown; 409 for a public preview or past 20 live ones |
| `DELETE /v1/previews/{id}/shares/{share}` | | 204: the link stops at the visitor's next request |
| `GET`/`PUT /v1/previews/{id}/env` | `{vars: {NAME: value \| null}}` (`null` removes; at most 100, 32 KiB each) | `{vars}`: the preview's own variables; its services redeploy with them |
| `POST /v1/previews/{id}/keep` | `{hours}` (0–720; 0 releases) | `Preview`: kept past its TTL until then (`kept_until`); a landed or closed change still ends it |
| `POST /v1/previews/{id}/visual/accept` | | `Preview`: how a change's pages look at its latest screenshot is accepted (`visual_review`, `roll/visual`); 409 with no screenshot yet, 400 for a branch's preview, 403 from a token when the review is required |
| `GET /v1/previews/{id}/logs?service=&lines=` | | `{service, lines: string[]}` |
| `GET /v1/previews/{id}/logs?service=&lines=&follow=1` | | `text/event-stream`: `event: service` (its name), then one event per line, the last `lines` first and then as they come. `docker logs -f` for `roll.toml` services; for others the tail is read every 2 s (2,000 lines wide after the first read) and only new lines are sent, with a line saying so when some were missed. Runs on the preview's holder; the stream ends when the client goes |
| `GET /v1/previews/{id}/activity` | | `Activity[]`, newest first |
| `GET /v1/previews/projects/{project}/settings` | | `Settings` |
| `PUT /v1/previews/projects/{project}/settings` | `Settings` (partial) | `Settings` |
| `GET /v1/previews/projects/{project}/usage` | | `{previews, sleeping_usd_month, active_usd_hour, month_to_date_usd, bytes, spent_usd_month, max_usd_month}` |
| `GET /v1/previews/projects/{project}/bypass` | | `{enabled, created_at, header}` (never the secret) |
| `POST /v1/previews/projects/{project}/bypass` | | 201 `{enabled, created_at, header, secret}`: a new secret, shown only here; the old one stops working |
| `DELETE /v1/previews/projects/{project}/bypass` | | 204 |

- 400: a bad body or setting. 404: no such preview, project or repo. 409: the change already has
  a preview in another project, or `max_previews` is reached (manual creation only; automatic
  ones queue).
- `POST /v1/previews` with `change` (`chg_…`) or `branch`. The repo must be in the project.

```ts
type Preview = {
  id: string; project: string; slug: string; repo: string;
  change: string | null; number: number | null; github_pr: number | null; branch: string | null;
  title: string; head: string; trusted: boolean;
  runs: string | null;                // the earlier head its services run (later pushes touched only ignored paths)
  earlier: {service: string; revision: string; url: string; share_url: string | null; at: number}[]; // earlier pushes, still reachable
  goes_at: number | null;             // when it goes (TTL, keep or expiry running out), once a day away (or a quarter of a shorter TTL)
  desired: "active" | "deleted";
  state: "queued" | "awaiting_approval" | "creating" | "ready" | "sleeping" | "updating" | "failed" | "deleting";
  error: string | null;
  url: string | null;                 // the first service's URL
  share_url: string | null;           // url with its access token (link access): the one to hand out
  services: Service[]; resources: Resource[];
  cost: { sleeping_usd_month: number; active_usd_hour: number; month_to_date_usd: number; bytes: number };
  schema_changes: string[] | null;    // "+ …"/"- …" against the parents, once read at `head`; null: not known
  kept_until: number | null;          // kept past its TTL until then (POST …/keep)
  screenshot_at: number | null;       // when GET …/screenshot was taken (github.screenshot)
  last_reset_at: number | null; last_active_at: number; expires_at: number | null;
  created_by: string; created_at: number; updated_at: number;
  baseline: boolean;                  // roll keeps it to compare the project's changes with (github.baseline)
};
type Service = { name: string; repo: string; url: string; share_url: string | null; revision: string | null; deployed: string | null;
                 state: "pending" | "deploying" | "running" | "sleeping" | "failed"; error: string | null };
type Resource = { kind: Kind; parent: string; ref: string | null; mode: "branch" | "schema";
                  state: "pending" | "ready" | "resetting" | "failed" | "deleting"; error: string | null; bytes: number };
type Kind = "postgres" | "redis" | "bucket" | "nosql" | "stream" | "queue" | "tasks" | "search";
type Activity = { at: number; message: string };
type Job = { image: string; run: string; timeout_s?: number /* 600 */; memory_mb?: number /* 1024 */ };
type Settings = {
  enabled: boolean;
  branches: string[];                 // globs on the change's branch; ["*"]
  drafts: boolean;                    // previews for draft changes (false)
  manual: boolean;                    // only changes whose title asks ([preview]) get one (false)
  paths: string[];                    // what a change must touch (gitignore-style); [] is anything
  ignore_paths: string[];             // touching only these doesn't count ([])
  resources: Record<string, "branch" | "schema" | "skip">;   // "<kind>:<ref>" → mode; unlisted: `data`
  data: "branch" | "schema";          // the default mode ("branch")
  reset_every_hours: number;          // 0: never
  sleep_after_s: number;              // 600
  ttl_hours: number;                  // 168
  max_previews: number;               // 10
  max_bytes: number;                  // 10 GiB
  max_usd_month: number;              // 0: no cap; once the month's cost reaches it, new previews wait
  secrets: { base: string; allow_production: boolean };      // "development", false
  env: Record<string, string>;        // overrides every service gets
  github: { comment: boolean; status: boolean; deployment: boolean; screenshot: boolean; baseline: boolean; pages: string[]; hide: string[]; budget: { load_ms: number; bytes: number } };   // all true but screenshot; pages, hide: []; budget 0s (none)
  access: "link" | "public";          // "link": a preview's URLs need its link ("link")
  visual_review: "off" | "check" | "required";   // the roll/visual review; required also blocks landing in roll ("off")
  feedback: boolean;                  // a feedback button on its pages for people who came in through roll (true)
  warm: boolean;                      // wake the change's services as soon as a new head is deployed (true)
  env_for: Record<string, Record<string, string>>; // branch glob → variables, over env, under a preview's own ({})
  seed: Job | null;                   // once on fresh data
  release: Job | null;                // at each new head, before services deploy (migrations)
  masking: Record<string, Record<string, string>>; // "postgres:<ref>" → {"schema.table.column": "email"}, "nosql:<ref>" → {"ks.table.column": "mask_default()"}
};
```

## Surfaces

- **roll code change**: `Change.preview` carries the environment: its state, the first URL,
  `environment: {id, slug, state, services: [{name, url, state}], resources: [{kind, parent, ref}],
  sleeping_usd_month, active_usd_hour, dashboard_url, github}` (docs/code/http-api.md). A change whose repo is in
  a project with previews on gets its preview from here instead of code-preview's single
  container.
- **GitHub PR**, through roll code's GitHub outbox (durable, retried, sent on the repo's owner):
  - one comment, edited in place, with a table of services, URLs, status, data branches and cost;
  - a commit status `roll/preview` on the head. A preview removed before it was ready (the change
    landed or closed) settles its pending status to success, so a commit now on the default branch
    never waits on it; a failure stays;
  - a GitHub deployment to the environment `preview/<slug>` with the URL, marked `inactive` when
    the preview is deleted.
  - with `github.screenshot`, a screenshot of the first service at each new head (Vercel's
    preview images): a headless Chromium (`ROLL_PREVIEW_SCREENSHOT_IMAGE`, Playwright's image by
    default) runs in the service container's own network namespace, so it reaches exactly what
    the service does and nothing on the host, within 1 GiB, one CPU and 90 s, and the PNG (at
    most 2 MiB) is kept with the preview. It's on the preview sheet and at `GET …/screenshot`.
    The one it replaces, at the head before, is kept too (`GET …/screenshot?previous=1`, the
    sheet's Before/After), with how much changed: the share of pixels whose colour moved more than
    a little (anti-aliasing isn't a change), the page's added height counting as changed
    (`visual_change` on the preview and the change; "The page changed 12.3% since `abc123`" in the
    pull request comment), as Percy and Chromatic show visual changes for review.
    The same load times the page (Vercel's Speed Insights, Netlify's Lighthouse plugin):
    `shot.js` drives the headless shell over the DevTools protocol on a pipe and reads the
    navigation timing, the first contentful paint and the bytes fetched (`page_speed`, with the
    head before's to compare; "Its first page loads in 1.2 s (+300 ms since the push before)" in
    the comment). The page reports this about itself, so it's bounded (10 minutes, 10 GiB) and
    only ever shown. An operator's own screenshot command only has to write the PNG.
    A change's screenshots are also compared with the default branch's (Chromatic's baseline):
    `base_change`, the sheet's main view (`GET …/screenshot?base=1`), "Against `main`, the page
    differs by 8.0%" in the comment. With `github.baseline` (on by default) roll keeps that preview
    itself: once the project has a change preview, it makes one of the first such repo's default
    branch (created by `roll`; one per project, since every preview runs all the project's
    services). It follows the branch, sleeps between screenshots, holds none of `max_previews`'
    slots (it still counts against `max_usd_month`, and waits once that's spent), and lasts while
    there are change previews, then its TTL. A preview of the repo's default branch someone made
    (`roll preview create --branch main`) serves instead. Deleted, it comes back at the next pass
    while the rule is on; turning the rule off deletes it. A change is compared at its screenshot;
    one taken before the baseline had any is compared once the baseline's first one comes in, and
    its comment updated. The baseline keeps its screenshots of its last 10 heads, and a change is
    compared with the one where it left the branch (its merge base, as Chromatic compares with an
    ancestor build), so main's own changes since don't count as the change's; when that head's
    are gone, with the latest (`base_change.head` says which; `?base=1` serves that one).
    The same session then takes the page at a phone's width (390 × 844 at twice the density,
    `GET …/screenshot?view=phone`, the sheet's Desktop/Phone), with its own change since the push
    before ("At a phone's width: 3.4%" in the comment), as Percy and Chromatic check widths.
    Then it takes the project's other pages (`github.pages`, up to 5 paths on the service's own
    origin, as Percy's snapshot list), at the desktop width: each is kept with the one before it
    and how much it changed, compared with the baseline's page at the same path, and counts what
    went wrong while it loaded (`pages` on the preview, `GET …/screenshot?page=/pricing`, a page
    switcher on the sheet, "2 more pages, most changed /pricing by 40.0%" in the comment's
    checks). A path no longer listed is dropped at the next screenshot.
    Before every capture the page is held still (Chromatic pauses animations, Percy takes
    `percyCSS`): animations and transitions stop, the caret is hidden, videos pause at their
    start, and the project's `github.hide` selectors (up to 20; a clock, an ad, a random avatar)
    are blanked with their box kept, so the same page screenshots the same and only real changes
    count. Speed is timed before that, on the page as it loaded.
    A project can give that page a budget (`github.budget`: a load time and bytes, as Lighthouse
    budgets do on Netlify and Vercel): a `roll/speed` commit status fails while the page is over
    it ("Loads in 2.4 s, 1.2 MB: over its budget (2.0 s load)") and passes within it, and the
    comment's checks say "over its budget".
    Where a page changed is a picture too (Percy's and Chromatic's diff view): `?diff=base` or
    `?diff=previous` (with `view` and `page` as for the screenshot) fades what's the same and marks
    what differs, the area only one of them has included; the sheet's Diff shows it against main
    (else the push before), so whoever accepts a visual change sees what they accept.
    With `visual_review` (`off` by default, `check` or `required`) a change's pages are reviewed as
    Chromatic's UI Review does: once compared with the baseline, the change has `visual_review`
    (the most any page differs, at either width), and a `roll/visual` commit status that's
    pending while a page differs and nobody has accepted it ("Differs from main by 8.0%: accept it
    in roll"), and success once someone has (the sheet's Accept, `roll preview accept`, `POST
    …/visual/accept`; "Accepted by ada"), or when the pages look the same. An acceptance is of one
    screenshot's head: the next push is reviewed again, unless every page (desktop, phone, the
    other pages) looks exactly as at the accepted head, when the acceptance carries over (as
    Chromatic carries accepted baselines forward). GitHub's branch protection can require
    `roll/visual`; with `required` roll's own landing does too (a `visual` blocker in
    `Code::blockers`, from the project's rule, never the change's). A required review is a
    person's: its author's acceptance doesn't count (as with approvals), and a scoped or OAuth
    token (an agent's) can't give one (403). Automations see it too: the `preview` trigger's
    `preview.environment.visual_review.state` field, and the built-in "Tell me when a visual change
    waits for a review" template (a notice linking to the preview, or a Slack step instead).
    The same session checks what a screen reader can't name (images without alt text, form
    fields without a label, buttons and links with no name) and whether the document has a
    language and a title (`page_a11y`, counts and the first one's markup, bounded; "**Accessibility:**
    3 images without alt text …" in the comment), the most common WCAG failures, as Vercel's
    toolbar and Netlify's checks show.
    The same session catches what went wrong while the page loaded: uncaught exceptions,
    `console.error`s, and requests that failed or answered 4xx/5xx (`page_health`, counts and the
    first of each, one bounded line each because they're the page's own words; "**While its
    first page loaded:** 2 errors (first: `TypeError: …`); 1 failed request" in the comment), so a
    broken page that still looks right in its screenshot shows.
    In the comment these are one folded block whose summary has only counts and numbers, problems
    first ("Page checks: 2 errors · 5 accessibility problems · differs from main by 8.0% · changed
    12.3% since the push before · loads in 1.2 s", as Vercel's bot comment folds its details), so
    the comment stays short however much the page reports; the page's own words are only inside.
    For a `public` preview the pull request comment shows it through a signed link
    (`/v1/previews/{id}/screenshot.png?exp=…&sig=…`: HMAC-SHA256 of the preview and the expiry,
    30 days after the screenshot, under a key the previews store makes once; a public route,
    because GitHub fetches images without credentials; anything but a valid link is 403). A
    private preview's comment doesn't, since whoever reads the pull request could then see it.
    Screenshots are taken after the reconciler's pass, one at a time, so a slow service or the
    first pull of the browser image (pulled once, when first needed) holds nothing else up; the
    browser container has no capabilities, no new privileges and a read-only root. A failed try
    is remembered for its head and the last screenshot stays. A link names one screenshot (its
    expiry is 30 days after it was taken) and works only while the preview is live and public,
    so a project that turns private takes its old comments' images down at once.
    It wakes the service once per push, so it's off by default.
- **Dashboard**: a Previews tab on each project, with the list, status, cost, logs, and open,
  sleep, wake, reset and delete actions, plus a "Previews" overlay on the canvas that draws each
  preview's branches next to their parents.
- **CLI**: `roll preview ls|show|open|create|rm|sleep|wake|refresh|reset|keep|logs [-f]|activity|settings|usage|bypass|wait`.
  For CI outside GitHub, `roll preview wait --change chg_…` (or `create … --wait`) blocks until
  the preview is ready and prints its URL (`--service` for another), exiting 1 if it fails, is
  deleted or isn't ready within `--timeout` (600 s): the next step tests that URL with the
  bypass secret.

Every one of them plays the mark states: `deploying` while creating or updating, `success` when
ready, `error` on failure, and `sleep` for a sleeping preview.

## Next

Ranked by what a person notices first. Done so far:

- the crate;
- rolld's adapters for every product (Postgres resets in place and goes cold after an idle
  hour), the secrets, roll apps and search adapters;
- seeds;
- link access;
- roll code and GitHub reporting;
- `roll preview`;
- the dashboard's Previews tab and canvas overlay;
- the end-to-end run and its cost;
- an adversarial review and its fixes;
- the private page's way in through roll;
- per-repo build caches;
- a running parent's Postgres copied by reflinks;
- roll apps end to end;
- previews spread across a cluster's nodes, a lease each (tested);
- roll apps' `pv-…` environments private, their 401 page linked through roll;
- NoSQL column masks per preview;
- copy-on-write stream branches (shared chunks, refcounted deletes) and Postgres `schema` mode;
- a monthly budget per project, `roll preview open`, and following logs in the dashboard;
- a second adversarial review and its fixes (and a stream branch's cut that waits for aborting
  transactions' index rows);
- search branches' cost from roll search's `own_bytes`;
- integration with roll secrets and roll MCP (`wip/previews-on-secrets-mcp`): a preview's
  branches resolve to the `preview` environment in rolld's access resolvers, so a "Previews
  only" token reads and changes the branches and gets 403 on their parents and on the rules
  (measured against a live rolld with the operator token on);
- a project bypass secret for CI (`x-roll-preview-bypass`), so end-to-end tests triggered by the
  GitHub deployment can open private previews.
- a `release` step at each new head (migrations) and the schema changes it left, on the
  preview, the pull request comment, the dashboard and the CLI (measured against real Postgres).
- live logs pushed (server-sent events) to the dashboard's sheet and `roll preview logs -f`.
- the bypass on roll apps' gate too (`bypass_sha256`, feature/roll-apps 31ddb210d).
- a budget alert to the project's repos (its own `budget` event: webhooks, the event stream,
  automations) at 80% and 100% of `max_usd_month`, once a month per budget;
- Postgres column masking per preview (measured: the branch reads `user-…@example.invalid`, the
  parent the real address);
- keeping a preview past its TTL (`POST …/keep`, the sheet, `roll preview keep`), and a warning a
  day before an unused one goes;
- each thread checked live against its latest branch: roll secrets (`wip/previews-on-secrets`:
  the one-call ensure, an untrusted environment expiring in 24 h, a dropped override gone, the
  service seeing development's value), roll apps (`wip/previews-on-apps`: a preview ready only
  once its deploy is live, the branch's Redis seen, nothing left after landing) and roll MCP
  (`wip/previews-on-secrets-mcp`: a "Previews only" token reaches branches, never parents) and
  roll search (`wip/previews-on-search`: the branch reads the parent's documents, its own writes
  stay in it, its bytes are roll search's `own_bytes`, it's gone after landing and the parent
  stays);
- unresolved secrets named in the preview's activity;
- `roll preview wait` for CI outside GitHub (and `create --wait`);
- screenshots of the first service at each head (taken in the service container's network), on
  the sheet, at `GET …/screenshot`, and in a public preview's pull request comment through a
  signed, expiring link;
- a Postgres role of its own per preview branch (no superuser, no file or WAL reads, unknown on
  the parent), so masking holds against the preview's own code too;
- earlier pushes reachable at URLs of their own (the last 3 per service, on the same data);
- services warmed as soon as a push is deployed (`warm`);
- a health check before "ready" (`roll.toml` `health`), and the start/stop race it found live
  (a sleep removing the container a start was making; a request stopping a fresh start);
- a feedback button on a preview's pages for people who came in through roll, posting on the
  change from roll (Vercel Comments);
- a preview's own variables over the project's (`…/env`, the sheet, `roll preview env`);
- `[skip preview]` and `[preview]` in a change's title and `no-preview`/`preview` labels (GitHub's
  or roll's), a `manual` mode, and path filters (`paths`,
  `ignore_paths`) on what a change touches since its merge base, and pushes that touch only
  ignored paths keep the running services;
- the same branch across a project's repos (another repo runs its own branch of the change's name,
  and a push to it reaches the preview);
- a look at the Previews tab and sheet from screenshots of the real thing (each row says its state
  and shows its screenshot, the sheet's schema changes wrap, masked branches say so, the
  screenshot is a thumbnail that opens full size, and on a phone the project's agents panel
  starts closed instead of covering the page);
- branch roles in pg-control (`app_role`, `app_connection_uri`, kept across in-place resets) with
  its e2e test, which previews use;
- this round checked live in `scripts/preview-cost.sh` (a preview's own variable, a kept build,
  `[skip preview]`);
- each change's page compared with the default branch's preview (Chromatic's baseline);
- accessibility per push (what a screen reader can't name, a missing language or title);
- a phone-width screenshot with its own visual change;
- every change compared with the default branch, whose preview roll keeps itself (`github.baseline`);
- the comment's page checks folded under one line of counts;
- screenshots of the project's other pages (`github.pages`), each compared like the first;
- a visual review per push (`visual_review`: the `roll/visual` status, Accept in roll, and
  `required` keeping the change from landing in roll);
- comparisons with main at the change's merge base (the baseline's last 10 heads kept);
- stable screenshots: animations stopped and `github.hide` selectors blanked before each capture;
- where a page changed, as a diff image (`?diff=base|previous`, the sheet's Diff);
- a speed budget per project (`github.budget`, the `roll/speed` status);
- visual reviews in automations (the `preview` trigger's field, a built-in template);
- an acceptance carried over a push that changes nothing visible;
- a look at the sheet and the change page from screenshots of a live preview (the screenshot's
  modes spaced apart, roll's baseline tagged in the list);
- a screenshot cut short by a redeploy or a sleep tried again once, and turning a project
  public or private restarting nothing on roll's runtime;
- what went wrong while the page loaded (exceptions, console errors, failed requests);
- expiring share links for people outside the team (revocable, shown once);
- page speed per push (timed with the screenshot, compared with the push before);
- the page's feedback listed by its button (who, what, its element outlined; roll apps' proxy
  can ask `Engine::notes_for` by environment name);
- a visual change per push (the screenshot against the one before, Before/After on the sheet,
  the share of pixels changed in the pull request comment);
- per-branch project variables (`env_for`);
- roll apps live in `scripts/preview-apps.sh` (2026-11-13, load 46: a change's `pv-` environment
  built at its commit and live in 39 s, its first request 47 ms, the branch's Redis seen, a
  preview's own variable set and then unset there, nothing left after landing); it found that a
  request during a converge could be lost (fixed in #27);
- removed variables unset from roll apps' `pv-` environments (the names roll set kept in
  `ROLL_PREVIEW_SET` there; a person's own never touched);
- main merged in (roll MCP and accounts), and the previews API scoped to a signed-in person's own
  projects;
- a seventh review and its fixes (health checks never hold a converge slot; a start notices a
  stop while it boots, and a stop keeps a start asked for after it; the feedback tag streams in,
  only on page loads, with the path but never the query; warming skips slept previews and other
  nodes' previews; `preview`/`no-preview` survive a long label list);
- a sixth review and its fixes (only `ignore_paths` keeps a build, never past `roll.toml`; hand-made
  previews outlive the rules; warnings for a keep running out; atomic preview variables; a
  pending `roll/preview` settled when a preview goes before it was ready);
- a fifth adversarial review and its fixes (the superuser never runs what a preview's code
  planted: pinned `search_path`, the schema diff as the branch's role, no in-place rewrite after
  preview code ran, triggers restored exactly; screenshots off the reconciler's pass; links bound
  to one screenshot of a live public preview);
- a fourth adversarial review and its fixes (masks
  without triggers and with a salt, masked branches reset by branching again, changed rules
  masking open previews again, deploys checked again instead of awaited, a failed deploy retried
  as a new one, trust in the deploy fingerprint, `wait` that can't pass on an older push);
- a third adversarial review and its fixes (no bypass for forks, rotation restarts nothing the
  runtime runs, printable schema lines, budget alerts that can't be lost or silenced, followed
  logs that say when lines were missed and stop with a hidden tab).

1. **The feedback button on roll apps' `pv-` environments**: apps adds it itself once told
   (`Target.feedback` = the `feedback` rule, a change's, link access; in `config_fingerprint`;
   `"feedback": true` in RollApps' environment `PUT`). Waits for that apps change
   (PR #26) to reach main: today's apps refuses an unknown field, which would
   fail every link-access deploy.
2. **roll apps' `pv-` environments, the rest of the apps thread's review**: once roll secrets is
   wired into apps (apps resolves `pv-<id>` itself), send only the links' redirects, not a
   second copy of the project's variables. (A preview's cost now uses the memory its live
   deployment runs with: `[app.previews.run]` and the environment's own size over the service's.)
3. **Postgres without a reflinking file system**: a branch that starts from the parent's base
   backup plus WAL with lazy pages, for ext4 hosts like this one. That's a pg-storage project of
   its own (a page server); the fleet's XFS volumes already share blocks through reflinks, and an
   idle preview's branch is evicted to object storage after an hour.
4. **Watch: a cold start on a Docker name conflict**: one live run in six (right after a killed
   daemon left containers) failed a wake with "the container name is already in use". A remove
   that dropped the service's entry before stopping it could start a second container under the
   name (fixed); if it comes back, log both starts' container ids.

Looked at and dropped: branching from a stopped parent without starting it. pg-control already
copies a stopped parent's directory directly; the slow first numbers (9.6–16.8 s) were brand-new
parents being initialized on their first start, and a preview of an existing project is ready in
about 5 s (branch compute 1.3 s, its history's base backup 1.8 s, the other products about 1 s).
