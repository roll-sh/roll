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
  - `roll preview create --repo acme/web --branch feat-x` (or `POST /v1/previews`) makes one for a
    branch with no change. It lives until it is deleted or its TTL passes.
- **Update**: every new head redeploys the services of the change's repo at that head. Services
  from the project's other repos run at their default branch's head, and are redeployed only
  when that head moves. This is Railway's "focused" environment, done the cheap way. Data
  branches stay (they hold the preview's own writes) unless the change resets them.
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
- **Limits**:
  - `max_previews` (10 by default): previews beyond it wait as `queued`, oldest change first,
    until one is deleted.
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
   5. The secrets environment `preview-<slug>` is ensured (base, overrides, resource URLs).
   6. Every service whose revision is not the one it should run is redeployed (the change's head
      for its repo, the default head for the others).
   7. The preview becomes `ready` (or `sleeping`), and the change and the PR hear about it.
3. A step that fails records its error on the preview (`failed`) and is retried with backoff
   (15 s × 2ⁿ, at most 10 minutes). Nothing else is undone.

The reconciler runs every 15 s, and right away when roll code emits a `change` or `landed`
event (the broadcast is only a nudge, so a dropped event costs at most one interval). In a
cluster it runs on one node (`singleton("previews")`), and preview hosts forward to that node,
which runs the services' containers. Only the reconciler writes a preview's resources and
services. The API writes only the desired state (and asks the reconciler to run).

## Branches per product

Branch names are `pv-<preview id>` (Postgres: `preview/<preview id>`). Branch adapters live in
`crates/rolld/src/previews.rs` and implement `roll_previews::Brancher`.

| Kind | Branch | Reset | Sleeping cost | Notes |
| --- | --- | --- | --- | --- |
| `postgres` | `pg_control::Control::create_branch` from the project's `main` | Delete and branch again, on the same branch name. The URL changes, and the services are redeployed with it | Its local data directory until eviction (24 h), then its own base backup in object storage | The weakest copy-on-write here: a branch is a full data directory on ext4 (reflinks on btrfs/XFS). See `## Next`. |
| `redis` | `redis_control::Control::branch` | Delete and branch again | Nothing in memory; its manifest and the segments it pins | Copy-on-write in storage. Loading replays the parent into memory. |
| `bucket` | `s3_store::Store::branch_bucket` | Delete and branch again | The objects it wrote, plus the parent's writes pushed down to it | One row to create. Credentials are a write key scoped to the branch. |
| `nosql` | `nosql_control::Control::branch` (`schema_only` for `schema`) | `nosql_control::Control::reset`: same id, endpoint and credentials | The SSTables it wrote | Zero copy. Masking policies are inherited. |
| `stream` | A new stream with the parent's topics and their configs (`schema`). `branch` also copies each topic's committed offsets so consumers resume where the parent was | Delete and branch again | Its own chunks | A data fork that shares the parent's chunks needs reference counting in the broker's retention and compaction first. See `## Next`. |
| `queue` | A vhost with the parent vhost's definitions (exchanges, queues, bindings, policies), no messages | Delete and branch again | Nothing (a vhost suspends after 5 minutes) | A branch never holds production messages. |
| `tasks` | A new tenant with no messages and no schedules | Delete and create again | Its own SQLite file | Copied schedules would call production endpoints, so they never are. |
| `search` | `search_control::Control::branch` (zero copy) | Delete and branch again | Segments it wrote | Joins when roll search lands (`feature/serverless-search`). Its adapter is behind the same trait. |

Safety rules every adapter follows:

- **It never deletes the parent.** `delete` deletes only an id the preview recorded. Before
  deleting, it checks that the id is a child of the recorded parent and has the preview's branch
  name. Anything else is refused and reported.
- **It branches only resources in the preview's project.** A preview can't name a resource
  outside its project.
- **No connection string is stored.** The preview records ids only. URLs and passwords are read
  from the product each time a service is deployed, so a rotated password reaches the next
  deploy.

## Services

A service is something the preview runs and serves over HTTP.

- Until roll apps lands (`feature/roll-apps`, docs/apps/), the services are the repos in the
  project whose `roll.toml` has a `[preview]` section. The service is named after the repo
  (`[preview] name = "api"` overrides it) and runs in the container `roll.toml` describes,
  through code-preview's `Isolation`: Docker here, a Fargate task on the fleet.
- roll apps will implement the same `Apps` trait (`deploy` a service at a revision into an
  environment, `stop`, `delete`, `logs`).
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
  2. the resolved secrets environment (`preview-<slug>`, inheriting its base);
  3. the project's preview `env` overrides;
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
- Containers and images are shared: one image per `roll.toml` image reference, pulled once per
  node. Build caches are the image's (`pnpm`, `cargo` caches under `/root/.cache` aren't kept
  between starts; see `## Next`).

## Secrets

A preview's secrets are an environment of the project's secrets (`feature/roll-secrets`,
docs/secrets.md) called `preview-<slug>`, inheriting from `secrets.base` (`development` by
default) with the project's `env` overrides and the branched resources' URLs on top.

- **Production secrets are never inherited unless the project allows it.** A base named
  `production` or `prod` is refused unless `secrets.allow_production` is true, and a fork's
  preview never gets them, whatever the setting says.
- Until roll secrets lands, the `Secrets` trait has a stand-in that knows no secrets: a
  service gets the overrides and resource URLs only.
- The environment is deleted with the preview.

## Safety

- **Forks.** A change with `trusted: false` (a PR from a fork) gets a preview only for a head a
  writer approved (`approve-run`, roll code's rule). A new push un-approves it. Its services
  stop, and it waits as `awaiting_approval` with its data branches kept. This also fixes roll
  code's own previews: the container started for a visit now refuses an unapproved head.
- **Data.** A branch is a child of the parent. Deleting the branch never touches the parent (the
  products' refcounts and garbage collection guarantee it, and the adapter checks parentage
  before it deletes).
- **Production secrets** are covered above.
- **Who.** `/v1/previews` sits behind the operator token like every product's API. Scoped tokens
  (`feature/roll-mcp`) classify these routes: `GET` is `read`; create, refresh, sleep and wake are
  `write`; reset and `DELETE` are `delete`. `Store::environment_of(kind, ref)` tells the
  evaluator which preview and project a branched resource belongs to, so a "previews only" token
  reaches the branches and never the originals.

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

The `measurements` section below is filled by `scripts/preview-cost.sh`. That script creates a
project with one of each resource, opens a change, visits it, lets it sleep, and reads what each
product and the container used. Numbers are from 2026-11-12, on ai-server (36 threads, ext4,
load 25–30 from other sessions).

_Pending: see `## Next`._

## Data model

`code_sql` like the workspace: SQLite at `<home>/previews.db` on one node, or the system
database's `previews` schema shared by every node. Every read-then-write is one `Db::write`
closure.

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
activity(preview_id TEXT NOT NULL, at INTEGER NOT NULL, seq INTEGER NOT NULL, message TEXT NOT NULL,
         PRIMARY KEY (preview_id, seq))   -- newest 200 per preview
```

- A preview id is `pv_` followed by 12 random characters of lowercase base32. Times are unix
  milliseconds.
- `resources.ref` is null until the branch exists. `bytes` is refreshed every 5 minutes.

## HTTP contract

Bodies are JSON. Every error is `{"error": string}`.

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
| `GET /v1/previews/{id}/logs?service=&lines=` | | `{service, lines: string[]}` |
| `GET /v1/previews/{id}/activity` | | `Activity[]`, newest first |
| `GET /v1/previews/projects/{project}/settings` | | `Settings` |
| `PUT /v1/previews/projects/{project}/settings` | `Settings` (partial) | `Settings` |
| `GET /v1/previews/projects/{project}/usage` | | `{previews, sleeping_usd_month, active_usd_hour, month_to_date_usd}` |

- 400: a bad body or setting. 404: no such preview, project or repo. 409: the change already has
  a preview in another project, or `max_previews` is reached (manual creation only; automatic
  ones queue).
- `POST /v1/previews` with `change` (`chg_…`) or `branch`. The repo must be in the project.

```ts
type Preview = {
  id: string; project: string; slug: string; repo: string;
  change: string | null; number: number | null; github_pr: number | null; branch: string | null;
  title: string; head: string; trusted: boolean;
  desired: "active" | "deleted";
  state: "queued" | "awaiting_approval" | "creating" | "ready" | "sleeping" | "updating" | "failed" | "deleting";
  error: string | null;
  url: string | null;                 // the first service's URL
  services: Service[]; resources: Resource[];
  cost: { sleeping_usd_month: number; active_usd_hour: number; month_to_date_usd: number; bytes: number };
  last_reset_at: number | null; last_active_at: number; expires_at: number | null;
  created_by: string; created_at: number; updated_at: number;
};
type Service = { name: string; repo: string; url: string; revision: string | null; deployed: string | null;
                 state: "pending" | "deploying" | "running" | "sleeping" | "failed"; error: string | null };
type Resource = { kind: Kind; parent: string; ref: string | null; mode: "branch" | "schema";
                  state: "pending" | "ready" | "resetting" | "failed" | "deleting"; error: string | null; bytes: number };
type Kind = "postgres" | "redis" | "bucket" | "nosql" | "stream" | "queue" | "tasks" | "search";
type Activity = { at: number; message: string };
type Settings = {
  enabled: boolean;
  branches: string[];                 // globs on the change's branch; ["*"]
  drafts: boolean;                    // previews for draft changes (false)
  resources: Record<string, "branch" | "schema" | "skip">;   // "<kind>:<ref>" → mode; unlisted: `data`
  data: "branch" | "schema";          // the default mode ("branch")
  reset_every_hours: number;          // 0: never
  sleep_after_s: number;              // 600
  ttl_hours: number;                  // 168
  max_previews: number;               // 10
  max_bytes: number;                  // 10 GiB
  secrets: { base: string; allow_production: boolean };      // "development", false
  env: Record<string, string>;        // overrides every service gets
  github: { comment: boolean; status: boolean; deployment: boolean };   // all true
};
```

## Surfaces

- **roll code change**: `Change.preview` carries the environment: its state, the first URL,
  `environment: {id, services: [{name, url, state}], cost_usd_month}`. A change whose repo is in
  a project with previews on gets its preview from here instead of code-preview's single
  container.
- **GitHub PR**, through roll code's GitHub outbox (durable, retried, sent on the repo's owner):
  - one comment, edited in place, with a table of services, URLs, status, data branches and cost;
  - a commit status `roll/preview` on the head;
  - a GitHub deployment to the environment `preview/<slug>` with the URL, marked `inactive` when
    the preview is deleted.
- **Dashboard**: a Previews tab on each project, with the list, status, cost, logs, and open,
  sleep, wake, reset and delete actions, plus a "Previews" overlay on the canvas that draws each
  preview's branches next to their parents.
- **CLI**: `roll preview ls|show|create|rm|sleep|wake|reset|logs|settings`.

Every one of them plays the mark states: `deploying` while creating or updating, `success` when
ready, `error` on failure, and `sleep` for a sleeping preview.

## Next

1. Build `crates/roll-previews`: store, settings, reconciler with fake adapters, router,
   `environment_of`.
2. Write rolld's adapters for Postgres, Redis, buckets and NoSQL, wire the router and the host
   routing, and add the container service runtime.
3. Report to roll code and GitHub: `Change.preview` environment, outbox job (comment, status,
   deployment). Fix code-preview's unapproved-head start.
4. Ship `roll preview` CLI.
5. Build the dashboard's Previews tab and canvas overlay.
6. Write the stream, queue and tasks adapters.
7. Measure the cost (`scripts/preview-cost.sh`) and write the numbers above.
8. Fork stream data copy-on-write (refcounted chunks in retention and compaction).
9. Make Postgres reset keep the endpoint (re-point the endpoint at a new branch), and make
   Postgres branching copy-on-write on ext4.
