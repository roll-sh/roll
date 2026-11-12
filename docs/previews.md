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
   5. The secrets environment `pv-<preview id>` is ensured (base, overrides, resource URLs).
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

Branch names are `pv-<preview id>` (a bucket's is `<parent>-pv-<preview id>`, since bucket names
are global). Branch adapters live in `crates/rolld/src/previews.rs` and implement
`roll_previews::ports::Brancher`. They call this node's own API over loopback with the operator
token, like `roll` does, so each request runs where the product keeps the resource (a Redis
database's or a stream's lease, storage's metadata owner) wherever the reconciler runs.

| Kind | Branch | Reset | Sleeping cost | Notes |
| --- | --- | --- | --- | --- |
| `postgres` | `pg_control::Control::create_branch` from the project's `main` | Delete and branch again, on the same branch name. The URL changes, and the services are redeployed with it | Its local data directory until eviction (24 h), then its own base backup in object storage | The weakest copy-on-write here: a branch is a full data directory on ext4 (reflinks on btrfs/XFS). See `## Next

Ranked by what a person notices first. Done so far: the crate, rolld's adapters for every
product, the secrets adapter, roll code and GitHub reporting, `roll preview`, the end-to-end run
and its cost.

1. **The dashboard's Previews tab and canvas overlay** (`feature/roll-previews-dashboard`, being
   built) merged here. Then the dashboard's workspace kinds for `queue` and `tasks`.
2. **Postgres in the measured run**, and Postgres branches that don't copy the data directory on
   ext4. A branch could start from the parent's latest base backup in object storage plus WAL
   (lazy pages), not `pg_basebackup`. Also: `schema` mode for Postgres (structure only), and a
   reset that keeps the endpoint, so `DATABASE_URL` doesn't change.
3. **Stream data branches** that share the parent's uploaded chunks (refcounted chunks in the
   broker's retention, compaction and `delete_stream`). Today a stream branch gets the topics,
   not the records.
4. **Several services per repo**: `[services.<name>]` in `roll.toml`, each with its own image,
   command, port and `root`, so a monorepo's web and API both run.
5. **Build caches.** Keep a service's package caches between starts, in a per-repo volume shared
   by every preview of the repo. This cuts the cold start of real apps.
6. **Spread preview services across nodes** (a lease per preview rather than one leader). Today
   one node serves every preview host in a cluster.
7. **Search branches** (roll search, `search_control::Control::branch`) when it lands, and the
   roll apps adapter (`Apps`) when `feature/roll-apps` lands.
8. **Seeds and anonymised data**: a per-resource `seed` command run once after branching, and
   NoSQL masking surfaced in the settings.
