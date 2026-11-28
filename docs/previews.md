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

- **Branch every product, cheaply.** Postgres, Redis, buckets, NoSQL, streams, queues, tasks
  and search clusters branch from their parent (see [Branches](#branches-per-product) for how
  each one branches). A branch of a 100 GB project starts in seconds and stores only what the
  preview writes where the file system shares blocks; Postgres on ext4 copies its data
  directory (the table below says where).
- **Put the whole preview to sleep, not just the app.** Services freeze after `sleep_after`
  without a request (paused, so the next visit is back in tens of milliseconds), and stop
  for good a working day later (`ROLL_PREVIEWS_WARM_MINUTES`, 480). Postgres computes, Redis, NoSQL, queues and search unload on their own
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
  - With `skip_authors` set (globs on who opened the change: a pull request's GitHub login, so
    `dependabot[bot]`, or `*[bot]` for every bot account), a change by one of them gets no
    preview, as Netlify's and Vercel's users skip dependency bots with an ignored build step. A
    `[preview]` title or a `preview` label still asks for one (a Git URL service's pull requests carry no labels, so there only the title asks). A Git URL service's pull requests
    on roll apps carry their opener too (roll apps' `GitPull.author`: GitHub's `user.login`, GitLab's `user.username`; Bitbucket's none yet), so the rule reaches them as well. A bot pushes its branch before it opens the pull request: when the push says who pushed (roll apps' `GitPush.pusher`), that push makes no preview; when it doesn't, the pull request takes back the preview that push made, and the branch's later pushes make none until the pull request closes, the branch is deleted or its title asks.
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
- **One voice per pull request**: a Git URL service can be on two projects' canvases, the
  operator's and an admin's, with each project standing for it. Then each project that wants a
  branch gets its own preview of it, but only one posts `roll/preview` and the pull request's
  comment.
  - **Who posts:** of the projects with an active preview of the branch, the one that has had the
    service on its canvas longest (ties go by id). roll apps puts a service on the canvas of the
    project it's made in, so normally that's the service's own project. A project whose rules skip
    the branch has no preview of it, so it never silences the one that does.
  - **The quiet preview** says so on its sheet (`reported_by`). On the operator's projects that's
    the speaker's name; on a person's project it's "another project", since a person never learns
    another owner's project. Its row in the Previews tab carries a `quiet` tag, and its chip on the
    canvas says so on hover.
  - **Handover:** when the speaking preview goes (the service leaves that canvas, or its project's
    rules drop the branch), the next preview's own node reports at once. The comment is kept with
    the pull request, so the next preview edits the same one, and the preview that went doesn't
    say so (that could land after the next one's).
  - **A roll code change** has one preview, made by the first project with previews on that
    stands for its repo. roll code's `POST /v1/code/changes/{id}/preview` picks the project that
    has had the repo on its canvas longest.
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
- **Production's browser checks** (Vercel's Deployment Checks with Checkly): roll apps runs a
  service's production browser checks (`checks[].browser`, a Playwright script) once on each
  preview head's deployment as it goes live (#790). The preview reads them from the `pv-`
  environment's `check_states`, a result counting only when it's of the live deployment, each
  pass from when the head is up until each has run, or 5 minutes after; they never hold `ready`
  up. They're on the record (`browser_checks`), the sheet (what failed, the check's own words
  made printable), `roll preview show` and the pull request comment ("Production's browser checks
  on this head: **1 failed** (`web: checkout`), 2 passed": names only). The screenshot and trace
  of a failure stay roll apps' (`read` on the service): the record names where (`files`), and the
  sheet opens them as the person looking, as roll apps' own Settings does (someone without `read`
  on the service gets nothing). The project's `browser_checks` rule:
  `report` (the default) shows them; `check` also posts `roll/browser-checks` on the head (pending
  while one runs, failing while one failed: for GitHub's branch protection); `required` also keeps
  the change from landing in roll (a `browser` blocker in `Code::blockers`, from the project's rule,
  never the change's) until they pass at its head. Live: `scripts/preview-apps.sh` gives production two
  browser checks, one the preview's page passes and one whose script throws, and the preview
  reads `home` passed and `cart` failed with its error.
- **Earlier pushes** (Vercel's per-push URLs): when a change's own service is redeployed at a new
  head, the head it ran stays reachable at `<service>-<head>-<slug>`, on the preview's same data,
  behind the same link, asleep until opened; the last 3 per service (an older one's container
  goes). For comparing before and after. A trusted change's only (an approval is for one head).
  Each runs on its own data: before the next head's seed or release, each branch (Postgres,
  Redis, search, NoSQL, bucket, stream) is branched again, all at once (measured 2026-11-23 under load 35–58: the push's setup step 3.8–3.9 s against 5.1–5.9 s one after another) (copy-on-write, masked as the preview's is) as that head left it, and the
  earlier push connects there, so a migration that renamed a column doesn't break the code
  before it. A copy that fails (its product down, a stream's tail slow to reach object storage)
  doesn't hold the push up: it's said in the activity, and that push shares the preview's data
  for that product (`own_data` names only the copies it has). An evicted push's copy that can't
  be deleted is said and deleted later (the next eviction, or the teardown); a reset, changed masks
  and the teardown wait for theirs. A copy's bytes count in the preview's cost, budget and `max_bytes` with its
  branches'. A copy goes with its push, and with the data it copied (a reset, or masking rules
  that changed: never served less masked than the rules say). Products that can't branch a
  branch share the preview's. A NoSQL copy inherits its branch's masking policy (a read policy,
  not a rewrite). A roll apps service's earlier push is that build started again, taking no
  traffic, with its copy's variables (roll apps' `POST …/deploys/{dep}/rollback {no_traffic,
  variables}`: names the environment has, sealed with that deployment); if that fails, the build
  itself on the preview's data (said in its activity, its copies dropped), and when a reset or
  changed masks drop its copy, the push goes with it (its variables can't fall back). Either way
  it's at a deployment's own URL (`<deployment>--<slug>` on roll apps' domain, never a project's preview domain;
  roll apps' `Deployment.url`), behind the same link (the token once, a cookie for that host) or
  sign-in: asked before the new head deploys, and kept only while the running build is the head
  it ran. roll apps keeps an environment's last 10 builds, so the 3 kept here stay; one it pruned
  answers roll apps' 404, not previews' 410 page.
  They're on the sheet, `roll preview show`, the change and the pull request comment.
  A link to one that's no longer kept says so: its host answers 410 with a page naming the
  service and the push and linking the service's latest (on roll's preview domain and on a
  project's own, there only for that project's previews; the latest 30 a preview evicted, gone
  with the preview), not a 404 that reads as a broken preview. On roll's runtime, a trusted
  change's private preview's page also starts it again on demand (Vercel keeps every
  deployment's URL): its "Start this push again" opens `/__roll/start-push` on that host, through
  the preview's own gate (the link's token it carried, or roll's sign-in for team access), a page
  that asks; only that page's own POST (this host's origin), from a visitor in by the link's
  cookie or a person's session (not a share link, not the CI bypass), and at most once a minute
  a preview, keeps it again in place of the oldest kept push of that service (whose container goes),
  at the same link, and the next request starts it, on the preview's data (its own copy went
  when it was dropped). A fork's earlier heads never start again
  (`engine.rs::earlier_pushes_stay_reachable_at_their_own_urls`,
  `runtime.rs::service_hosts_reach_their_containers`).
- **A page loaded before a push keeps working** (Vercel's skew protection; roll apps'
  `run.skew_protection`): with `skew_protection = true` in `[preview]`, a request that names an
  earlier push goes to that push's own service (above), past the same gate, while the push is
  kept and for a day after it was replaced. A request names it with the `x-deployment-id` header,
  a `dpl` query parameter, or the `__roll_dpl` cookie every page load gets. The app knows its own
  push as `ROLL_DEPLOYMENT`, for its asset URLs and fetches (Next.js's `deploymentId`). A page
  load always gets the current push and a new cookie, so a reload moves a visitor on. The cookie
  is roll's and never reaches the app. A single-page app left open while someone pushes keeps
  loading its own chunks and calling the API it was built against, instead of failing with
  "failed to fetch dynamically imported module". This covers a trusted change's services on the
  runtime, the only ones whose earlier pushes are kept. A roll apps service follows its own
  `run.skew_protection` in its `pv-` environment.
- **From the pull request or roll's change** (Bunnyshell's and Uffizzi's comment commands):
  someone who can write to the repo comments, in the pull request's conversation, an inline
  review comment or any of the change's threads in roll, `@roll preview redeploy` (build and deploy it again), `reset` (its
  data branched again from the parents), `wake`, `sleep` or `keep [hours]` (past its TTL, 24 when
  not said, at most 720); roll answers where it was asked with what it did, and the preview's
  activity names who asked ("@octo asked for its data reset from the pull request"). Readers and
  strangers get nothing, as with every `@roll` command (roll code's GitHub permission check).
- **Watch paths** (Railway's watch paths, Render's per-service build filters): a `roll.toml`
  service lists the paths whose change rebuilds it (`[preview] watch = ["web/**",
  "package.json"]`, gitignore-style as `paths` reads them; at most 50, no `!negation`). A push to
  the change that touches none of them, nor `roll.toml`, keeps the build the service runs: no
  build, no redeploy, and a start at the new head runs it ("kept web's build at 3f2a…: the push
  to 9c1d… touched none of what it watches", in the activity). A path list that can't be read, a
  pattern that doesn't parse, a deploy hook's rebuild or a fork's change (its approval is for one
  head) rebuilds it as before. Like an earlier push, the kept build runs on data the new head's
  release may have migrated. Empty (the default): every push rebuilds it.
- **Crons**: a roll apps cron service, and a service's scheduled commands and calls, are deployed
  in a preview like anything else, but the schedule doesn't fire unless the project's `crons` says
  so (roll apps' `scheduled: false` on each `pv-` environment): a preview nobody looks at runs no jobs, and a nightly digest doesn't mail anyone
  from its copy of the data (Heroku's review apps leave the scheduler out too). A roll apps
  without the field refuses it as unknown and the environment is made without it (its schedule
  runs, as before).
- **How it came up** (Vercel's and Render's deploy steps): each head's timeline is on the
  preview's record (`timeline`: when converging it started, and when each step first finished:
  `data`, its branches made or kept and masked; `copies`, when it took any, the earlier push's
  own copies of them; `setup`, seed, release and schema read; `deploy`,
  every service live; `checks`, health paths answered; `ready`) and the sheet ("Up in 9.4 s: data
  4.1 s · setup 1.2 s · deploy 3.0 s · checks 0.4 s", the steps so far while it's on its way). A
  push starts a new one; a push that keeps the services shows how little it took.
- **Warm after a push**: once a new head is deployed, the change's own services are woken (after
  the pass, a few at a time), so the first visit, usually the pull request comment's link,
  doesn't wait for a cold start; they sleep again after `sleep_after` unused. Not a preview
  someone put to sleep, and only on the node that holds its lease. About $0.002 a push here;
  `warm: false` turns it off.
- **Sleep**: a service with no requests for `sleep_after` (10 minutes by default) is frozen
  first: its container is paused (`docker pause`), keeping its memory (12–22 MiB for the
  measured service) and using no CPU, and it reads and costs as asleep. A visit in that window
  thaws it: 48–72 ms to the first response on this machine, where a cold start is 3.3–3.9 s.
  After `ROLL_PREVIEWS_WARM_MINUTES` (480, a working day; 0 stops at once) it's stopped for good,
  and a node keeps at most `ROLL_PREVIEWS_WARM_MAX` (32) frozen, stopping the oldest past it, so
  the window costs no more memory than 32 frozen services however long it is. A frozen service
  doesn't hold its data up: measured (2026-11-17), the preview's Postgres branch suspended within
  five minutes of its services freezing and stayed suspended while they were frozen, so a longer
  window keeps the next visit at tens of milliseconds without keeping a database running (the
  window was 30 minutes until then). A
  request that lands just as a service freezes thaws it again at once. A frozen service keeps
  its dynamic credentials renewed (its process comes back with them). Docker only: Fargate has
  no pause, so services there stop. Sleep asked for (`roll preview sleep`, the sheet's Sleep)
  stops at once. Otherwise a request to any URL of the preview wakes it. It's held for up to
  60 s; after that the visitor gets a "starting" page that refreshes. Data products sleep on
  their own and wake on connect. `scripts/preview-cost.sh` checks a frozen service thaws (the
  same container, within 1.5 s).
- **Reset**: `reset_every` (`0` off, or hours) re-branches every data resource from its parent's
  current state on a schedule. `POST /v1/previews/{id}/reset` does it now, for all resources or
  the ones named.
- **Restore points** (Neon's snapshots, Railway's volume backups): the preview's data saved as it
  is now and put back later, after a test run, a demo or an agent changed it. `POST
  …/restore-points` `{label?}` saves one: a copy-on-write branch of each branch whose product
  can put a branch back (`Brancher::restores`: Postgres, pg-control's reset with `from`,
  so the branch keeps its endpoint, `DATABASE_URL` and role; NoSQL, a zero-copy branch cut at a
  whole second, put back by NoSQL's point-in-time restore of the preview's own branch to that
  second, in place with the same id, endpoint and credentials, within its 7 days of history:
  what was written in the fraction of a second before saving isn't in the point; Redis, the same
  way, through redis-control's branch `at` and its restore in place, within the database's
  `history_retention_s`: the same id, `REDIS_URL` and password, its clients disconnected;
  streams, a branch of the preview's stream (its chunk rows name the same objects), put back by
  branching that copy again under the preview's stream's name, since a stream's offsets only move
  forward: a new stream id and key, so its services are deployed again with them, and the point
  outlives a reset), at most 3 per preview. `POST
  …/restore-points/{point}/restore` puts the data back on the next converge, as a reset does:
  the services sleep meanwhile, and the seed doesn't run again (the point holds what it made).
  A reset that makes a new branch (Redis's, a new database) deletes the points' part of it: they
  go back only into the branch they were saved from (`Brancher::points_need_their_branch`).
  A point is masked as the branch was. New masking rules delete the points masked by the old
  ones, and a point is never put back under rules other than its own. A resource moving to
  structure-only, or no longer wanted, takes its points with it, and the preview's deletion
  deletes them all. Saving and putting back say who asked in its activity. Search branches and
  buckets aren't saved yet: re-branching a search copy would chain its browser keys off the
  copy's (roll search adds a `reset {from}` in place, its Next), and roll storage refuses to
  delete a bucket that has branches, so a bucket needs a put-back on roll storage's side (Next). The sheet's Data branches has them (Save now, Restore, Delete);
  `roll preview points <id> [--save [label] | --restore <point> | --delete <point>]`. Checked
  live in `scripts/preview-cost.sh` (a table made after the point is gone once it's put
  back, on the same `DATABASE_URL`). Its services sleep for it and, as after a reset, the preview
  settles asleep when it was idle (the next visit wakes it) or ready. Measured on this machine
  (2026-11-24): asked to ready again in 12.8 s, the Postgres branch back in 5.3 s of it (its
  compute stopped, the point copied in, started again) and the services healthy at 12.3 s. `a_restore_point_puts_the_data_back_without_seeding_again`,
  pg-control's `branches_are_isolated_copies` (a reset from another branch).
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
  - `max_bytes` (10 GiB by default) is a cap on the bytes a preview's branches and its earlier
    pushes' copies own. A preview over the cap goes to sleep with `error` and stays asleep until
    resetting it brings it back under or the cap is raised: a wake (`POST …/wake`, the sheet) is
    a 409, roll apps' services included, and a visit doesn't start its own services. Its roll
    apps services are kept down there too: their `pv-` environments are paused (`PUT …
    {paused: true}`, Render's Suspend), so a visit gets roll apps' "Paused" page and wakes
    nothing, deploys and jobs wait. Under the cap again, its next converge or a wake resumes
    them (only what previews paused: `held_down`). Live: `scripts/preview-apps.sh` caps a
    preview's 2 MiB Redis copy at 1 MiB, a visit to its service answers 503, and a wake under a
    raised cap lifts the pause. `a_preview_over_its_size_cap_stays_asleep`.

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
   3. Every data resource without a branch is branched, as `pv-<preview id>` on its parent, all
      of them at once (each records its own progress; the first error is the converge's once
      all are done: 14 s from change to ready with eight products, against 16.6–18.4 s one
      after another). The
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
       How long it took is kept (`release`: `{head, ms, before_ms}`) and said on the sheet, the
       change page and the pull request comment ("Its release (migrations) took 2.4 s on the
       preview's data, 1.3 s slower than the push before"), since the preview's data is the
       parent's: a migration that's slow here holds its locks on production as long
       (PlanetScale's deploy requests show this before a merge). The time is the whole job's,
       container start included (live: the change carries it).
       The locks it held that make others wait are said beside it (PlanetScale's safe migrations,
       squawk's lint, measured instead of guessed): while the release runs, each Postgres branch's
       `pg_locks` is read every half second (`Brancher::locks`, as the superuser, the catalog's
       names only, other sessions' granted locks on user tables), and the strongest per table from
       `SHARE` up (writes wait; `ACCESS EXCLUSIVE`, reads too) is kept with how long it was seen
       (`release.locks`, `[{table, mode, ms}]`, 10 at most): "It held ACCESS EXCLUSIVE (reads and
       writes wait) on `public.orders` for 41 s: on production it would hold them as long", in the
       comment and on the sheet (`LOCKS_SQL` checked against `postgres:18-alpine` with a session holding `LOCK TABLE …
       ACCESS EXCLUSIVE`. The live script's release doesn't take one: on ai-server a job's
       container can't reach the host's Postgres, ufw drops Docker's bridge to the host).
       A preview's data is as old as its branch (days, on a long-lived change), so the release
       can be rehearsed on today's: `POST …/rehearse` (`roll preview rehearse`, the sheet's
       "Rehearse on today's data", 202) asks the next converge, at the change's head, to branch
       each Postgres parent as it is now (`<preview>-r`), mask it by the project's rules before
       anything connects, run the release once more with those branches' variables in place of
       the preview's (by value: a branch's URL is its own role's; the ones roll makes from it,
       `PGHOST`, `PGUSER`, `PGPASSWORD`, `PGOPTIONS`, and a link's names made from them, too, so a
       release that reads those never runs on the preview's own branch), then delete them
       whatever happened. `rehearsal` (`{head, by, at, ok, ms, error}`, `ok` null while it waits)
       says how it went beside the release's time, at the head it's for only (one asked at an
       earlier head never runs, and an earlier head's outcome isn't shown); a failure is said
       (masked, one line), never the converge's, and it runs once a head. With
       `release_checks` (`check` or `required`, off by default) roll rehearses it at every head of
       a change instead, queued or not, and with `required` the change can't land in roll until,
       at its head, with its preview settled there (main's code runs before it settles), the
       rehearsal passed and main's code didn't break on its migrated data (step
       4e; `release` on the change's blockers: "its release hasn't been rehearsed on today's data
       at its head yet", "its release failed on today's data: …", "main's code breaks on its
       migrated data: /orders (500): expand first, contract in a later change"). Rehearsed at each
       head before it's queued, the blocker never waits on the queue that would ask for it; a
       preview with no Postgres branch has nothing to rehearse, and a check of main's code that
       couldn't run (no preview of main, a branch that can't be copied: its `error` on the preview),
       has nothing to say, so neither holds anything back
       there (`queue.rs::blockers_keep_changes_out_of_the_queue`, the `release` case). With
       `rehearse_when_queued` (off by default: each is a fresh branch, its masking and a release
       run) roll asks for one itself when the change is queued to land in roll code, at that
       head: the change becoming queued wakes a settled preview at once, before the queue lands it
       (`engine.rs::a_rehearsal_runs_the_release_on_a_fresh_masked_branch_then_drops_it`).
   4d. Once per head, the Postgres branches' schema is compared with their parents' (see
       "Schema changes").
   4e. Main's code on the change's migrated data (pgroll's and Reshape's expand/contract,
       tested on real data instead of assumed): while a change deploys, the version before it
       still serves, on the schema its release just changed. So once per head of a trusted change
       whose release changed the schema, after its services are up and healthy, main's code runs
       again, taking no traffic: on roll apps, main's live build of each of main's services (roll
       apps' rollback with variables, as an earlier push's own copy does); on roll's runtime,
       main's revision of each of the change's repo's services, in the change's preview as
       `<name>@main` (the other repos' services run main's code there already), at a host no
       visitor reaches (`?dpl=` and skew cookies name pinned pushes only), its variables held in
       memory while it runs (they're the copy's credentials) and stopped after. Both run on a copy of every one of the change's
       branches as the release left them (`<preview>-m<head>`, copy-on-write, masked as the branch
       is; named by the head, so a copy a crash left is never an earlier head's), never on the
       change's own (a second consumer on its queue, writes to its bucket): a branch its product
       can't copy stops it, saying which. Main's own branches' variables are swapped for the
       copies' by value (and the ones roll
       makes from them). Its pages, `/` and the project's `github.pages` (10 at most; the other
       services `/`), are read there and on main's preview; a page that answers on main's data
       (below 500) but not on the change's (a 5xx, or no answer) broke. The copies go after,
       whatever happened. `main_on_change` (`{head, at, ms, pages: [{service, path, status,
       main_status}], error}`) has it at that head, the sheet and the pull request comment say it
       ("Main's code breaks on this change's migrated data: `/orders` answers 500 (on main's
       data, 200)… expand first and contract in a later change"), and the activity too. Only a
       trusted change: main's build reads the copy with main's (or the preview's) secrets. Where it
       can't run (no preview of main, a branch that can't be copied) `error` says
       why and nothing is said on the pull request.
   5. The secrets environment `pv-<preview id>` is ensured (base, overrides, resource URLs).
   6. Every service whose revision is not the one it should run is redeployed (the change's head
      for its repo; for the others their branch of the same name, else their default head), all
      of them at once. Every outcome is kept before the first failure fails the converge, so a
      service that deployed isn't deployed again. Measured on 2026-11-18 with two roll apps
      services, six interleaved pairs in both orders under 32–79 load: a push with cached builds
      is live again in 4.2 s against 5.8 s one after another (medians; at once won 5 of 6, the
      sixth a tie). A first push with cold builds is no faster (CPU-bound: two builds share the
      cores), within the noise in both orders.
   7. The preview becomes `ready` (or `sleeping`), and the change and the PR hear about it.
   Every write that asks for a converge (a push, a delete, a reset, a refresh) bumps the row's
   `generation`. A converge settles a row only if its generation hasn't moved since it started,
   so a delete or reset that lands mid-converge is never lost. A branch the product made whose
   id never got recorded (a crash, a timeout) is found by its name when the preview is deleted.
3. A step that fails records its error on the preview (`failed`) and is retried with backoff
   (15 s × 2ⁿ, at most 10 minutes). Nothing else is undone.
4. A preview that goes (landed, closed, stale, deleted) first runs the project's `teardown`
   once, the way the seed runs, before its services, secrets environment and branches are
   removed, like Heroku's `pr-predestroy`: what the preview's code made outside roll with its
   own keys (a payment provider's test webhook, an OAuth redirect URI, a DNS record) is cleaned
   up while everything it needs is still there. It runs only at a head the preview ran (a
   fork's push nobody approved never runs it; the approved head before it does), not again
   when a removal after it is retried (a crash during the job itself runs it again, so make it
   safe to repeat). A failure doesn't keep the preview: it's in rolld's log and the activity, and
   the change is told as the preview goes (`Code::report_environment_gone`): the `preview` event
   carries `teardown` (its exit code and output's tail) and the pull request's comment reads
   "deleted. Its teardown failed:" with that output in a fence it can't close.

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
| `tasks` | A new tenant with no messages and no schedules | Delete and create again | Its own SQLite file | Copied schedules would call production endpoints, so they never are. Its services get the tenant's own URL, token and webhook signing keys (`ROLL_TASKS_SIGNING_KEY`, `ROLL_TASKS_NEXT_SIGNING_KEY`, read at each start), so its deliveries verify; roll apps' `tasks` link (#792) names them. |
| `search` | `search_control::Control::branch` (zero copy) | Delete and branch again | Segments it wrote | Field masks rewrite the branch's documents, then expunge what they replaced; mapping changes are schema changes. |

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
    `{branch: null, ephemeral: true, report: false}`. `report: false` (apps' #249) keeps roll
    apps from posting its own commit status and GitHub Deployment for the environment's deploys.
    The pull request then hears one voice: previews' `roll/preview` and comment. A roll apps
    older than that, such as a node not yet upgraded on a fleet, refuses the field. Then the
    environment is made without it, and both report.
  - The branches' URLs and the project's overrides are set as its variables.
  - Every variable of a link to a branched resource comes from the branch, under roll apps'
    names (a stream's `KAFKA_BROKERS`/`KAFKA_USERNAME`/`KAFKA_PASSWORD`, a bucket's
    `AWS_ENDPOINT_URL`, a search cluster's `ELASTICSEARCH_API_KEY` or `OPENSEARCH_URL`/
    `OPENSEARCH_USERNAME`/`OPENSEARCH_PASSWORD`: the branch's data-only key or user, made with the
    branch through roll search's keys route (`access: data`), with `ELASTICSEARCH_URL` then carrying
    no login, so code that needs more than production's link fails in its preview too), or is set blank when
    the branch has nothing for it (a Redis branch gives its `REDIS_REST_URL`/`REDIS_REST_TOKEN`
    too). Never production's: until 2026-11-16 a name that differed
    from the branch's kept production's value, so a stream's or an OpenSearch cluster's preview
    reached production's data. `a_links_variables_come_from_the_branch_or_are_blank`.
  - It is deployed at the change's git commit (or its image, for an image service), with an
    `Idempotency-Key` of `pv-<id>@<sha>`, so a retry after a crash returns the same deployment.
    When the environment already runs that commit (a reset's new URLs), it's redeployed: the
    same image with the new variables, no build.
  - roll apps injects the roll secrets environment of the same name, builds once, scales to
    zero, and answers on its own hostnames (`<slug>.up.<site>`). A preview's service URLs are
    those.
  - Deleting the preview deletes the environments.
- **`roll.toml` containers**, when the project has no roll apps services. The services are the
  project's repos whose `roll.toml` has a `[preview]` section, named after the repo. A repo with
  a Compose file and no `roll.toml` gets its services from roll apps' Compose import (`roll apps
  import-compose`, New service's "A file"; docs/apps/README.md), whose services then come into
  each preview through their `pv-` environments: previews read no Compose file themselves, so the
  two never disagree about one (the Previews tab's empty state says so)
  (`[preview] name = "api"` overrides that). Each runs in the container `roll.toml` describes,
  through code-preview's `Isolation`: Docker here, a Fargate task on the fleet.
  - With `[[preview.states]]` (a name, a page and steps, as `github.states`), `pages =
    ["/pricing"]` and `hide = [".clock"]`, the repo names the page states its screenshots reach,
    more of its own pages and what to blank in them, versioned with the code they name: read from
    its `roll.toml` at the revision the first service runs, with the project's (a state name the
    project has wins), and refused with the rest of the file when they break the same rules
    (live: a repo page and a repo state compared with main's). A service on roll apps takes them
    from its `[app.previews.screenshots]` instead (roll apps describes it, and leaves that table
    to previews: agreed with the apps thread, its 60dd4071a), read from `roll.toml` under the
    service's root at the revision it runs, never the catalog's copy of the default branch; a
    table that breaks the rules is said in the activity.
  - With `health = "/healthz"` in `[preview]`, a service counts once it answers 2xx/3xx there
    (Render's and Railway's health checks), asked once per deploy (it wakes the service once). A
    service that answers 500 fails the preview with the status and the first line it said, so
    `roll/preview` and `roll preview wait` don't pass on a broken page; the retry asks again.
    A converge waits for it a few seconds at most: one still installing or booting is asked
    again on a later pass (for up to 20 minutes), so no converge slot waits on a build. roll
    apps services are checked by roll apps.
  - **Log drains.** A project that sends its logs to a drain (roll apps' log drains,
    `previews: true`: Datadog, Better Stack, a collector) gets its previews' `roll.toml` services
    there too, as Vercel's and Render's drains take previews' output. roll apps sends its own
    `pv-` environments' instances' output itself; for a `roll.toml` service the preview's holder
    follows its container (`docker logs -f --timestamps --since`), masks each batch as the
    sheet's logs are, and hands it to roll apps' drainer (`Apps::drain_preview_lines`), so the
    lines get the drain's queue, signature, format and limits. A line's `environment` is the
    preview's `pv-` one, `service_name` the service, and `service`, `instance` and `deployment`
    are null: a drain narrowed to roll apps services never gets one. Each running service has
    one follower, only while a drain wants previews' output (asked every pass, read again
    every 30 s). A follower after a restart or a redeploy starts at the stamp of the last line
    sent and drops what Docker gives again, so nothing is lost or sent twice. A process's first
    follower starts when the process did, and lines whose masks can't be read aren't sent.
    A preview's jobs (`release`, `seed`, `teardown`) go to drains of builds (`sources`
    `build`, where Render and Vercel put deploy-time commands): each job's output tail once it
    ends, masked with what it was given and then as logs are, the job's name its
    `service_name`.
    `a_previews_runtime_output_goes_to_the_projects_log_drains_masked`,
    `a_previews_runtime_lines_reach_only_drains_that_take_previews`.
- The tree is copied into the container, never bind-mounted.
- Each service answers at `<service>-<slug>.<domain>`.
  - `<slug>` is `pr<number>` for a GitHub PR, `c<number>` for a roll code change, or the branch
    name for a branch preview, then `-<project name>`.
  - Example: `web-pr482-shop.preview.roll.site`.
  - Labels are cut to 63 characters, and a clash gets the preview id's last 4 characters.
  - The domain is rolld's preview domain (`--code-preview-domain`), served on the API port like
    roll code's own previews.
- A project with its own preview domain (roll apps' project preview domain: a verified wildcard
  custom domain, `*.preview.acme.dev`) has its previews answer at `<service>-<slug>.preview.acme.dev`
  instead (Vercel's preview deployment suffix), so cookies scoped to the team's domain and OAuth
  redirect allowlists work.
  - **Who serves what:** one wildcard record (`*.preview.acme.dev`) points at roll apps'
    ingress. A `pv-` environment says its label (`preview_host`), and apps serves that name.
    Apps hands any other name under the domain to previews' router, in process (rolld wires apps'
    `PreviewRouter`, #278), which serves a roll-runtime service, and only for the project that
    holds the domain. Previews answers the 404 for a name it doesn't hold. rolld's API port
    serves the same names too (a node without apps' ingress).
  - **Team access:** a ticket (`POST /v1/previews/{id}/visit`) to a name under the domain lands
    on that name, at the domain's scheme; the preview's name under another project's domain gets
    none.
  - **Changes:** the domain is read from roll apps at most once a minute. When it's verified,
    changed or removed, the project's runtime services' URLs move and every preview reports
    again, so the pull request's links follow.
  - **TLS:** plain HTTP for now, on a local node or behind the user's own TLS proxy. TLS waits on
    wildcard certificates over DNS-01 (roll apps' "Wildcard certificates", on the edge).
- A service gets this environment, later entries winning:
  1. its `roll.toml` `[preview] env`;
  2. the resolved secrets environment (`pv-<preview id>`, inheriting its base);
  3. the project's preview `env` overrides;
  3a. the project's per-branch variables (`env_for`: a branch glob → variables, every glob that
     matches the preview's branch, in the globs' order; Vercel's per-branch preview variables);
  3b. the preview's own variables (`PUT /v1/previews/{id}/env`, the sheet, `roll preview env`):
     Vercel's branch-scoped variables. Setting them redeploys its services; names roll or the
     preview's branches set (`ROLL_*`, `PORT`, `DATABASE_URL`, …) are refused, so they can't
     point a preview at other data. With `ROLL_SECRET_KEY` they're sealed at rest (AES-256-GCM,
     bound to the preview's id: roll's reviewer's sealer), so the previews table holds no value
     and a copy on another row doesn't open; ones written before a key read as they were and are
     sealed at their next write. A node without the key that finds them sealed fails the
     preview's converge rather than start its services without them
     (`a_previews_own_variables_are_sealed_at_rest_to_it`);

  A value of `env` or `env_for` can name a key in roll secrets instead of holding it (Doppler's
  and 1Password's references): `roll://ENV/KEY` or `roll://ENV/folder/KEY`, in the project's own
  environments (no `PROJECT@`: 400). It goes into the preview's environment as roll secrets' own
  reference (`${ENV.folder.KEY}`) and is read there, with the environment, as the preview reads
  everything: untrusted, so a protected environment or a sensitive key never resolves (it's left
  out and logged as a secret that doesn't resolve), a branched parent's link is followed as any,
  and the value is masked in what the preview shows. A payment provider's test key then stays in
  roll secrets (its health, leak scanning, reach and rotation) rather than in the project's rules.
  A rule naming an environment other than the one the previews follow (`secrets.base`) routes
  its values into code whoever pushes a branch controls, so a person choosing that reach must be
  able to reveal it: saving a new or changed reference needs its writer's reveal on that
  environment's folder in roll secrets (roll secrets' own answer for the request's caller, as roll
  apps asks it; 403 otherwise), and who wrote each is kept (`env_refs`). At every converge each
  preview gets such a reference only while its writer still may choose it for that preview: roll
  secrets reads it as them (`Secrets::digest_as`, its #407, named for the preview's project and as
  its repo, untrusted for a fork's code; no value, not audited), so a writer who can no longer
  sign in, lost the project or its reveal, a revoked token, a key made sensitive since or a
  protected environment for an untrusted preview leaves it out, said once in the preview's
  activity ("references left out (who chose them can't now): K: …"). Saving the rules again as
  someone who may brings it back. A service start asks too: one whose reference lost its writer
  since the last converge waits for the converge that leaves it out (`refs_still_allowed`), so it
  never reaches a service in between. A Git URL service's preview is read as that service (its
  place on the canvas), every other preview as its repo. A deleted preview's own references'
  setters go with it. A reference to the base needs none of this, but changing the base needs its writer's
  reveal on the new one (every preview's code then reads it). The rules are written only as they
  were checked: another request's change in between is a 409.
  Moving the base checks the references that named the old one as new. A value that reads as a
  reference only once trimmed or lowercased (` roll://…`) is refused. Who wrote each reference
  is kept, not yet checked again later: a writer who loses reveal leaves their reference working
  until roll secrets' `bind_ref` binds it to them (its #407).
  The rules dialog offers a key in roll secrets for a variable whose name reads as a secret
  (`STRIPE_SECRET_KEY`, `GITHUB_TOKEN`: the same rule as roll apps' sealed-by-default names) and
  whose value isn't a reference: one click writes the value to the base environment under that
  name (roll secrets' own write, so its rules and audit apply) and puts `roll://BASE/NAME` in
  the rule, saved with the rest. The value then lives in roll secrets only. A protected base refuses
  the write (`approval_required`): the button opens a change request with it instead (as roll
  secrets' Find does), and the rule keeps its value and says so until it's approved.
  Only references someone was checked for go to roll secrets as references, as their own input
  to rolld's ensure: the rules' (when saved) and a preview's own variables' (`PUT …/{id}/env`:
  its setter's reveal on that folder, 403 otherwise; the base needs none; kept as its writer
  under `preview:<id>` in `env_refs`, which saving the rules leaves alone). Both are read as their
  writer at every converge (`digest_as`, above). Every other value previews write, anything
  stored before included, is plain text: its `${` is escaped, so it's never a reference by
  accident and reads back as written. A look-alike (` roll://…`, `ROLL://…`) is refused (400). Renaming an environment rewrites roll
  secrets' references, not these rules: the next converge logs the old name as one that doesn't
  resolve. `a_roll_reference_becomes_roll_secrets_own_and_a_plain_value_stays_plain`,
  `a_rules_reference_to_another_environment_needs_its_writers_reveal_there`;
  `scripts/preview-cost.sh` reads a staging key through `roll://staging/…` in the live service's
  environment, and a `${USER}` left as it is;
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
     | Search | Only a data-only credential of the branch, as production's link gets (roll search's keys route, `access: data`): `ELASTICSEARCH_URL` with no login and `ELASTICSEARCH_API_KEY`, or on OpenSearch the data user in the URL and `OPENSEARCH_USERNAME`/`OPENSEARCH_PASSWORD`. When the parent has browser keys (a site that searches from the browser), the branch gets keys of its own after them (roll search's `…/browser-keys/from-parent`) as `ROLL_SEARCH_BROWSER_KEYS` (JSON; the one to read: `ROLL_SEARCH_BROWSER_KEY`, `ROLL_SEARCH_BROWSER_KEY_ID` and `ROLL_SEARCH_SIGNING_SECRET` are only set while the parent has exactly one), hidden in logs, errors and pages as the data key is (each JSON field on its own). Previews' environment rules don't interpolate (`${…}` stays as written), so a site's build reads `ROLL_SEARCH_*` itself rather than through a rule. After every deploy (`Brancher::serve`, best effort: a problem on the preview, never a failed pass) the branch's allowed origins become its parent's plus the preview's pages' origins, off when the parent's are, and its copies of parent keys revoked or rotated since are revoked; they're all revoked with the data key, ones never recorded too. An earlier push's pinned head reaches the preview's branch at its own URL, which is among those origins; one with its own copy of the data has that copy's origins set to its own URLs. Never the branch's own login: no data credential, no link (the branch fails and is made again). A branch whose link held that login (made before 2026-11-21) is branched again at the next converge. roll search keeps a deleted cluster restorable with its keys and password, so before a search branch is deleted or reset its data key is revoked (`Brancher::retire`) and its password changed (no change, no delete). `a_search_branch_gives_only_a_data_credential`, `a_search_branch_gets_browser_keys_of_its_own_for_the_previews_pages` |

     A search branch starts with its parent's IP allowlist (`ip_allowlist`, search's #243). Masking
     a branch goes through roll search's own console route, so it works whatever the list says.
     A preview's services reach the branch as any client does, so a cluster with a list must
     admit the addresses roll's hosts send from: on one node, the host or the Docker bridge's
     gateway (search's architecture.md, "Client addresses and IP allowlists"). On roll cloud there
     are no fixed outbound addresses yet (apps' Next), so a cluster that previews' services call
     keeps `[]` for now. roll adds no exception for its own instances: one would let every
     tenant's app past every list.

     With two resources of a kind, the first one takes the plain name and the others add
     `_<NAME>` (`DATABASE_URL_ANALYTICS`).
  5. `ROLL_PREVIEW`, `ROLL_PREVIEW_URL`, and `ROLL_PREVIEW_URL_<SERVICE>` for each service, so
     services find each other; `PORT`, `ROLL_REPO`, `ROLL_CHANGE` and `ROLL_CHANGESET`;
     `ROLL_DEPLOYMENT`, the push, which a request names to reach it again (skew protection).
- **Shared images and caches.** Images are pulled once per node. Each repo has one package cache
  (a Docker volume `roll-previews-cache-<repo>`, at `/roll-cache`), shared by every preview of
  the repo. npm, pnpm, yarn, bun, pip, uv, cargo and go are pointed into it, so the second
  preview installs from what the first downloaded. The cache goes with the repo's last preview.

## Git URL services on roll apps

A roll apps service built from a Git URL (roll apps' `Source::Git`, public or with a private
repository login, no roll code repo)
gets a preview per branch, as a roll code repo's branches do. roll apps' push hook (the
repository's own webhook: GitHub, GitLab, Bitbucket) hands previews each push to a branch that
none of the service's environments follows, after checking the branch really is at that commit
(`apps_control::integrations::GitPushes`; rolld wires it to `Engine::git_push`). Then:

- In each project that stands for the service (on its canvas and `Workspace::trusts`: anyone may
  put any service on their own canvas, and a project that doesn't own it gets none of its
  branches, titles or commits), under its rules (previews on, the branch
  matching `branches`, no `[skip preview]` in the message, room under `max_previews`), the
  branch's preview is made, or moved to the pushed commit. A deletion deletes it. The same push
  twice changes nothing.
- The preview's repo is `git:<service id>` (the catalog says so for Git sources), so its
  services deploy at the pushed commit through roll apps' own `deploys {revision}`, with every
  data branch, mask, egress rule and screenshot as any preview has. Another Git URL service of
  the project builds its own (production's) commit.
- Only the repository's writers move its branches (roll apps checks the push), so these previews
  are trusted.
- Pull requests (roll apps' `GitPull`: GitHub's `pull_request`, GitLab's merge request events,
  every claim checked against the repository's refs, `refs/pull/<n>/head` for a fork's) name
  their preview by title (plain words, at most 200 characters) and link it (`pull`, `https://`
  only). A same-repo pull request is its branch's preview. A fork's is `pull/<n>`, untrusted
  (secrets resolve untrusted, as a roll code fork's do), and runs nothing until a writer approves
  that head (`POST …/approve {head}`, the sheet's "Approve and run"); the next push waits again,
  its services asleep meanwhile. One waiting holds a place under `max_previews` (anyone can open
  them). A close deletes the preview. A later push never makes a row trusted again. A fork's row
  is found by its number (a writer's branch named `pull/7` is another preview). Approving is a
  person's call (a token gets 403). Under Roll only the instance runs closed: roll apps holds
  the `pv-` environment's instance to it (apps' #143), on a gateway that doesn't reach the host's
  services (#240). The build gets none of its secret values (#222, `build_closed`), on a network
  of its own (#238). It can't reach the host's services, the metadata endpoint or any private
  range. Its only way out is a proxy to public hosts, so a fork's build can fetch its packages but
  never reaches roll or the node, and never has a secret to send. rolld's adapter deploys nothing
  to a closed environment that doesn't say `build_closed`, and each deploy says `closed: true`.
  A roll apps that predates either refuses it, so an older node can't build a fork's head
  open.
  A title loses control and invisible formatting characters.
  `a_forks_pull_request_on_a_git_url_service_runs_only_its_approved_head`; live in
  `scripts/preview-apps.sh` with GitHub's `pull_request` events through roll apps' hook (#115:
  `GitPushes::takes_pulls`).
- They say themselves where the code lives (Vercel's status and comment): a `roll/preview`
  commit status on the pushed commit (pending, ready, failed; a fork's that waits is pending,
  "waiting for a writer", unlinked), and for a pull request's preview the same comment a roll code
  change's gets, edited in place, saying "deleted" when it goes. Each is posted only when it
  changed, and what was posted (the comment's id too) is kept, so a restart posts nothing new.
  Through the `GitReport` port: rolld posts with roll apps' GitHub App connection (a short-lived
  token for that one repository; never the service's git login). A service on GitLab or
  Bitbucket gets the same through roll apps (`Apps::commit_status`, `Apps::pull_request_note`:
  `roll/preview` on the commit, and one note on the merge request edited in place, made again
  when it was deleted there), with the service's Git login, sent only to its source's own host
  through the client for tenant-chosen URLs. It's posted on the service's holder, which uses
  that login: a signed fleet hop (`fleet::REPORT`) when that's another node. A service with
  neither gets nothing. The project's `github.status` and `github.comment` switches hold as for a roll
  code change's. A comment's number and id belong to the repository its pull request's page
  names: once the service is connected to another repository, nothing is posted there (an
  independent review's finding). `a_git_url_preview_reports_its_status_and_pull_request_comment`.
- roll code's discovery leaves them alone (they follow pushes, not roll code's branches), and
  keeps them while the service is on the canvas.

`a_git_url_services_branch_push_makes_its_preview`; live in `scripts/preview-apps.sh`: a `git push` of a
branch to the service's repository (roll code's own git over HTTP) and its GitHub push webhook
made the branch's preview, serving the pushed commit, in 21 s (a Docker build); the next push moved
the same preview, and deleting the branch deleted it.

## Secrets

A preview's secrets are an environment of the project's secrets (`feature/roll-secrets`,
docs/secrets.md) called `pv-<preview id>` (an ephemeral environment, labelled `preview`), inheriting from `secrets.base` (`development` by
default) with the project's `env` overrides and the branched resources' URLs on top.

- **A link to a branched parent never hands out the parent's values.** A secrets environment
  inherits its base's links, so a link there to Postgres project P (`PGPASSWORD`, `PGHOST`…), a
  stream (`KAFKA_USERNAME`…), a bucket, Redis or a search cluster that the preview branched would
  still give the parent's (a fork's code included). The branch's values go by those names too,
  set after the secrets layer so they win (`names::with_standard`: `PG*` parsed from the branch's
  app URI, `PGOPTIONS` and `PGSSLMODE` from its query too, as roll apps' Postgres link sets them
  (roll Postgres picks the endpoint by `options=endpoint=…` on one node, so an app connecting
  with libpq's `PG*` names needs it; no `sslmode` there is libpq's own `prefer`, never a
  parent's and never empty, which libpq refuses), the stream's SASL login, the search branch's key and login), and a credential the
  branch has none for is blank (a NoSQL HTTP token, Kafka client properties). The roll secrets
  thread follows with the exact fix: a link to a branched parent isn't resolved at all and comes
  back as `followed`, its name filled from the branch even under a custom prefix.
  The base's links to a branched parent (its list: links, no values, nothing resolved) are
  also set from the branch as the environment's own keys before each ensure, so roll apps, which
  resolves a preview's environment itself, gets the branch's value under a renamed link too.
  `a_branchs_values_go_by_the_names_a_link_to_its_parent_would_use`.
- **Protected values stay out of preview code.** A preview resolves its secrets as `untrusted`
  (roll secrets' flag): nothing of a protected environment resolves, whether directly, by
  reference, by import or through the base. The preview gets its own overrides and links. Only
  a project with `secrets.allow_production` and a trusted change resolve without it.
- **Dynamic credentials live as long as what started with them, and only trusted code gets
  them.** A preview that resolves as untrusted (a fork's code, or a project without
  `secrets.allow_production`) asks for none (`dynamic: false`, roll secrets' own rule for
  untrusted callers, which it enforces too): a GitHub App installation token or a cloud session
  would outlive what the code was approved for. Those keys are left out and named in the
  activity. For a trusted preview, a dynamic link (a per-lease
  database login) is issued to each start of a service (consumer `preview:pv-<id>/<service>`)
  and each seed or release job (`…/seed`, `…/release`). previews keeps only the lease's id, never
  the credential (table `leases`). Each pass, the node that runs the preview renews a lease
  within 10 minutes of its end (`POST …/leases/{id}/renew`; each renewal is in roll secrets'
  audit log, so not more often). It revokes the lease once its service isn't running there
  (asleep, failed, removed, or moved to another node), and the next start revokes the start
  before's. So a sleeping preview holds no credentials. A lease that can't be renewed (revoked
  in roll secrets, or at its 30-day limit) puts its service to sleep, said in the activity, and
  the next request starts it with a new credential. A job's leases are revoked when the job
  ends. Deleting the preview's environment revokes whatever is left. On roll apps a service's
  secrets are roll apps' resolver's (`dynamic: false`), not previews'.
  `a_start_s_dynamic_credentials_live_as_long_as_it_runs`.
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
- **It expires by itself.** `ttl_minutes` is a day, renewed by each whole converge (every
  hour for a settled preview, at once for a deploy, a reset or a refresh), so a teardown that never happened
  leaves it for a day at most; roll secrets then deletes it and revokes its leases.
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
logged and read again on the next converge; it never fails the preview. Only what the change made is
named: each branch's own lines are kept as it's made (masked, before any seed or release runs;
`base_schema` on its resource row, read from the branch alone), and a line counts only when the
branch also differs in it from that base (`schema::diff_since`). Another change's migration that
lands on production while the preview is open isn't read as this one dropping its table, in the
lines or the risks. A branch made before bases were kept, or whose base couldn't be read (said in
its activity), is compared with its parent alone.

Other open changes of the same repo whose previews' schema changes touch the same tables are
named under them (another repo's never are: a public repo's comment never names a private repo's
branch or tables) (`schema_overlaps`, PlanetScale's conflicting deploy requests: a table added or dropped, a
column, a constraint or an index on it, `roll_previews::schema::tables_touched`; 5 changes and 5
tables each at most), on the sheet and in the pull request comment ("**Also changing these
tables:** `feat/b` (`public.users`). Whichever lands second migrates a table the first one
changed"), since the second to land migrates a table the first already changed.

What in those lines would hurt production when the change lands is named above them
(`schema_risks`, `roll_previews::schema::risks`; PlanetScale's deploy request warnings, Atlas's
migration lint): a table that goes ("production's rows go with it"), a column that goes, a
rename included ("the version running while it deploys may still read it": expand, then
contract), a column whose type changes ("Postgres may rewrite the table under an exclusive
lock") and one made `NOT NULL` ("scans the table under an exclusive lock", where a `CHECK … NOT
VALID` validated first doesn't); on a search branch, an index that goes ("its documents go with
it") and a field whose type changes ("a mapping can't change in place": a reindex). Added
columns, fields and indexes aren't risks here: the release ran
them on production's own rows, and its time is said beside it. The risks are read from every
line, not only the 200 shown (a migration of many removals still names its type changes), at most
20 with a count of the rest; a view's columns are `view column` lines, never a table's risk, and
`NOT NULL` is read before a column's default (whose text could say anything). The comment lists them unfolded
as "Migration risks", each in a code span its names can't close; the sheet and the change page
show them in red over the lines (the change page with the lines folded).

Search branches the same way: each one's `_mapping` and its parent's, read through roll search's
operator console request (no cluster password), become `index <name>` and `field <index>.<path>
<type>` lines (nested fields and multi-fields by path, system indices left out), so a change
whose code adds or retypes a field says so (live: `+ field products.price float` after the
preview wrote a document with a price). An index the parent made after the branch started (the
cluster's `parent_at`: a connector's first sync, a rollover, a day's `logs-…` index) that the
branch doesn't have is left out: production's new index, not one the change drops (live: a
connector's index the parent's sync makes after the preview is no risk). With several such
resources, each one's lines follow a `# <kind>:<ref>` heading.

## Egress

A preview runs a change's code against real data, and caught mail and removed provider keys
close only the commonest way it reaches real people. A webhook to a customer, or a call to a
payment provider's live API, is still one line of code away. A project chooses what its
previews' code may reach:

- `egress: "open"` (the default, as before): anything.
- `egress: "roll"`: only roll's own products (the preview's branches) and roll's proxy in front
  of it. Nothing on the internet.
- A fork's preview's pages carry a Content-Security-Policy (`runtime.rs` `fork_policy`): they
  may load from and send to only the preview's own hosts (`default-src 'self'` and its services'
  origins; inline scripts, `eval`, `data:` and `blob:` still allowed, so its pages run as they
  did; `form-action` the same). An approved fork's code could otherwise read what a reviewer's
  page shows (rows of the repo's data) and send it to another site from their browser, around
  `egress_forks`, which holds only its server. A policy of the page's own still applies (a
  browser keeps both); a navigation away isn't something a policy can stop. Previews' own proxy
  and roll apps' for a fork's `pv-` environments (a closed one: roll apps' #1032 adds the same policy, its origins the same `pv-` name's hosts across the project's services and their custom domains; live in `scripts/preview-apps.sh`, the fork's pages carry it and the same repo's pull request's don't). `runtime::tests::service_hosts_reach_their_containers`.
- `data_forks`: what a fork's preview's data is. It's `"schema"` by default: its branches hold the
  structure without the rows (Postgres' schema, NoSQL's tables, streams' topics, queues' and
  tasks' definitions), whatever the project's own previews get, and a kind that can't be
  structure-only (Redis, buckets, search) is left out, its variables blank. A fork's code is
  approved to run, not read for what it does with the rows it can see, and masks only hide the
  columns someone named. `"same"` gives it what the project's own previews get, masked the same.
  A fork's seed still fills its structure. Changing it reaches a fork's existing previews at their
  next converge, which makes their branches again. `a_forks_preview_gets_the_datas_structure_only`.
- `egress_forks`: what a fork's preview reaches when `egress` is `"open"`. It's `"roll"` by
  default, with `egress_hosts` still allowed through the proxy; `"open"` lets it reach what the
  project's own previews do. A fork's head runs only once a writer approves it, but an approval
  is to run its code, not a read of what it sends out. With open egress, an approved fork could
  post the values it resolved (staging's, since an untrusted preview never gets a protected
  environment's) anywhere. This mirrors roll secrets' split, where untrusted gets less. Asked
  for by the roll secrets thread (2026-11-17). It holds for its services and for its seed,
  release and teardown jobs: each runs on the preview's own network, with the listed hosts
  through its egress proxy (an adversarial review found the jobs ran on Docker's default bridge).
  On roll apps it holds for the instance (apps' #143; its gateway isolated from the host's
  services, #240). The build has none of its secret values (#222, `build_closed`). It runs on a
  network of its own whose only way out is a proxy to public hosts (#238), so it reaches neither
  roll nor the node. A roll apps that doesn't say `build_closed` gets nothing
  deployed under Roll only, trusted previews' included (a closed environment's build also loses
  sealed values such as a private registry's token).

Under `"roll"`, a preview's services resolve names through Docker's embedded DNS with
TEST-NET-1 (`192.0.2.1`, never routed) as its upstream. The network's own names (`roll-egress`,
the other services) resolve, and an outside name (`<secret>.attacker.example`) is forwarded
nowhere it could be read, whatever the Docker version. From version 26, Docker doesn't forward
from an internal network; before that, it did, from the host. The egress proxy checks a name
against `egress_hosts` before it resolves it.

**Decision (2026-11-16): enforce it on the node, with a Docker internal network per preview.**
A preview whose project chooses `"roll"` gets its own network made with `--internal`, which has
no route out. Its service containers join that network instead of the default bridge and
publish no port. roll's proxy reaches each one at its address on that network, and a container
reaches roll's products at that network's gateway (`host.docker.internal` points there, not at
`docker0`). The screenshot run already joins the service's network, so the page's own code
can't reach out either; it finds the app's port on the container's `sh.roll.code.port` label,
since nothing is published (screenshots failed under `"roll"` until 2026-11-16: the live check
now takes one there).

Why this way, rather than the alternatives:

- **Per-container firewall rules:** no. roll mustn't touch the host's firewall.
- **The cloud fleet's security groups:** no. They exist only on AWS, they apply to a whole node
  rather than one preview, and a single-node install would get nothing.
- **An HTTP proxy in `HTTP_PROXY`:** advisory only, since code can ignore it. It comes later
  for allowed hosts, but only behind the internal network, so it's the only way out.

It's measured on ai-server: a container on such a network gets "network unreachable" for an
outside address, and the host reaches its server at its address. Containers there can't reach
the host either way (the host's firewall), so the products' reach is the same as on the default
bridge. On the fleet, where products listen on every interface, the network's gateway reaches
them. It's reversible: `"open"` is the default, and the setting only adds a network. A service
hosted on roll apps gets the same rule: the preview's environment there is made with
`egress: {mode: "roll", hosts}`, so it runs on a network of its own with no route out and its
listed hosts go through roll apps' egress proxy (docs/apps/README.md, "Egress"); what that proxy
refuses shows in the preview's activity too.
Changing it restarts the services (it's part of what they're deployed with). Live in
`scripts/preview-cost.sh`: the service reached 1.1.1.1 with `open`, then ran on
`roll-previews-net-<id>`, was still served through roll, and got "network unreachable".

**Allowed hosts:** with `"roll"`, `egress_hosts` lists the hosts outside roll its code may still
reach (`api.stripe.com`, `*.sentry.io` for its subdomains; at most 20, names only). The
preview's services then get `HTTPS_PROXY`/`HTTP_PROXY` pointing at its egress proxy: a small
container (`roll-previews-egress-<id>`, the screenshot image's Node running `egress.js`) on the
preview's network as `roll-egress` and on Docker's default network. It passes on HTTPS tunnels
and plain HTTP to a listed host on port 443 or 80 only, and refuses anything else with a 403.
A listed name is the tenant's choice and may point anywhere, so the proxy resolves it itself and
connects only to a public address, the one it checked (roll-net's `private_ip`: loopback,
private, link-local and the cloud's metadata address, CGNAT, IPv4 inside IPv6), never to the
host, roll's own network or the cloud's metadata; a name that resolves only to private addresses
is refused and said as such (the benchmarking range 198.18.0.0/15, the 6to4 relays and Teredo too;
an adversarial review, 2026-11-17). Clearing the list removes the proxy at the services' next
start, so nothing listens on the network with an old list; the activity names at most 50 refused
hosts per preview and counts the rest (before 2026-11-16 it was connected to: `169.254.169.254.nip.io`
listed would have reached the metadata service).
Each pass, what it refused is said in the preview's activity, each host once. Code that ignores
the variables still can't get out, because the network has no route. A sidecar rather than a
listener on the host, because containers can't always reach the host (they can't on ai-server:
its firewall), while container to container always works. Changing the list restarts the
services, and the proxy goes with the preview's last service. Live: github.com reached through
it, example.com refused and said in the activity, and a listed `169.254.169.254.nip.io` refused
as a private address.

### Keys a preview never holds

A fork's preview runs code nobody in the project wrote, so roll secrets never gives it a
sensitive value. With roll secrets' host bindings (#520) it can still call an outside API with a
key: its environment gets an `rlph_` placeholder where the key would be, and only roll's egress
proxy swaps it for the value, only in a request to a host the key is bound to (Infisical's Agent
Vault, Akeyless's SecretlessAI). Neither Vercel's, Netlify's nor Render's fork previews can do
this: they get no secrets, or all of them.

- `egress_hosts` decides first. A key's bound hosts never open egress by themselves: a fork's
  preview reaches a bound host only when the project lists it too (agreed with roll secrets), and
  its activity names a key bound to a host its egress doesn't allow ("keys bound to hosts its
  egress doesn't allow (add them to egress_hosts, or calls there fail): STRIPE_KEY →
  api.stripe.com", once until it changes), so a call that fails says why.
- What it holds is on its record (`intercepted`: each key with the hosts it's bound to, and how
  many requests through its egress proxy had a key swapped in or were refused; never a value or
  a placeholder) and the sheet ("Keys it never holds: STRIPE_KEY → api.stripe.com · 12 swapped
  in, 1 refused"). Its services on roll apps are counted by roll apps' proxies (their `pv-`
  environments' `intercepted`, apps' #612: since each proxy came up, not kept across its
  restarts), which previews reads with the egress refusals and adds as `on_apps` (requests,
  swapped, the latest refusals' reasons, masked as its logs are): "on roll apps, 3 swapped in".
- For a host the preview's interception CA (#517) covers, `egress.js` hands the client's
  connection to rolld over a unix socket in the preview's own directory, mounted into the egress
  container (containers can't reach the host's TCP ports on ai-server, and no port is opened).
  Every other allowed host is tunnelled untouched, as now.
- rolld serves it with `roll_previews::intercept::serve`: it checks the CA is live (on every
  connection), terminates TLS with a certificate for exactly that host, runs each HTTP/1.1
  request's headers and query through roll secrets' filter (`proxy::filter` →
  `Secrets::substitute`, which checks the placeholder, the host, the place and its maker again),
  answers 403 to a request it refuses (nothing of it is sent), and re-dials the host over TLS
  verified against the system's roots, never the interception CA. The `Host` header is the host
  connected to, whatever the request said. No upgrades (WebSockets) through a bound host.
- The CA's certificate is trusted by the preview's services without touching their image's own
  store (not `update-ca-certificates`: a service runs as whatever user its image says): a copy of
  the system's bundle plus it, named by `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` and
  `CURL_CA_BUNDLE`, and `NODE_EXTRA_CA_CERTS` naming the CA alone; its key stays in
  rolld's memory. Tearing the preview down ends its consumer (`end_consumer`): its placeholders and
  CAs stop at once.
- The preview resolves as the rule's writer (`Ask { maker }`, from `env_refs`), so a placeholder
  works only while that person still may reveal the key.
- Services on roll apps: roll apps resolves the `pv-` environment's secrets itself, and its
  closed environments swap placeholders with the same step (`roll-intercept`, consumer
  `apps:<project>/pv-…`, apps' #603). Previews names the maker there too: the environment's PUT
  carries `closed_by` (the rules' writer, or `null` for none, when keys bound to hosts are
  withheld), which only the operator may send, so a fork's preview on roll apps uses a key only
  while the person who wrote the rules may reveal it, not whoever owns the project. A roll apps
  without the field refuses it as unknown and the environment is made without it (its maker is
  who set the egress, as before). A change of writer deploys it again.
- A host that echoes a key back (an echo service, a debug endpoint) never hands it to the
  preview: a request a key was swapped into goes with `Accept-Encoding: identity`, and its response
  goes through roll secrets' scrub (`proxy::Scrub`, the values in any form it looks for, never
  leaving roll secrets) before it reaches the preview. In a header: a 502. In the body: the
  connection is cut before those bytes. An encoded body (which can't be checked): a 502. An echo
  ends the placeholder (`Secrets::echoed`: an alert audited, the environment's notice queued) and
  says so in the activity ("echo.localhost echoed STRIPE_KEY back in its response: stopped, and its
  placeholder ended"). After that the placeholder is refused (a 403) while the CA stands; ending
  it shrinks the key's bound hosts, which ends the CA (roll secrets' #705), and with no other live
  placeholder there's nothing left to intercept, so connections are then cut. Either way it never
  reaches the key. `roll-intercept`'s port: `Interception::filter` answers a `Filtered` with a
  `ResponseScrub`, so roll apps' closed environments get the same. Swapped requests and refusals
  are counted in the preview's activity, never their values or placeholders.

The intercepting step is its own crate, `crates/roll-intercept` (product-neutral: roll apps'
closed environments use it too), with roll secrets as its `Interception` in rolld's
`src/intercept.rs` (`CaInterception`). Built so far: `intercept::serve` and its port (`intercept::Interception`), tested with a fake CA,
a fake filter and a TLS host of its own
(`intercept::tests::a_placeholder_becomes_its_key_only_on_the_way_to_its_host`: swapped only on the
way out, a refused placeholder never reaches the host, an uncovered host or an ended CA is never
terminated, an upstream the real roots don't vouch for gets nothing); `egress.js`'s handoff
(`INTERCEPT`: a CONNECT to a covered host, once allowed, goes to `/roll/intercept.sock` as
`host:port` then the client's bytes; plain HTTP to one is refused, a key goes over TLS only;
unset, nothing changes) and rolld's side of the socket (`intercept::listen`, `read_handoff`,
bounded and refusing anything but a name and a port; `public_dial`, public addresses only).
The runtime serves it too (`ports::Intercepts`, set by rolld once it can answer; unset, and with
no `intercept_dir`, nothing changes): a preview with an interception gets
`<intercept_dir>/<preview>/` (0700, its CA's `ca.pem` and `intercept.sock`), the listener on it,
and its egress container running as the directory's owner with it mounted at `/roll` and
`INTERCEPT` set; each connection's swaps and refusals go in the activity ("egress to
api.stripe.com: 3 request(s), 3 key(s) swapped in"), never a value. A dead listener's socket is
made again in place (a running container keeps its mount); a change of hosts makes the egress
container again; dropping the proxy ends the listener and the directory. The test's first
connection goes through a real socket. A service of an intercepted preview trusts its CA: the
CA's PEM comes as `ROLL_INTERCEPT_CA`, and before its command runs it's written to a file and
added to a copy of the system's bundle (Debian's, Red Hat's or Alpine's path, whichever is
there), which `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` and `CURL_CA_BUNDLE` name, with
`NODE_EXTRA_CA_CERTS` naming the CA alone: every other host verifies as before (checked in
Alpine and Debian images). A runtime with a trust store of its own (Java's keystore) doesn't read
these. After an independent review: each preview's interception is bounded (16 connections at
once, a 10 s handshake, 30 s for a request's headers, each host's leaf made once for the CA's
life, `live` still asked every connection), a connection the host closed is made again (verified
as the first), the socket is made under the preview's egress turn (two starts make one), only
the socket's own directory is mounted, read-only (the CA's file stays out), and the activity
names only connections that swapped or refused something; hop-by-hop headers stay with their hop both ways (the host closing its connection doesn't close the client's). HTTP/1.1 only: a client offering
nothing but h2 can't connect. Next: rolld's adapter once roll secrets' batch 6 lands, then the
live check.

Wired to roll secrets (batch 6: #520 placeholders, #517 the CA): a preview whose egress goes
through its proxy ("Roll only", a fork's default) resolves with placeholders for its consumer
(`preview:pv-…`), made by whoever last saved the project's preview rules (recorded with them; the
operator is no one, so rules it saved make none). When that person may no longer reveal there,
it resolves as without placeholders and says "placeholders off: …; a writer saving the rules
again turns them back on" in its activity, never with anyone else's rights. rolld's
`SecretsIntercepts` asks `intercept_ca` for the consumer (reused while `intercept_ca_live` says
so and it's more than 3 h from its end: a new CA restarts the preview's services, and roll secrets
says a CA is no longer live once its consumer's bound hosts gain one it doesn't name; none while
the preview holds no placeholder), `intercept_ca_stands` on every connection (a stale CA keeps
serving the hosts it names until it's swapped), and roll secrets' `proxy::filter` for each request; the
preview's teardown calls `end_consumer`, ending its placeholders and CAs at once. Each preview's
socket lives under `<home>/previews/intercept`. `a_roll_only_preview_takes_placeholders_made_by_who_saved_its_rules`.
Only roll secrets' `maker_unreachable` refusal falls back without placeholders; any other 403 is
what it says. A CA that ended (a key's bound hosts shrank: a revoked placeholder, a cleared
binding, a cut-off) refuses the connection with "a key's bound hosts changed; restarting with a
new interception certificate" and kicks a converge, which mints the new CA and restarts the
preview's services (the CA's id is in their deploy fingerprint), since they trust its certificate.
Live: `scripts/preview-intercept.sh` (rolld with `ROLL_PREVIEWS_INTERCEPT_ALLOW_PRIVATE`
trusting a CA of its own, a TLS echo host `echo.localhost`): a key bound to it, a token saving the
rules (the maker), a "Roll only" preview. Its service holds `rlph_…`, never the key; a request
through the preview's socket, as its egress proxy hands one over, reaches the host with the key
swapped in; a placeholder that isn't live is refused (403); the activity says a key was swapped
in and never shows it. (The egress proxy only lets ports 443 and 80 through, and 443 isn't the
script's on a shared machine, so it goes to the socket directly: what `egress.js` would send.)
Until roll secrets' #582 gives the maker refusal its own code, a maker who can't reveal there
fails the start with the reason rather than falling back.

### Credentials at rest

What previews keeps that's a credential is sealed with the deployment key (`ROLL_SECRET_KEY`,
AES-256-GCM, as roll's reviewer keys are), each bound to its row: a preview's own variables (to
the preview), the rules' pasted `env`/`env_for` values (to the project, scope and name; a
`roll://` reference isn't a secret and stays as it is), the screenshot links' signing key, and
what a branch's product handed out once (`kept`: a stream's SASL secret, a queue user's password,
a bucket key's secret, a search branch's keys; to the preview, the resource and the earlier push
or restore point it's a copy for; roll search's review of #1054 found it in the clear). They
open inside the store as they're read. With no key, a pasted value is refused (400, naming
`ROLL_SECRET_KEY`), never written in the clear; the signing key is then this process's own, kept
in memory only (its links stop at a restart: local development); a branch's `kept` is written as
it is, since its services need it (local development too). A database from before is sealed
the moment the key comes (`Store::set_sealer`), in one write with SQLite's `secure_delete` on and
the WAL checkpointed and truncated after it, so nothing of the old values stays readable in the
file, its free pages or its `-wal` (`credentials_are_sealed_at_rest_and_nothing_stays_in_the_file`,
on an upgraded database). On Postgres (a fleet's control database) the rewrite is an ordinary
write: the old row versions stay as dead tuples until autovacuum reuses their pages (`VACUUM FULL`
on `settings`, `previews`, `keys`, `resources`, `pinned_data` and `restore_points` removes them at
once). Either way, backups and WAL archives
taken before the key came still hold the clear values: rotate or expire them. Roll secrets'
thirty-fourth review found them in the clear.

## Safety

- **Secret values in what a preview serves** (Netlify's secrets scan). A preview's pages are
  read by whoever opens it (a public one by anyone), and its code can put what it was given in
  them: a server-rendered page, a bundle built with a server variable, an API answer. The first
  page's screenshot run keeps what its pages served (documents, the origin's own scripts, fetch
  and XHR answers, each path once and never its query; 40 bodies, 2 MiB each, 8 MiB in all) and
  hands it to roll, which looks in it for the preview's secret values: roll secrets' values (a
  `plain` one's URL password only), its branches' credentials, and its own variables named like
  a credential. A value meant for the browser (`NEXT_PUBLIC_`, `VITE_`, `PUBLIC_`, `REACT_APP_`,
  `NUXT_PUBLIC_`, `EXPO_PUBLIC_`, `GATSBY_`, `VUE_APP_`, `STORYBOOK_`) isn't one, nor a user name
  (`PGUSER`, `SMTP_USERNAME`), nor one under 8 characters. The run is never given the values (the page's code shares its container), and
  what was served is dropped once looked in. Each one found is named by the name the preview gets
  it as, with where (`served_secrets: [{key, path}]`, for the run's head): on the record, first
  on the sheet's verdict strip ("1 secret value in what it serves") and in a box under it, in
  `roll preview show`, in the activity when it changes, and in the pull request comment
  ("**Secret values in what it serves:** `STRIPE_KEY` in `/assets/app.js`"), never the value
  (checked in the screenshot image: a page's document, its script and its fetch answer kept by
  path, the query left out; live: `scripts/preview-cost.sh`'s service serves its environment at
  `/env.txt`, and the record names `DATABASE_URL` there without the password).
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
    - **Safe by default** (Tonic flags new sensitive columns, Neosync needs a rule for each): where
      a parent has rules, every column of it that looks like people's data (the suggestions'
      reading, `mask::suggest`: emails, names, phones, addresses, dates of birth, IPs, national
      ids, bank and card numbers, secrets) and has none is masked too, with the rule suggested for
      it, so a column production gains never reaches a preview in the clear. The preview's
      activity names them, and `masked_by_default` (`[{resource, column, why}]`) has them for the
      sheet. The project's repos are told of each such column once, the first time a preview masks
      it (production gained it, or the project's first rules), as their `masking` event
      (webhooks, the event stream, automations; docs/code/http-api.md), so a person who can write
      its rule hears of it: how many each resource has, never their names (a repo's readers may
      not read the resource's schema), with the dashboard's link, which names them. `keep` is a rule that leaves a column as it is (the project's word that it isn't
      people's data); it's never sent as a mask. A parent with no rules hands over its data as its
      `data` says (`real`). NoSQL and search parents the same way: a NoSQL table's columns
      outside its keys and its views' keys (`mask_default()`, a secret's `mask_null()`), and a
      search index's fields (`email` or `redact` into a text-like field, `remove` otherwise). A
      NoSQL column masked by default is a mask like any other, so a query that compares it is
      refused: `keep` it if the app filters on it. The columns are read on the branch (or masked
      base) just made from the parent, so a production database that sleeps isn't woken for it.
      Columns that can't be read (or not within 10 s) fail it, as Neosync's and Tonic's do:
      what's masked by default isn't known, so the preview fails with why and tries again rather
      than hand anything over, and a masked base isn't made. Masked bases are masked the same way
      when they're made (`engine.rs::columns_that_look_like_peoples_data_are_masked_by_default`).
    - **Masked once, branched many** (Neon's anonymized branches): a masked Postgres branch comes
      from its parent's masked base, a branch of the parent masked once by the rules (the rewrite
      and `VACUUM (FULL)` above, once; with no role of its own, which would take the tables, so
      each branch's role gets them as from the parent), which each preview's branch then branches copy-on-write,
      masked as it is. One masked copy per parent and rules, not one per preview and reset, and
      the data step skips the rewrite. The base serves new branches for a day
      (`ROLL_PREVIEWS_MASK_BASE_HOURS`, 24, a week at most; 0 masks each branch's own copy),
      then the next one is made from the parent as it is then, so a preview's data is at most
      that old. New rules make a new base. One is made by one converge at a time (a claim, its
      branch found again by name after a crash); a branch that finds one being made waits for it
      (2 minutes at most), and one that can't have it masks its own copy as before; a base that
      couldn't be made lets go of its claim at once, so the next converge takes it over under
      the same name. An old base (other rules, past its day, or one
      no project's rules want) is deleted once no branch comes from it (after a branch's
      deletion, and every 10 minutes on the leader); the base for the rules now stays a day for
      the next preview. Within one base the same input masks to the same output across its
      previews. Data as of a time (`data_at`) and structure-only branches don't use it. A
      branch's schema changes are still read against the parent, past its base
      (live: the preview's activity says
      it branched from it, still masked, and no older base outlives its branches).
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
    of what it inherits. The policy holds against the preview's own code (its credentials are
    the branch's superuser) as the Postgres rewrite does: filters and indexes compare the masked
    values, a table or view whose primary key has a masked column can't be read, and a masked
    column can't be in an `IF` (`nosql-engine`'s
    `platform_policy_tells_no_role_a_masked_value`; live: the branch reads `****`, and a filter
    on the real address finds nothing there while it finds the row on the parent). Mask a
    table's other columns, not its keys: the settings refuse a new mask on a column of a
    primary key in the parent's schema (a table's, or a view's of that table), with a 400 that
    names it (`mask::nosql_key_masks`; live: 400). A column the schema doesn't have yet is
    allowed, since a migration may add it.
  - Search: `"search:<cluster>"` → `{"index.field": rule}` (a nested field's path too:
    `users.address.city`), the rule `redact` (`[masked]`), `remove` or `email`
    (`user-<document id>@example.invalid`). The branch's documents are rewritten with
    `_update_by_query` (a Painless script) before any service is pointed at it; the parent's
    segments are shared, so the rewrites are the branch's own and the parent keeps its values.
    The old versions stay in those shared segments as deleted documents, whose terms a term
    dictionary still answers with (`_terms_enum`, suggesters, document frequencies), so every
    segment that holds one is then expunged as the branch's own (`expunge_deletes_allowed` 0,
    `_forcemerge?only_expunge_deletes`), as Postgres masking's `VACUUM FULL` (live: the branch
    reads `[masked]` and its `_terms_enum` finds nothing, the parent reads `parent`; roll
    search's `documents` (control.rs) pins the sequence).
    A masked `semantic_text` field: a branch doesn't get its parent's inference endpoint's key
    (roll search's #988), so the masking's writes send `X-Roll-Unembedded: drop`. The masked text
    stays unembedded, and the real values' chunks and embeddings go, with no provider called. An
    app's own writes to such a field on a preview's branch need the endpoint's key set on the
    branch (`_inference/{task}/{id}/_update`, or the Inference tab's "A roll secret"); without it
    they're a 400 ("this branch doesn't have the parent's provider key; set it on the branch").
    Endpoints that need no key (E5 on the node, the preconfigured ones) are unaffected.
  - Other products: a `seed` anonymises the rows instead.
  - **Suggestions** (as Tonic's and Neon's anonymisation tools name columns that look personal):
    `GET /v1/previews/projects/{project}/mask-suggestions` reads the columns of every Postgres,
    NoSQL and search resource on the project's canvas that the project stands for
    (`Workspace::trusts`), whatever its rule (the rules dialog shows them under those on
    `branch`, as the rule being edited says, so switching one to `branch` shows them at once),
    and names those that look like people's data and have no mask yet, each with a rule
    (`mask::suggest`). A scoped token sees only the resources its grant lets it read
    (`Grant::allows`, `read` on `postgres:<ref>` and the rest): column names are the product's
    metadata. The promise: **a suggested rule never fails the masking**, since a failed mask fails
    the whole preview and "Add all" applies every one.
    - What's read, and what isn't. At most 2,000 columns per resource (`SUGGEST_COLUMNS_MAX`),
      each resource for at most 10 s (`SUGGEST_READ_FOR`). A Postgres parent whose compute
      sleeps isn't woken for it (reading would start it and reset its idle timer), and the rules
      dialog reads them only when asked ("Look for personal data that isn't masked"), so opening
      it never resets a running parent's idle timer either; a sleeping one is named in
      `unread` ("asleep: open it to read its columns"), as is one that wasn't read in time,
      couldn't be read, or has more columns than are read. The dialog shows that under the
      resource.
    - Postgres reads the parent's `main` as the superuser with only the catalog's names in reach
      and `statement_timeout` 10 s (its tables' columns, a partitioned table's once). Generated
      columns and `GENERATED ALWAYS` identities can't be written, so they're left out. For each
      column it reads what holds it, across the table's partitions:
      - nothing is suggested for a column that a foreign key of another table points at, that a
        CHECK or an exclusion constraint names, that a unique index's expression or predicate
        uses (`UNIQUE (lower(email))`), that is a partition key, or whose domain has constraints;
      - only `null` (on a nullable column) for a foreign key's own column, an array, a range, an
        enum or a composite;
      - a unique index's column is unique (`hash`, not one value for every row), and one
        `NULLS NOT DISTINCT` can't take a null either.

      NoSQL reads the parent's `/schema`, leaving out key columns (a table's or a view's of it),
      which can't be masked. Search reads the parent's indices' mappings one index at a time,
      leaves only (a multi-field is masked with its field), and a data stream's backing indices
      (`.ds-…`, which a mask can name) but not system indices.
    - Names are split into words (`snake_case`, `camelCase`, `IPAddress`, dots, and a strong word
      glued to others in one lowercase word, as Postgres folds `EmailAddress`: `emailaddress`,
      `homephone`, `useremail`) and matched case-insensitively: an email (`email`); a name,
      phone, postal address, date of birth (`born_on` too), IP address, bank account or card
      number (`redact`, or `hash` where a unique index covers the column, since one value for
      every row would fail it); a national id, tax id or passport number (`hash`: unique, and
      joined on; a `*_id` only when it holds text, since a number is another row's reference); a
      secret, token or API key (`null`, or `redact` on a `NOT NULL` column).
    - A rule the column's type can take: `redact`, `hash` and `email` write text (3, 32 and 33
      characters), so in a `varchar(n)` or `char(n)` the first that fits (`email`, then `redact`
      or `hash`), else `null` on a nullable column, else nothing; a Postgres column of another
      type gets `null`, or, `NOT NULL`, a `fixed:` value of its type (`1970-01-01`, `0.0.0.0`,
      `0`), or nothing. Types are matched by their whole name. A search field that isn't text-like
      is `remove`d (`redact` and `email` write strings), and a search secret is `remove`d; NoSQL
      gets `mask_default()` (`mask_null()` for a secret), which any type takes (`mask_hash`
      changes the type a client reads). Booleans are never suggested.
    - Left out on purpose: usernames, logins and handles, and passwords and their hashes,
      salts and digests (a preview's test accounts sign in with them: masking them locks those
      accounts out; a password reset token is a token, and suggested), the names of things
      (`filename`, `hostname`, `company_name`), a bare `name` on a table that isn't about people
      (`products.name`; `users.name` and `customers.name` are suggested), machines' addresses
      (`mac_address`, `wallet_address`), a mail server, another row's id number
      (`order_id_number`), times (`*_at`, `*_on`), references (`*_id`, but a national or tax id),
      counts, kinds and flags (`is_*`, `has_*`, `email_verified`).
- **Data as of a point in time** (Neon's time-travel branches): `data_at` (`-24h`, `-30m`, `-7d`
  or an RFC 3339 time, 30 days back at most) makes each branching and reset start from the
  parent as it was then, where the product keeps history: Postgres (pg-control's point in time;
  a reset branches again from there, since an in-place reset can't go back), NoSQL (`at`, in
  place) and Redis (`at`; a reset is a new branch from then, as Redis's resets are). Structure-only branches need no history and ignore it; other products branch as of
  now and the preview's activity says so (`Brancher::branch_at`/`reset_at`). Not checked live: a
  Postgres branch as of a time seconds old stayed in recovery past two minutes even once the
  parent's history reached past it (pg-control's `RECOVERY_TIMEOUT` is 30 minutes), so the
  live script leaves it out until pg-control's recovery is looked at.
- **Previews never email real people.** A preview's data is production's people, so its mail is
  caught (`mail: "catch"`, the default; review apps' MailHog): rolld runs a small SMTP catcher
  (`ROLL_PREVIEWS_SMTP`, `0.0.0.0:2525`; `roll_previews::mail`) that accepts mail only from a
  preview's own sign-in (`SMTP_USER` its id, `SMTP_PASSWORD` a keyed hash only roll makes, so
  nothing is stored and no preview can send as another), keeps the last 50 per preview and
  at most 10 MiB of them (the oldest go first: a preview's code can't fill the store), and
  relays nothing. Every service's `SMTP_*`, `MAIL_*`, `EMAIL_*`, `MAILER_DSN` and `MAIL_URL` are
  roll's whatever the project or its secrets say (they're production's), and mail providers'
  API keys (`SENDGRID_API_KEY`, `POSTMARK_SERVER_TOKEN`, `RESEND_API_KEY`, …) are removed. With
  no catcher listening (its port taken) they point nowhere: mail fails rather than reaching
  anyone. The sheet and `roll preview mail` list what was caught (`GET …/mail`; each opened as
  text, `…/mail/{id}` or `--read <id>`, never rendered). `mail: "deliver"` hands the project's own settings through. A deploy on roll
  apps gets the same overrides in its variables (and the `pv-` secrets environment's), with each
  provider key set to `roll-previews-mail-caught` over what roll secrets gives, since previews
  can't remove those there (`engine.rs::a_preview_s_mail_is_caught_never_sent`;
  `scripts/preview-apps.sh` checks the environment's variables on the stack). An app that
  sends through a provider's API under another variable's name isn't covered; egress rules are
  their own item. (Live: the key gone from the service, a mail sent as the preview caught and
  listed, a wrong password refused.)
  - Each branch says what it hands the preview's code (`resources[].data`): `none` (structure
    only, or queues and tasks, which never copy messages), `masked` (Postgres, NoSQL or search
    with masks set) or `real` (the parent's data as it is). The sheet and the change page mark a
    `real` branch "production data, unmasked", and the settings say on each such resource how
    not to (live: bucket, Redis and stream `real`, the masked three `masked`, queue and tasks
    `none`).
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
  - **Signed in to roll** (`access: "team"`, the rules' "Signed in to roll"; Vercel
    Authentication, Netlify's team access): only people signed in to roll who may open the
    project open its previews, as roll apps' sign-in-gated environments do (`visit.rs`, the same
    design and names; docs/apps/README.md, "Signing in to an environment"):
    - *Sent to sign in.* A page load with no session of the preview's own goes to
      `/dashboard/previews/visit?preview=<id>&to=<the page>` on roll's dashboard; anything else
      is a 401. The preview's host never sees roll's session cookie (another site), so the
      dashboard asks `POST /v1/previews/{id}/visit {to}` as the person for a one-use ticket (a
      minute) to the page they wanted: only for a URL at one of the preview's own service
      hosts, and only for a person's session, never a token.
    - *A session of its own.* The ticket goes back to the preview's host (`/__roll/visit`),
      which trades it for a host-only `HttpOnly`, `SameSite=Lax` cookie, `roll_visit_<preview>`,
      for 12 hours, and redirects to a path on the same host (no `//`, `/\` or control
      characters). The ticket's URL is the service's own (its scheme, host and port, as roll
      runs it); only the path and query come from `to`, so a crafted `to` can't send the ticket
      over plain HTTP or to another port. A ticket only opens a session on its own preview. The
      service never sees the cookie; when a browser sends several by that name (a sibling
      preview's host can set one on the parent domain), any live one lets them in. Tickets and
      sessions are kept by their SHA-256, and only while the project has team access: the write
      that keeps one reads the project's access, so a change of access can't miss it.
    - *Who.* rolld's `AccountsVisitors` (`crates/rolld/src/previews/mod.rs`) is the one place
      that says for services roll runs itself: an account that still signs in and is an admin, or
      whose role in the project now may reveal a preview there (docs/members.md: a developer
      whose environments take in `preview`, an admin or owner; never a viewer, who sees no values:
      the running app is what a share link gives, and minting one is `reveal`), through the same
      roll session that asked for the ticket. A ticket is only given to them, and each session
      asks again every 5 minutes (`visitor_recheck`, off the async workers): a roll session signed
      out, revoked or expired, an account disabled or deleted, or a member removed or made a
      viewer, is turned away at its next request after that, and its
      session is gone. Changing the project's access ends every session.
    - *The link's token lets nobody in.* The links roll hands out (`share_url`) are the plain
      URLs, as the change's and the pull request's are. A URL that still carries
      `?roll_access=` is redirected to the same URL without it, and the `roll_access_<preview>`
      cookie never reaches a service under any access. Leaving `link` gives each of the
      project's previews a new token, so going back to `link` doesn't revive the old links.
      Share links and the bypass header for CI still work.
    - *Feedback.* A session holder gets the feedback button, as a link visitor does under `link`
      ("Feedback on the preview page"): the gate marks a request it let in through roll (the
      link's cookie, or a live session; `ThroughRoll` in `runtime.rs`), and only such a request
      gets the script, the page's notes and the tag. Its note opens roll's dashboard, where the
      person is signed in and posts it as themselves (`POST /v1/code/changes/{id}/reviews`, a
      comment review roll code names by the person's handle, as every person's review). The page never holds
      a credential, so the app's own code can't post as them. A share link's visitor and a
      bypass request get no button (nobody to post as).
    - *Who opened it.* Previews run on branches of production data, so a team sees who looked
      (Vercel's deployment protection logs access too). When a ticket is traded for a session,
      the same write adds "`<handle>` opened it (signed in to roll)" to the preview's activity
      (the person's roll handle, as roll code names people): at most once a day per person (a
      new device or an ended session isn't news, and the activity keeps its last 200 lines),
      never per request. Admins who open a person's preview are listed too, as anyone who may
      open it is. It also keeps a `visits` row per person (`first_at`, `last_at`); `last_at` moves
      at each 5-minute recheck, not at each request. `GET /v1/previews/{id}/visitors` lists
      the last 8, latest first, as `[{who, last_at}]` (read access, like the activity), and
      the sheet's Activity tab shows them as "Who opened it". A share link isn't a person
      signed in: following one (`?roll_share=`) adds "a share link opened it" to the activity,
      at most once per link per day (`shares.opened_day`), under any access. The CI bypass
      header records nothing. Services on roll apps sign people in through roll apps, so they
      don't show here.
    - *Services on roll apps.* They get `access: {signed_in: true, bypass_sha256}` on their `pv-`
      environment, so roll apps' own sign-in flow and its own check decide (its
      `AccountsVisitors` and `/dashboard/apps/visit`: admins only today), not previews'. roll
      apps refuses `signed_in` on a node without roll's dashboard address (`web_url`), which
      fails the deploy. Share links open them as previews' own (see Share links). roll
      apps adds the feedback button only for a visitor with its access cookie (`via_link` in
      `apps-control/src/public.rs`), never for one with its sign-in session, so a team
      preview's services on roll apps have no button yet.
    - `visit.rs::a_signed_in_person_gets_into_a_team_preview` follows a page load to the
      dashboard, a ticket only for a person, only to the preview's hosts and only on its own
      preview, its URL at the service's scheme and port whatever `to` said, the ticket used
      once, the session the app never sees (nor junk cookies by its name shutting it out), the
      link's token taken off the URL, signing out of roll and the person losing the project each
      ending it, and a new access ending it and replacing the link's token; the session holder
      getting the feedback button's script (its notes to the preview in roll's dashboard) and
      the page's notes by who left them, and nobody without a session, nor a share link or the
      bypass, getting either. It also checks that the activity names the person once per
      session and the visitors list names them, and that a share link followed twice is one line.
  - Never indexed: every response through a preview's host, the service's own and roll's 401
    alike, carries `X-Robots-Tag: noindex` (as Vercel's previews do), whatever the service
    sends, so a public preview, a share link or an earlier push's URL isn't what a search
    lands on (live: both). roll apps does the same for its preview environments and a deploy's
    own URL (#26, 3b7cd1ac5; `scripts/preview-apps.sh` checks it on the stack).
  - A scoped token without reveal on the preview gets `share_url` with `?roll_access=redacted`
    (roll MCP's catalogue, #24): CI and agents that open previews need reveal, as
    `previews_share` does; `roll preview open` under such a token lands on the 401 page and its
    way in through roll.
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

  - **Webhooks** (Vercel's bypass in a query parameter, for what takes a URL and no header:
    Stripe's, GitHub's, Slack's): `POST …/webhooks {path, label}` (`roll preview webhook <id>
    /api/stripe --label Stripe`, the sheet's Webhooks) makes a URL of the preview's own, the
    first service's at `path` with `?roll_hook=<token>`, that lets requests to exactly that path
    of that service in (not another service's host, nor an earlier push's), whatever their
    method, with no cookie or redirect; the service gets the request without
    the token (the rest of the query kept). It lasts as long as the preview, until revoked; only
    its SHA-256 is kept, checked at every request, so a revoked one stops at once, and like a
    share link it acts at its maker's rights now (one who may no longer open the project's
    previews ends it). Its token is never a share link's (a `roll_share` with it opens
    nothing), the project's bypass never comes into it, and how many requests it let in and when
    last are kept (`hits`, `last_at`). Only a private, trusted preview takes one: a fork's code
    would get what's sent to it. A service on roll apps gets them on its `pv-` environment's
    access (`webhooks: [{path, token_sha256}]`, never the token; roll apps' gate checks them the
    same way, roll apps #1198), sent again at the next converge when one is made or revoked
    (`engine.rs::a_webhook_url_reaches_roll_apps_gate`, (`runtime.rs::service_hosts_reach_their_containers`).
    Services on roll apps get the secret's SHA-256 with the link's token (`access: {token,
    bypass_sha256}`, or `{signed_in: true, bypass_sha256}` under team access); their proxy checks
    it the same way. Rotating or removing the secret makes
    every preview of the project converge again, so they get the new one.
  - Services on roll apps get the same token (`PUT …/environments/pv-… {access: {token}}`), and
    roll apps' waking proxy checks it the same way: the token once, then a host-only cookie,
    else 401 without waking anything.
  - roll apps' proxy asks roll previews, through its hooks, for a `pv-` environment's notes
    (`…/feedback.json`), share links, guests' notes and team snapshots, naming its own service.
    roll previews answers only for a preview it knows runs on that service (rolld's
    `services::preview_on`, the lookup a deploy makes): an environment's name alone proves
    nothing (roll apps lets only roll previews make `pv-` environments now, #1205; one a tenant
    named after another's preview before that gets no notes, no share link and no snapshot).
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
  revoked or ended link stops at once. A share visitor never gets the project's bypass. A link
  says who it's for (`label`, optional: "Acme's review"), and with `notes` its visitors get the
  feedback button as guests (Netlify's drawer and Vercel's comments for people outside the
  team): the note form asks for their name (kept in their browser) and posts from the page, to
  `POST /__roll/notes` on the preview's own host (a JSON body from its own origin, 64 KiB at
  most, the link's cookie the credential), where the note is made on the change as
  `guest:<name>` (no handle has a colon, so a guest is never mistaken for a team member), in
  the dashboard's shape with "a guest, through the share link "Acme's review"" in its line. A
  guest's words are made inert for a pull request: an `@` can't mention anyone, and no image
  or HTML renders. They're words from outside the team wherever they're read: the review is
  made with an outside origin (roll code's review tools mark it), and `GET …/notes` marks a
  guest's notes and replies `outside`, so MCP's lockdown never hands them to a model. At most 10 an hour through one link (429 after). A guest sees only the
  notes left through their link, with no Reply or Resolve (the team answers in roll, on the
  change, or in Slack); a link without `notes` gives no button, as before.
  `a_share_links_guest_leaves_notes_as_a_guest` and the runtime's link test (the guest's
  script; a note from another site's page refused).
  Where roll apps serves the preview, its proxy takes the same links on the `pv-` environment's
  host through a port of roll apps' (`integrations::PreviewShares`, rolld's
  `previews::services::Shares`): `?roll_share=` sets the same cookie, and every request asks
  previews again (`Engine::share_visit`, which previews' own proxy uses too: live, and its maker
  still reaching the project), so a revoked link stops at once there as well. The cookie never
  reaches the app, and a guest is no visitor session of roll apps'. Guests' notes and their list
  go through the same port. `apps-control/tests/api.rs::a_previews_share_link_lets_its_guest_in_until_it_ends`.

## Cost

A preview's cost is the sum of what its branches and services use, priced with each product's
own rates (the same ones its usage page shows).

- **Sleeping**, per month: the bytes each branch owns, at its product's storage rate. Compute
  costs nothing. Nor does it cost roll's control plane much: usage is read every 5 minutes
  (`usage_every`), but a settled preview, asleep or running, goes through a whole converge (roll secrets'
  ensure, its base's links followed, every branch checked) only every hour (`settled_every`;
  well inside its environment's 24 h lease), its usage read alone in between, as Render and
  Vercel meter idle previews without waking them. Over its size cap on such a read, it converges
  to sleep there. A link added to the base reaches a sleeping preview at that hourly converge
  (rules' changes reach every preview at once). `a_settled_previews_usage_is_read_without_a_whole_converge`.
- **Active**, per hour: the services' containers (memory GiB-hours), Postgres compute units
  while connected, and Redis memory while loaded.
- **Runs**: its browsers' and jobs' containers by their GiB-hours at the same rate: screenshot
  runs (each container's own time, side by side: Chromium's, the other browsers', the stories'; a
  run that doesn't finish counts its whole time), another service's run, agents' runs (`try`),
  and seed, release and teardown jobs at their `memory_mb` (`run_usage`, this month's). They're
  in `month_to_date_usd`, so `max_usd_month` caps them too, and said apart as `runs_usd`.
  `a_previews_browser_runs_are_in_its_cost`.
- **roll apps services**: what roll apps billed their `pv-` environments this month (their
  builds, compute and egress at roll apps' prices, from its `…/usage` since the 1st, UTC), read
  with the branches' usage (`service_spend`). roll's runtime never runs them, so they have no
  running time here; until 2026-11-25 their whole bill was missing from `month_to_date_usd`, and
  `max_usd_month` couldn't hold a project whose previews run on roll apps.
  `a_previews_roll_apps_bill_is_in_its_cost`. A `roll.toml` service builds inside its own
  container (`run`), so its build is in its running time.
- Every preview shows `cost: {sleeping_usd_month, active_usd_hour, month_to_date_usd, runs_usd}`,
  and the project shows the total.

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

### Deploy hooks

A headless CMS (Contentful, Sanity, Strapi) publishes content that a static site reads at
build time, so its previews show old content until they're built again. Vercel's and Netlify's
deploy hooks are a secret URL the CMS's publish webhook calls. roll's are the same:
- **Made per project** (the rules dialog's "Deploy hooks", or `POST …/projects/{project}/hooks`)
  with a name and branch globs. The URL is shown once and kept as its SHA-256. It's a roll secret
  (`rlb_`, checksummed), so secret scanning finds a leaked one and roll revokes it.
- **Calling it** builds every active preview of those branches again at its head, with nothing
  cached: roll apps builds with `no_cache`; roll's runtime runs a service's build at each start
  anyway, so it restarts it. Each preview's activity says which hook and from what address.
- **At most once a minute per hook:** a CMS that saves often makes one rebuild, not one per save.
- **Anything but a live hook is a 404**, so the URL tells a guesser nothing.

The rebuild is a number on the preview, part of what each service was deployed with, so a
rebuild that's asked while a deploy is under way still runs once it's done, and converging again
builds nothing more.

### A shell, files and ports in a preview

Render's shell on a preview environment and `heroku run -a <review app>`: a login shell in a
preview's running `roll.toml` service, through the Docker Engine API (an exec with a TTY, as
roll apps' shells are), over HTTP as the dashboard's terminal drives roll apps' (a session, its
screen as server-sent events, keys and resizes posted). It runs on the preview's holder, starts
or thaws a sleeping service and keeps it awake while it's open, and the activity says who
opened it. Every route of it is `admin` (rolld's catalogue), as roll apps' shells: it runs
anything with the preview's variables and branches, so its output isn't masked. A roll apps
service's shell is roll apps' own, on its `pv-` environment: the sheet's Shell tab opens that one
for it (the project's roll apps service of that name, as Promote finds it), so every service of
a preview has its shell in the same place. A redeploy ends a shell with the
container it was in; a preview that goes ends its shells first.
`roll preview shell <preview> [--service]` makes this terminal the shell's (raw mode, its size
followed) until it exits. Files go in and out the same way (`…/files`, `roll preview cp <preview>
./seed.sql :/tmp/`, `roll preview cp <preview> :/app/out.csv .`, as `roll apps cp`: `docker cp`
in the service's container, at most 100 MiB a time, admin, said in the activity). A port it
listens on comes to a laptop the same way (`…/forward`, `roll preview forward <preview> 9229`, as
`roll apps proxy`): a relay in the service's network namespace (`--network container:`, as
screenshots join it) reaches `127.0.0.1:<port>` there whatever network the preview has, Roll-only
egress included, removed with each connection. `a_shell_in_a_previews_service_is_said_and_bounded`;
`files_go_in_and_out_of_a_previews_service_and_are_said`,
`a_forwarded_port_carries_bytes_is_said_once_and_is_bounded`; `scripts/preview-cost.sh` types into a
live preview's container through `roll preview shell` and reads its `ROLL_PREVIEW` back, puts a
file in with `roll preview cp` and brings it back, and reads the service through `roll preview
forward` (no relay left after).
An agent drives a page too (GitHub Copilot's coding agent with Playwright; roll MCP's
`previews_try` and `previews_try_screenshot`): `POST …/try` `{service?, path, steps}` loads the
service's page in a browser in its network (locked down as a screenshot is) and takes the steps,
a page state's (`click`, `type`, `choose`, `press`, `wait`; 10 at most, 15 s), stateless: nothing is held
between calls, so an agent sends the whole way to a state each time. It answers `{run, status,
step_failed?, text, errors}`: the page's status, the step that found nothing ("click #nope:
nothing matched"), its visible text after the steps (its own line breaks kept, cut at 40,000
characters) and what went wrong on the way (uncaught errors, failed requests, 20 at most), masked
as its logs are. Its picture is `GET …/try/{run}/screenshot` (`image/png`, kept 5 minutes on the
preview's holder; another preview's id gets a 404), asked for only when the text isn't enough.
From a terminal it's `roll preview try <preview> [--service web] [--out page.png] /login type
'#email' ada@example.com press Enter`: the steps are words after the path, each verb taking its
own (`type` a selector and the text, `choose` a selector and the option's place from 1, the
others one).
Two runs at a time per preview (a third is a 409), each taking one of the node's screenshot
slots; typed text is never kept or said by previews. Both are `reveal` (its clicks can change the
preview's own data). A roll apps service's runs in its instance's network on the node that runs
it, as its screenshot does (a signed hop on a fleet; a 409 where roll apps can't be joined,
Fargate). Checked in the screenshot image (a click changed "0 items" to "1 item", a missing selector named, a failed request listed)
and live in `scripts/preview-cost.sh` and, on roll apps, `scripts/preview-apps.sh`.
`an_agents_run_on_a_page_answers_what_it_showed_masked`, `a_runs_answer_is_bounded`,
`an_apps_instance_is_screenshotted_on_its_services_holder`.
A data branch can be looked at from the sheet too (Neon's Tables, Supabase's table editor): a
Postgres branch's "Tables" lists the tables its own role can read (with about how many rows), and
one's first 50 rows, cells as text cut at 200 characters (`…/data/{kind:parent}?table=`). It's
read as the branch's own role, in a read-only transaction of at most 5 s with only the catalog in
its path, so it sees what the preview's code sees (masked as the preview's is), and a table is
taken only from that list, its name quoted, never run. Only a branch that's ready and masked with
today's rules, as `connect` gives (409 otherwise); another kind's is a 409 until its product has
a way to show it (connect to it with `roll preview connect`). It's `reveal` (copies of
production's data). `a_previews_data_branch_is_looked_at_from_the_sheet`,
`a_browsed_tables_name_is_quoted`.
A page can be read through roll too (Vercel MCP's `web_fetch_vercel_url`; roll MCP's
`previews_fetch`): `…/fetch?path=/checkout` (`roll preview fetch <preview> /checkout`) answers its
status, the headers worth seeing (content type, location, cache control, length), and its text up
to 256 KiB, masked as its logs are, so an agent checks the page its change touched without anyone
making a share link. It goes through the runtime's own proxy past the preview's gate (no token
asked or answered), to the preview's own service only, `GET` or `HEAD`, with no body; it's
`reveal`, as its mail is (a page can show copies of production's people). A roll apps service's
page is read through roll apps' own proxy, by its host on the node's apps listener
(`--apps-listen`; on a fleet, forwarded to wherever its instance runs), as a visitor who came in by the preview's link: its
token as the cookie roll apps' gate takes, never in the answer (live: `scripts/preview-apps.sh`).
A team-access preview's roll apps service takes only signed-in people, so it answers its 401.
`a_page_read_through_roll_passes_the_gate_bounded`.
A service can also be swapped for a process on a laptop (Signadot's local sandboxes,
Telepresence's intercepts): `roll preview swap <preview> 3000 --service web` opens
`…/swap?service=web`, a WebSocket carrying one HTTP/2 connection the laptop serves, and while
it's open every request for that service goes there instead of its container: its visitors',
and its other services' (they reach it by its URL). The laptop's process works against the
preview's data and services (`roll preview connect`, `roll preview env`) and is debugged there,
with no deploy in between. The CLI sends each request to `127.0.0.1:<port>` as that host (a dev
server's host check passes; `x-forwarded-host` keeps the preview's) and says each on stderr. The
request is the one the container would get: past the preview's gate, roll's access cookie and
bypass secret taken out. A WebSocket from the preview isn't carried (501). One laptop at a
time per service (409), on the preview's holder; it's admin (the laptop gets the preview's
visitors' requests), and the activity says who swapped it and when it was given back. Ctrl-C,
a dropped connection or the preview going gives it back to its container. A roll apps service
is served by roll apps' proxy, so it can't be swapped (409).
`a_swapped_service_is_answered_by_the_laptop_until_given_back`; `scripts/preview-cost.sh` swaps
the live preview's web for a local server, reads it through the preview's host, and reads the
container again once it's given back.

### Traces

With the project's `traces: true` (the rules dialog's Traces), a preview's `roll.toml` services get
the OpenTelemetry SDK's own variables: `OTEL_EXPORTER_OTLP_ENDPOINT` to the preview's intake on
rolld (`…/v1/previews/{id}/otlp/{token}`, its token derived from the preview's id and masked in what
it shows), `OTEL_SERVICE_NAME` the service's, traces exported and metrics and logs not (its logs are
its output already). An app with an SDK sends its spans with no setup (Vercel's preview traces, roll
apps' `traces`). Turning it on restarts the runtime's services (it's in their deploy's fingerprint).
Spans are kept on the preview's holder, in memory, its newest 20,000 for six hours, and go with the
preview: they're for looking at it now. The sheet lists recent traces under Requests, each opening
to its spans as bars on the trace's time, and above them each route's time (p50, p95) against
main's preview's same route, the clearly slower ones first and in red; `roll preview traces
[--trace <id> | --routes]`. The holder reads the slower ones (at most five, the most slower than
main first) onto the preview's record about once a minute, off the reconcile pass and a few
previews at a time, main's routes once a round and only on main's own holder (where its spans
are: elsewhere nothing is said rather than everything); only those few names are masked, each
once (`slower_routes`, kept in the store's
`runtime_facts` so any node shows them): the sheet's verdict strip says "2 routes slower than main" (opening
Requests), and the pull request's comment says it in its checks with each route's p50 and
main's, a route's name in a code span nothing in it can close. A change of which routes are slower
(not of their times, which drift with every trace) reports the preview again
(`a_route_slower_than_mains_reaches_the_record_and_the_report`).

New errors against main (Sentry's new issues in a release): every five minutes, in the same round
off the reconcile pass, the holder reads each running change's or branch's error groups of the
last hour and main's preview's (once a round, and only while main's runs on this node: otherwise
nothing is said rather than everything), compares them by service and fingerprint (the
backends' own, before previews key them), and keeps the ones main's doesn't have on the record
(`new_errors`: how many, and the most frequent five by service and title, masked and shown to
120 characters; their values are read only when that set moved). The verdict strip leads with
"2 new errors against main" (opening Errors) and the change page lists them; the pull request's
comment only counts them by service (their lines can hold a branch's data, and anyone reading the
pull request reads its comment). A change reports the preview again; an error fixed (gone from
the last hour) goes. A preview asleep keeps what it last said. An error whose line holds a
per-preview value reads as new only if it's new without them: a group main's has under another
fingerprint is the same one when their titles match with each preview's own names (its id and
slug, its branches' refs, its services' hosts) taken out.
`an_error_mains_preview_doesnt_have_reaches_the_record_and_the_report`.

Where a slower route's time went (Sentry's span comparison): for each slower route, each span
name's median time under it (a trace's spans of one name summed, so an N+1's queries are one; a
trace without the span counts as 0) here and on main's preview, and the one that grew most, by
20 ms or more, is said with it (`grew: {span, ms, main_ms}`): on `…/routes`' slower rows, the
sheet's route table ("SELECT orders +280 ms"), the record's `slower_routes`, the change page and
the pull request's comment (checked live in `scripts/preview-cost.sh`, against main's preview
running beside the change's, with `ROLL_PREVIEWS_COMPARE_EVERY=5`). Only spans this node keeps compare (main's on its holder); a roll
apps service's traces have no spans here, so its routes say no reason.
`a_slower_routes_time_is_put_on_the_span_that_grew`.

Regressions as a check (Lighthouse CI's budgets, Sentry's regression gates): with the project's
`regressions: "check"` (the rules dialog's "Regressions against main"), a change's head gets the
`roll/regressions` commit status, `pending` until the holder has compared it with main's preview at
that head (errors, and routes when the project keeps traces; main's preview asleep still
compares, held elsewhere doesn't), then failing while its preview has routes slower than main's or
errors main's doesn't have (each read is bound to the head it was read at, so a push never shows
the head before's) ("1 route slower, 2 new errors against main": counts only, a status is
public) and passing once they're gone or accepted. `regressions: "required"` also keeps it from
landing in roll (a `regressions` blocker in `Code::blockers`) until then. Someone other than the
change's author accepts them at its head (`POST …/regressions/accept`, the sheet's "Accept them";
a required rule's by a person signed in to roll, never a token or the operator's token; the
author is read for sure, and an unknown one refuses; the person is told apart from the author by
every name they go by, their roll handle and each linked GitHub login (roll accounts, which
follow a GitHub rename at the next sign-in or link; names that can't be read refuse, 500); a signed-in person must also be able to write
to the change's repo in roll code, roll code's bar for a landing decision, as GitHub's Accept is): the acceptance names what it accepted, so
anything new at the same head asks again, and nothing (or nothing compared yet) is a 409. The
status passes with who did ("Accepted by ada: …"), and a new push asks again. The blocker also
waits while the change's preview isn't compared at the change's own head. `an_error_mains_preview_doesnt_have_reaches_the_record_and_the_report`
(accepting), `code-control/tests/queue.rs` (the blocker).

Route budgets (Lighthouse CI's budgets beside its comparisons): a project's
`github.budget.routes` (`{"GET /orders": 300}`, the rules dialog's "Route budgets", at most 50,
each 1 to 600000 ms) holds a route (a trace's root span name) to a p50 of its own over the last
hour's traces (5 traces at least). A route over it joins the record's `slower_routes` with its
`budget_ms`, whether or not main's preview is slower or there at all (a route over it stays over
until it's back under 90% of it, at that head, so one at its line doesn't flip every round), so it
counts wherever slower routes count, and with no comparison with main needed (`roll/regressions`
fails and the blocker waits on it alone, and it can be accepted; an acceptance names each budget, so
a changed budget asks again): the verdict strip (danger), the change page, the pull request's comment
("budget 300 ms"), `roll/regressions` and, required, the blocker; `…/routes` rows carry `budget_ms`
and `over_budget`, and the sheet's route table says each budget. `a_route_slower_than_mains_reaches_the_record_and_the_report`.

Repeated queries (Sentry's N+1 detection): a trace whose spans run one database statement
(`db.statement`, or the newer `db.query.text`) ten times or more says it as `repeated`
(`{statement, count}`), the statement's shape with its literals taken out (`SELECT … WHERE id = ?`,
an `IN` list as one) and masked as its names are. The trace list and the route table (`…/routes`,
a route's worst trace) tag it N+1, an opened trace says what it runs, and `roll preview traces
--routes` prints it under the route. `a_statement_run_many_times_in_a_trace_is_said_by_its_shape`.
Read back masked as
its logs are: a span's name, service, attributes and ids are its code's words (the OTLP reader
refuses ids that aren't 32 or 16 hex characters; a secret's value sent as one shows as `***`). The intake reads OTLP with roll
apps' reader (`apps_control::otlp::spans_of_request`). A roll apps service's traces are roll apps'
own: with the project's `traces` the preview's `pv-` environment gets roll apps' `traces: true` (its
own OpenTelemetry variables and intake), and the preview's list and trace reads take them from roll
apps too, masked the same way, one list for every service. `a_previews_traces_are_kept_by_its_token_and_read_masked`;
`scripts/preview-cost.sh` posts a span from inside a live preview's service to its own endpoint and
reads it back as the preview's trace.

An error group (the Errors tab, `…/errors`) whose newest sample names a trace the preview keeps (a
run of 32 hex digits on its own: `trace_id=…`, `"traceId":"…"`, a `traceparent`; most SDKs log the
request's trace id with its error) carries it as `trace`: the Errors tab shows that request's spans
under the sample, and `roll preview errors` prints its id for `roll preview traces --trace`. Only a
kept trace counts, so a hex id that's something else links nothing.

### Feedback on the preview page

Vercel Comments, Netlify's drawer: people reviewing a preview leave notes on its pages, and they
land on the change.

- The preview's proxy adds `<script src="/__roll/feedback.js" defer>` before `</body>` of a
  page load (`Sec-Fetch-Dest: document`; not a fetch, a fragment or an iframe), only for a
  visitor who came in through roll (the preview's access cookie, or a team-access session: a
  person signed in to roll, "Access") and when the project's
  `feedback` rule is on (the default). It asks the service for the page uncompressed and adds
  the tag as the page streams (streamed rendering stays streamed, memory stays one chunk);
  the page is then `Cache-Control: private` with no `ETag`. Anything compressed, not HTML or
  not a 200 passes through untouched. The script is served from the preview's own host, so a
  `script-src 'self'` policy allows it.
- The script (no dependencies, in a shadow root) shows a small button; it picks an element,
  takes a note, and opens a small window of roll's (480 × 640, Vercel's comments posting in
  place) with the page's path (never its query, which can hold a token):
  `/dashboard/previews/note?preview={id}&fb_path=…&fb_selector=…&fb_note=…`, the note's form
  alone. Posted, it closes, and the page reads its list of notes again when it has the focus
  back. The Reply and Resolve of a note it lists open the same window with that note's thread
  alone (`fb_reply`/`fb_resolve`; Vercel's threads answer in place), which closes once the note
  is resolved. On a phone's width it opens the preview's sheet in roll's dashboard in a tab instead
  (`/dashboard/projects/{project}/previews?preview={id}&fb_…`). Either opens with `noopener`, so
  the page gets no handle on roll's window, and the preview's origin never holds a roll
  credential: the person, signed in to roll there (a sign-in comes back to the form), reads the
  note and posts it (nothing is posted from the link alone).
- roll posts it as a `comment` review on the change ("**Feedback on the preview** · `/checkout`
  linked to the preview · `button.pay`", then the note), which reaches the pull request like any
  review. The page's own names are kept to one inert line each.
- A note carries what went wrong on the page (Netlify's drawer, Jam): the script sends the page's
  uncaught errors and rejected promises since it loaded, and the requests answered 400 or more,
  as the browser's own resource timings tell them (earlier ones too, before it loaded; roll's own
  `/__roll/` ones left out), each a path and a status, never a query. At most 5 of each, each cut
  to 200 characters, plus the browser (`Chrome 131 on macOS`, from the user agent). Nothing of
  the page's is wrapped (`console`, `fetch`), so its devtools stay its own, and a
  `console.error` the page catches isn't sent. The browser goes on the note's `<sub>` line.
  Errors and requests go in a folded "On the page: 1 error, 2 failed requests" block after it,
  so `roll_previews::notes` never reads them as the note. They go only if the person keeps them:
  the form in roll lists them, with a box to leave them out (a page's own words can hold what a
  public pull request shouldn't).
- The button also lists what's been said about the page already (Vercel's comment pins): the
  proxy answers `/__roll/feedback.json?path=…` for the same visitors only (link access or a
  team session, through roll), from the change's feedback reviews (`roll_previews::notes` reads back the shape the
  dashboard posts and nothing else; dismissed ones don't show), the latest 50. Each note says
  who and when, and outlines its element on hover; all of it is text, never HTML.
- A preview's own services get roll's word for a cloud (Vercel's OIDC federation; roll apps'
  `pv-` services have their own, "Instances' identity"): with `oidc_audiences` (clouds' relying
  parties, `sts.amazonaws.com`, a GCP pool provider; at most 5), each `roll.toml` service of a
  trusted preview gets `ROLL_OIDC_URL` (an address of its own on roll, derived from the preview,
  the service and the store's key, masked in what the preview shows) and `ROLL_OIDC_AUDIENCE` (the
  first). A `GET` (`?aud=` one of the list; the first when not given) answers `{token, expires_at}`:
  roll secrets' issuer's token, an hour long, `sub = roll-previews:<project>:<preview>:<service>`,
  with `project`, `preview_id`, `service`, `environment` (`pv-<id>`) and `preview: true` among its
  claims, so a trust written for an app's subject never matches it. An audience the rules don't
  name is a 403, an address that isn't one a 404; a token is answered again for a minute per
  service and audience, and masked in the preview's output once issued. roll secrets refuses a
  fork's preview itself (from its environment's own `trusted`), and rolld asks that the project
  stands for the service's repo. A cloud trusts one preview's service by its `sub`; trusting a
  service across a project's previews (`roll-previews:<project>:*:<service>` as a condition) is the
  customer's call, and roll writes no such trust. `oidc_audiences` is a person's rule (roll MCP
  doesn't set it). `a_previews_service_gets_roll_s_word_for_a_named_cloud_only`.
- The toolbar's **Main** opens the page the reviewer is on (its path and query) on main's preview
  of the same service, in a new tab (Vercel's deployment switcher), to see how it looks there:
  `GET /__roll/main?path=` on the preview's own host answers `{url}`, for a team visitor only (a
  share link's guest has no main to see), or 404 when main's preview doesn't run that service (the
  button then says "No main preview"); a path that isn't one on that host (`//…`) is none. The
  script reads whether it's served by roll previews (`__ROLL_MAIN__`: `false` on roll apps' hosts,
  whose pages keep the button hidden and never send it to the app).
- A reviewer sets the project's feature flags for their own view (Vercel's Flags Explorer): with
  `flags` (`[{name, options}]`, at most 20, each name and option 1-60 letters, digits, `-`, `_` or
  `.`), the page's toolbar gets "Flags": each flag a choice (the app's own, or one of its
  options), applied to this browser only by reloading with a cookie (`roll_flags`). The preview's
  proxy turns it into the header the services read, `x-roll-flags: new-checkout=on,theme=dark`,
  only what the project's rule names with one of its options; the cookie never reaches the
  services, and an `x-roll-flags` a visitor sends is dropped. The app reads the header where it
  decides a flag (a flags SDK's override hook); a visitor without the toolbar gets the app's own
  choices. roll MCP's `previews_set_rules` takes `flags` (an agent that adds a flag to the code
  offers it on the toolbar too). `a_reviewers_flags_reach_the_service_as_a_header`.
- A note says what the reviewer did before it (Jam's user actions, the light part of Netlify's
  recordings): the page's script keeps the last 20 clicks (by a selector), inputs ("type 11
  characters in #coupon": how many, never what), choices ("choose option 2 in select#size": the
  option's place, never its words), form submits, Enter and Escape, and page
  changes (the address looked at twice a second; none of the page's own functions wrapped), each
  with how long before the note; roll's own toolbar and the element picked for the note aren't
  steps. They go with the errors and requests in the note's folded "On the page" block ("What
  they did:", numbered), only if the person keeps them (`fb_steps`; checked in Chromium). The
  page's toolbar is also driven in a browser by `scripts/preview-cost.sh` (in through the
  preview's link, an element picked, a note posted): #861's Flags form came before the note's in
  the toolbar's markup and the note's lookup took it, so picking an element opened the wrong
  form until this fixed it (`form.card`).
- A note or an error becomes a fix to make (Vercel Comments' "Create issue", Netlify's drawer):
  the note's sheet (opened from its Reply or Resolve) and each error group on the sheet's Errors
  tab have "Make a work item", where roll's agents claim work too. A note's is made by roll
  (`POST …/notes/{note}/work`, `roll preview notes <id> --work <note>`): on a repo mirrored to
  GitHub, a team member's note opens an issue there, by roll's App (Marker.io's and Jam's notes
  to GitHub Issues), made the repo's work item at once (GitHub's `issues` webhook finds it), by
  roll's own number, as the team's words (an issue roll's App opened is never someone outside
  the team's, whatever GitHub says of its bot);
  elsewhere, and always for a guest's note (its words are never published to GitHub, and stay
  marked as outside the team's), a work item in the change's repo,
  titled by the note's first line, its body the note with its page, element and link, labelled
  `preview-feedback`, once a note (kept, shown with the note as `work`), and the note gets a
  reply naming it. A guest's note (a share link's) makes an item marked as someone outside the
  team's words, so MCP's lockdown withholds them from an agent that claims it; made from the
  browser as the person, it wasn't. It's roll code's resource, so the caller must write the
  repo's code too (their grant asked inside the request) (`engine.rs::a_note_becomes_a_work_item_once`). An error's
  item is made in the preview's repo as the person looking (roll code's `POST …/work`; someone
  without write on the repo gets its refusal). It takes its service
  and title, and its sample as the preview shows it (masked), in a block nothing in it can close,
  labelled `preview-error` (`lib/preview-work.ts`).
- A note is resolved from the page too (Vercel Comments): its Resolve opens roll
  (`?fb_resolve=<review id>`), where the sheet shows the note and anyone on the project resolves
  it (`POST …/notes/{review}/resolve`, kept by previews: roll code's comment reviews aren't
  dismissed; from a terminal `roll preview notes` lists the open ones and `--resolve <id>`
  resolves one), so the button stops listing it. The page holds no credential for this either
  (live: a resolved note folds away).
- A note shows what it's about (Vercel's comments carry a picture): the next screenshot run of
  the change's preview crops each open note's element from its page by its selector, in the
  screenshot's own container at the desktop width (scrolled into view, a 24 px margin, at most
  1280 by 800). The visitor's browser runs no capture code, so the page's code can't forge the
  picture, and a selector roll refuses (`selector_ok`) is never sent. At most 10 notes a run,
  each picture at most 512 KiB, 50 kept a preview; a note already cropped isn't asked again, and
  one whose element isn't on the page has none. `GET …/notes/{note}/crop` serves it, the notes
  list says `crop`, and the sheet shows it with the note, as does the change page's review
  timeline in roll (beside the note's review).
  `a_notes_element_is_cropped_by_the_next_screenshot`.
  As its reviewer saw it (Jam's recordings, without trusting the reviewer's browser): a note
  says the reviewer's viewport and their last steps (#879), so roll's browser takes it at that
  viewport (a phone's emulated as one) after taking their clicks again (checking a box is one),
  their typing (as many `x`s as they typed: the button keeps how many characters, never what),
  their choices in a list (the button says the option's place, "choose option 2 in
  select#size", never its words) and Enter or Escape, from their last page change on, the last
  10, 10 s at most. Roll's own browser
  does it, so the page's code still can't forge the picture. The crop says how it was taken
  (`crop_as` in the notes list, `X-Roll-Crop-As` on the picture: "390x844, after 2 steps";
  the sheet's "As they saw it"). In their browser's engine too (the note's "Safari 18 on iOS"):
  every browser on iOS is WebKit at a phone's size, Safari elsewhere WebKit, Firefox Firefox;
  those crops go to the other browsers' run (`browsers.js`, Playwright; it runs for them even
  without `github.browsers`), the same viewport and steps, a 512 KiB picture at most, only for
  a note asked; the label names the engine ("WebKit, 390x844, after 1 step"). Chromium's run
  takes the rest. `a_note_says_how_its_reviewer_saw_the_page`; live, a Safari-on-iOS note after
  a click, cropped in WebKit.
- What the reviewer saw, from their own browser (Jam's and Marker.io's screenshots): the note
  form's "Attach my screen" asks the browser for this tab (`getDisplayMedia`,
  `preferCurrentTab`), only on that click, never on its own. One frame is drawn to a canvas
  with roll's card and buttons hidden (the picked element's outline stays), the capture stops
  at once, and the frame is scaled to at most 1600 px a side and sent as WebP (JPEG where the
  browser can't) to `POST /__roll/screen` on the preview's own host (from its own origin, the
  visitor's cookie the credential, as `/__roll/notes`). A browser that refuses, or a reviewer
  who says no, leaves the note as it was: it still posts. roll reads the picture by its own
  bytes, never by its type: a PNG, JPEG or WebP of at most 1 MiB and 1600 by 1600, decoded whole,
  or a 400 (413 past 1 MiB). It waits half an hour for its note (20 at most per preview for the
  team, 20 per share link; a guest's only while their link's 10 notes an hour aren't spent, 429
  after) under a one-use token: a guest's note carries it (`screen`) and keeps it as theirs;
  for the team the page hands it to roll's window (`fb_screen`), which posts the note and then
  attaches it (`POST …/notes/{note}/screen`, the note's own author only). It's kept as the
  note's picture, in the crop's place (`GET …/notes/{note}/crop` with its own type and
  `X-Roll-Crop-Own: true`; `crop_own` in the notes list), so the next screenshot run doesn't
  crop that note, and a crop it was already taking never replaces it. The sheet's note, its
  thread and the change's review show it as "Their own screen", and a public preview's pull
  request comment links it as the crop's, except a guest's (or another outside reader's): the
  pull request shows no image of theirs, as their words have none, and their picture is marked
  outside (`X-Roll-Outside`) like the rest of their note. The page's code can draw what it
  likes on its own tab, so this is the reviewer's view, not roll's proof of the page.
  `a_reviewers_own_screen_is_kept_with_their_note`.
- The open notes reach the pull request (Vercel's comments): the preview's own comment, updated
  in place, lists them (newest first, at most 10), each with its page, who and what as plain
  words (one line, markdown escaped: a note can't link, embed an image, mention or close
  anything), and a public preview's signed link to its crop (`screenshot.png?note=<id>`, a
  signature of its own per note, as the screenshot's). Resolved ones drop out on the comment's
  next update. `the_pages_notes_are_listed_as_plain_words`.
- A note is a thread (Vercel Comments' replies): its Reply opens roll (`?fb_reply=<review id>`)
  on the same view, the note with its replies, a reply box and Resolve. A reply is a `comment`
  review on the change starting "**Reply on the preview** · `<the note's id>`"
  (`roll_previews::notes::thread` reads them back, the latest 20 per note, oldest first; one
  naming any other review joins nothing), so it reaches the pull request as a GitHub review like
  the note did. The page lists the replies under their note, as text, and `roll preview notes`
  under each note too (live: a reply listed under its note).
- People are told by mail (Vercel's comment notifications; `note_mail`, on unless turned off):
  a new note goes to the change's author, a reply to the note's author, and a resolve to the
  note's author ("bob resolved your note"), each to their own address through roll's accounts
  (`Accounts::mail_user`, never an address a caller names), and only while they may open the
  project's previews (as a share link's maker must). Never to a guest (no account), nor to
  whoever wrote it. roll code tells previews of each review as it's made
  (`PreviewBackend::reviewed`, after its event); previews keeps what to send (`note_tells`, once
  per review and person) and its loop sends it, each claimed first so one node does, tried
  again after 1 and 5 minutes when it couldn't go. GitHub tells a mirrored pull request's
  people already; this is for a roll-only change. `a_notes_people_are_told_by_mail`.
- Notes as Slack threads (Vercel's comments in Slack), both ways, through roll code's chat ops
  (docs/code/chatops.md): the "Preview notes as Slack threads" automation template (trigger
  `code` on `review` events, step `slack.note` with a channel) posts each new note as a message
  (the page, linked to the preview, the element, the note quoted) and binds its thread to the
  note (`chat_notes`). A message in that thread, from someone the Slack install allows, becomes
  the note's reply on the change, signed "— Ada, in Slack" (the person's Slack name), made as
  the automation through roll's API, and a reply made on the page goes into the thread. What
  came from Slack is never posted back to it, and a note is posted once. Resolving a note (roll's
  Resolve) leaves the reply "Resolved." on the change as the person who resolved it, so the pull
  request and the note's Slack thread hear it too (`a_pages_notes_by_its_environment_name`). The note's format is roll code's (`code_control::review::preview_note`,
  read back by `roll_previews::notes`). `preview_notes_are_slack_threads_both_ways`.
- Previews deployed on roll apps go through apps' own proxy, which adds it for its link
  visitors only: under team access (apps' `signed_in` environments) they get none yet.
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
restore_points(preview_id TEXT NOT NULL, point TEXT NOT NULL, label TEXT NOT NULL, by TEXT NOT NULL,
               at INTEGER NOT NULL, kind TEXT NOT NULL, parent TEXT NOT NULL, name TEXT NOT NULL,
               ref TEXT NOT NULL, kept TEXT, source TEXT NOT NULL,
               mask TEXT NOT NULL,         -- the masking rules' digest it was masked by
               PRIMARY KEY (preview_id, point, kind, parent))   -- a row per branch it holds
```

- A preview id is `pv_` followed by 12 random characters of lowercase base32. Times are unix
  milliseconds.
- `resources.ref` is null until the branch exists. `bytes` is refreshed every 5 minutes.

## HTTP contract

Bodies are JSON. Every error is `{"error": string}`. Previews are a member product
(docs/members.md): a signed-in person who isn't an admin reaches the previews of the projects
they're a member of, each route as their role there allows, by roll-access's catalogue (a preview
is in its project, and its environment, `preview`, is never protected): a viewer reads a preview,
its logs, traces and screenshots; a developer also reveals what holds credentials (`…/env`,
`…/connect`, mail, share links) and writes (sleep, wake, refresh, its variables); an admin or owner
also opens shells, files and ports, deletes and resets, accepts reviews and manages the project's
rules, bypass secret and hooks. Another project's preview, or a project they aren't in, is a 404;
what their role lacks is a 403 naming it. Starting a preview by hand (`POST /v1/previews`, `roll
preview up`) makes something in the project: an admin's, the request naming the project in
`X-Roll-Project` (as roll-access reads every create; the dashboard and the CLI send it). The handlers see the person
(their session's grant isn't a token): their name on an accept, a person's accept for a required
rule. Tokens (their scopes are roll MCP's catalog, and a rule naming a project reaches the
previews in it), the operator and admins see every project.

| Route | Body | Answer |
| --- | --- | --- |
| `GET /v1/previews?project=&state=` | | `Preview[]`, newest first, light (`Preview::light`): every count and state a row reads, without each page's accessibility issues, links and comparison, the first page's issues' markup and axe-core's words, or the words against main (by the bounds, about 130 KB a preview whose pages have the most violations, down to a few KB); `GET …/{id}` has all of it |
| `POST /v1/previews` | `{project, repo, branch? , change?}` | 201 `Preview` (or 200 with the project's existing one). 404 unless the repo is on the project's canvas and the project stands for it (`Workspace::trusts`, as for the previews it makes itself: a preview builds and runs the repo). 409 when the change's preview is another project's (a change has one; it's never handed to another project); 400 when `X-Roll-Project` (the project roll-access decided the create on) names another project than the body |
| `GET /v1/previews/events?after=&limit=&latest=&wait=` | | `{events: Event[], next}`: what happened to previews after the seq `after` (0 by default), oldest first, at most `limit` (1–500, 100 by default); `wait` (1–25 seconds, not with `latest`): with none yet, the answer waits that long for one (a long poll: woken at once by an event this node records, another node's read within 2 s), then answers empty; `next` is where to ask from again (past events the caller doesn't see too: a signed-in person sees their projects'). `latest=1`: `{events: [], next}` with the newest seq, to follow from now on (400 with `after`). `type Event = {seq, at, kind: "created" \| "ready" \| "failed" \| "visual_change" \| "browser_checks_passed" \| "browser_checks_failed" \| "deleted", project, preview, change: string \| null, data: {head}}`: `ready` is up after a push or the first deploy (asleep once up counts), never a sleeping preview waking; `visual_change` is a visual change waiting for review, once a head; `browser_checks_passed` and `browser_checks_failed` say production's browser checks have all run on the head, once a head. Each event's `seq` is taken in the write that made it so, so commit order is seq order and a follower past one never misses an earlier one; kept 7 days. roll MCP's event feeds follow it |
| `GET /v1/previews/{id}` | | `Preview` |
| `DELETE /v1/previews/{id}` | | 202 `Preview` (`desired: "deleted"`) |
| `POST /v1/previews/{id}/refresh` | | `Preview`: redeploy now |
| `POST /v1/previews/{id}/sleep` | | `Preview`: stop its services now |
| `POST /v1/previews/{id}/wake` | | `Preview`: start its services now |
| `POST /v1/previews/{id}/reset` | `{resources?: ["postgres:prj_…"]}` | `Preview`: re-branch from the parents |
| `POST /v1/previews/{id}/rehearse` | | 202 `Preview`: its next converge rehearses the change's release on fresh, masked branches of its Postgres parents as they are now (`rehearsal`); 400 for a default branch's preview, 409 without a `release` or a Postgres branch |
| `GET /v1/previews/{id}/restore-points` | | `{points: [{id, label, by, at, data}], max}`: oldest first, `data` the resources it holds (`postgres:prj_…`) |
| `POST /v1/previews/{id}/restore-points` | `{label?}` (one line, 60 characters at most) | 201 `{id, label, by, at, data}`: its Postgres, NoSQL, Redis and stream data saved now; 409 while it isn't ready, past `max`, or with nothing to save |
| `POST /v1/previews/{id}/restore-points/{point}/restore` | | `Preview`: the data put back as the point left it, on its next converge (no seed); `delete`, as a reset is |
| `DELETE /v1/previews/{id}/restore-points/{point}` | | 204 |
| `GET /v1/previews/{id}/screenshot` | | `image/png`: the first service's page at the preview's head (404 when it has none); `?previous=1` the one before, `?view=phone` at a phone's width, `?base=1` the default branch's preview's, `?page=/pricing` another of `github.pages`, `?diff=base` or `?diff=previous` where it differs from that one or the one before (the page faded, changed pixels marked red) |
| `GET /v1/previews/{id}/screenshot.png?exp=&sig=` | | `image/png` behind a signed, expiring link, without credentials (403 otherwise): the pull request comment's image; `&diff=base` under its own signature, the diff against main |
| `GET`/`POST /v1/previews/{id}/shares` | `{hours, label?, notes?}` (1–720 hours; `label` who it's for, one line of 60 characters at most; `notes` its visitors may leave notes as guests) for a new one | `Share[]` (never a token), or 201 `Share` with `url`, the only time the link is shown; 409 for a public preview or past 20 live ones. A link works only while its maker (the person, or the person behind the token, that made it; not the operator's) may still open the project's previews: an account that still signs in, with a role that reveals a preview. Asked again at most every few minutes while it's used; one whose maker may not ends at once ("ada's share link ended: …" in the activity). `visit::tests::a_signed_in_person_gets_into_a_team_preview` |
| `DELETE /v1/previews/{id}/shares/{share}` | | 204: the link stops at the visitor's next request |
| `GET`/`POST /v1/previews/{id}/webhooks` | `{path, label?}` (`path` its first service's own: no query, `#`, `%`, `\`, `.` or `..` segment, nor `/__roll/`; `label` what it's for, 60 characters at most) | `GET`: `[{id, service, path, label, created_by, created_at, revoked, hits, last_at}]` (never the URL); `POST`: 201 with `url` (`<first service's URL><path>?roll_hook=<token>`), shown only here; 409 for a public preview, a fork's, or 20 already |
| `DELETE /v1/previews/{id}/webhooks/{webhook}` | | 204: it lets nothing in from the next request |
| `GET`/`PUT /v1/previews/{id}/env` | `{vars: {NAME: value \| null}}` (`null` removes; at most 100, 32 KiB each; a value may be `roll://ENV[/folder]/KEY`) | `{vars}`: the preview's own variables; its services redeploy with them. 403 for a reference to an environment other than the base without the caller's reveal on its folder; 400 for a malformed one or a look-alike |
| `POST /v1/previews/{id}/approve` | `{head}` (the head looked at) | `Preview`: that head of a fork's pull request on a Git URL service runs (`pull.approved_head`); the next push waits again. 409 when the preview moved on to another head or isn't a fork's pull request. Admin in the permissions catalogue, and no MCP tool: letting outside code run with the project's data is a person's call |
| `POST /v1/previews/{id}/visit` | `{to}` (a URL at one of the preview's service hosts, under roll's preview domain or its project's own) | `{url}`: a one-use way into a team-access preview (`access: "team"`) at `to`'s host, good for a minute, for a person signed in to roll (a session; a token is a 403). 400 when the project's previews aren't `team` or `to` isn't the preview's; 403 for a person who may not open the project. Operator-only in the permissions catalogue: no grant ever gives one |
| `POST /v1/previews/{id}/keep` | `{hours}` (0–720; 0 releases) | `Preview`: kept past its TTL until then (`kept_until`); a landed or closed change still ends it |
| `GET /v1/previews/{id}/notes` | | `{notes: [{id, path, selector, note, by, at, replies: [{id, note, by, at}], crop, crop_as?, crop_own?, work?: {number, url}}]}`: the notes left on its pages that nobody has resolved, every page's (the latest 500); `crop`: a picture of it is kept, `crop_own` when its reviewer took it of their own tab |
| `GET /v1/previews/{id}/notes/{note}/crop` | | `image/png`: the note's element as a screenshot run cropped it from its page (`X-Roll-Crop-As`: how, when as its reviewer saw it), or the picture its reviewer took of their own tab (`X-Roll-Crop-Own: true`; `image/png`, `image/jpeg` or `image/webp`; a guest's marked outside); 404 until there's one |
| `POST /v1/previews/{id}/notes/{note}/screen` | `{screen}` (the `rsc_…` token `POST /__roll/screen` answered the page) | `{note, crop_own: true}`: the picture the note's reviewer took of their own tab kept as its picture. 403 for anyone but the note's author (the operator excepted); 404 for a note that isn't the change's or a token that's used, gone (half an hour) or a guest's. No MCP tool: a person's own step from roll's window |
| `POST /v1/previews/{id}/notes/{note}/resolve` | | `{resolved}`: a note on the preview's page (its review's id) resolved, so the page stops listing it; 404 for a note that isn't the change's |
| `POST /v1/previews/{id}/notes/{note}/work` | | `{number, url}`: the note made a work item on its change's repo by roll (a GitHub issue on a repo mirrored there, a guest's note excepted; labelled `preview-feedback`, a guest's words marked as outside the team's), said on the change; once a note (asked again, the same one; two callers at once make one, the second a 409 while it's made); 403 when the caller's grant (or a person's token's session) doesn't write the change's repo's code; 404 for a note that isn't the change's |
| `GET /v1/previews/{id}/mail` | | `{mail: [{id, at, from, to, subject, size}]}`: what its services sent, caught (newest first, the last 50) |
| `GET /v1/previews/{id}/mail/{mail}` | | One caught message as sent, as `text/plain` (never rendered) |
| `GET /v1/previews/{id}/connect` | | `{branches: [{kind, parent, vars}]}`: each data branch's connection as its services get it, the branch's own credentials (a Postgres branch's role, never the parent's or a superuser's), read from the products each time; 409 while it's being deleted |
| `GET /v1/previews/{id}/qr.png` | `service?` | The preview's link with its access (`share_url`, or that service's) as a QR code PNG, to open on a phone; `Cache-Control: no-store` (it carries the token); 404 with no link yet |
| `POST /v1/previews/{id}/regressions/accept` | `{head?}` (the push looked at; a newer one is a 409) | `Preview`: its regressions against main accepted at its head (`regressions_accepted`), the change reported again. Not by the change's author; a `required` rule's only by a person (403 for a token). 400 for a branch's preview. Admin on the preview's project |
| `POST /v1/previews/{id}/visual/accept` | `{head?, path?, group?}` (the screenshot's looked at, `visual_review.head`; `path`: that page only, `""` the first; `group`: every page changed the same way, `visual_review.pages[].group`; `stories:untaken`: the stories the change touched its run didn't take, accepted as not taken; else every page and those) | `Preview`: how a change's pages look at its latest screenshot is accepted (`visual_review`, `roll/visual`); 404 for a page it has no screenshot of; 409 with no screenshot yet, 400 for a branch's preview, 403 from the change's author (under any name they go by) or from a token or the operator's token when the review is required, 409 when the change's author isn't known or `head` isn't the latest screenshot's (a newer one came in) |
| `POST /v1/previews/{id}/visual/refuse` | `{head?, path?, group?, reason?}` (as accept's; `reason`: why, one line of 200 characters at most) | `Preview`: how a change's pages look at its latest screenshot is refused (`visual_review.state` `refused`, `roll/visual` fails) until a new head or an accept; 400 for a longer reason, 404 for a page it has no screenshot of, 409 when a newer screenshot came in (`head`) |
| `GET /v1/previews/{id}/logs?service=&lines=` | | `{service, lines: string[]}`, masked: every value the preview got from roll secrets, the project's `env` and the preview's own variables, and its branches' credentials (a password, key or token, a URL's password) are `***`, as checks' output is (each line of 6 characters or more). A preview's code, a fork's too, can print what it was given, and anyone who reads the preview reads its logs. 409 when what they must hide can't be read. A seed's, release's or teardown's output (the preview's error, a teardown's in the pull request's comment), what its page said in a screenshot run (its errors and failed requests, broken links, words, markup and API operations: the comment's "What went wrong") and the hosts its egress proxy refused (a name is its code's to choose) are masked the same way (a picture can't be: a page that renders a secret shows it). Masking matches values as written, so code that prints a secret encoded (base64, hex) gets it through, as with every log masker (GitHub Actions' too). It hides what a preview shows, not what its code sends: with egress open (the default) its code reaches the internet, DNS included. Under "Roll only" it can't: the preview's network is `--internal`, where Docker's embedded DNS doesn't forward outside names (SERVFAIL; checked on Docker 29.9) and no resolver is reachable, and the egress proxy checks a name against `egress_hosts` before it resolves it, so `<secret>.attacker.example` is never looked up; on roll apps, every variable previews sets in the `pv-` environment is sealed, so apps' own logs hide it and its API never returns it (`…/connect` and `…/env` reveal them) |
| `GET /v1/previews/{id}/logs?service=&lines=&follow=1` | | `text/event-stream`: `event: service` (its name), then one event per line, the last `lines` first and then as they come; a line the service wrote on stderr is `event: stderr` (a stdout one the default event; `roll preview logs -f` writes them to its own stderr and stdout). `docker logs -f` for `roll.toml` services; for others the tail is read every 2 s (2,000 lines wide after the first read) and only new lines are sent, with a line saying so when some were missed. Runs on the preview's holder; the stream ends when the client goes |
| `POST /v1/previews/{id}/shell/sessions` | `{ service?, cols?, rows? }` (the preview's first service without one; 120×32) | 201 `{ id }`: a login shell (bash where the image has it, else sh) in the running `roll.toml` service's container, started or thawed if it sleeps (up to 60 s), kept awake while the shell is open, its opener in the activity ("ana opened a shell in web"). At most 4 a preview (409); a roll apps service's is 409 (its own shell is on its `pv-` environment in roll apps). Runs on the preview's holder. A terminal isn't masked: it's an admin's, as roll apps' shells are, and reaches what the service does (the preview's branches and variables, never a parent's) |
| `GET /v1/previews/{id}/shell/sessions/{sh}/screen` | | `text/event-stream`: base64 chunks (the scrollback first, at most 256 KiB), then `event: closed` when the shell ends (its `exit`, a close, the preview going, a minute with nobody watching: a closed tab gives its place back) |
| `POST /v1/previews/{id}/shell/sessions/{sh}/keys` | `{ data }` (base64, at most 64 KiB) | 204; 404 once it's closed |
| `POST /v1/previews/{id}/shell/sessions/{sh}/resize` | `{ cols, rows }` | 204 |
| `DELETE /v1/previews/{id}/shell/sessions/{sh}` | | 204 |
| `GET /v1/previews/{id}/files?service=&path=` | | `application/x-tar`: what's at `path` (absolute) in the running `roll.toml` service, started if it sleeps (`docker cp`), `x-roll-service` naming it; 404 before any of the answer when nothing is there; said in the activity ("ana copied /app/out.csv out of web"). Admin, as the shell is (it reads anything the service keeps); a roll apps service's is 409 (roll apps' own `…/files` on `pv-`). Runs on the preview's holder |
| `GET /v1/previews/{id}/forward?service=&port=` | WebSocket | one TCP connection to `port` in the running `roll.toml` service, any port it listens on (a debugger's, an admin's), published or not: a relay (busybox `nc`, `alpine:3.20`, 32 MiB, no capabilities) in the service's network namespace, removed with the connection; binary both ways, an empty message a half-close, as roll apps' tunnel (`roll preview forward`). The service is held awake while connections are open; at most 32 a preview (409); said in the activity once an hour per person, service and port. Admin; a roll apps service's is 409 (roll apps' own tunnel). Runs on the preview's holder (roll-cluster forwards the upgrade) |
| `GET /v1/previews/{id}/fetch?path=&service=&head=` | `{service, path, url, status, headers: {content-type?, location?, cache-control?, content-length?}, body: string \| null, truncated}` | one page of a `roll.toml` service read through roll's own way in (no link token or share link), `GET` or `HEAD` (`head=true`); its text masked as its logs are, `null` when not text, cut at 256 KiB (`truncated`); `url` never carries a token; a roll apps service's page through roll apps' proxy as a link visitor (a team-access one's answers its 401); `reveal` (roll MCP's `previews_fetch`; `roll preview fetch`) |
| `POST /v1/previews/{id}/try` | `{service?, path, steps}` (a page state's steps, 10 at most) | `{run, status, step_failed?, text, errors, login?}`: the page in a browser after the steps, masked (`login`: how its `github.login` sign-in went, when the project has one); two at a time per preview (409); `reveal` (roll MCP's `previews_try`; `roll preview try`) |
| `GET /v1/previews/{id}/try/{run}/screenshot` | | `image/png`, the run's picture, kept 5 minutes; `reveal` (roll MCP's `previews_try_screenshot`) |
| `GET /v1/previews/{id}/data/{resource}?table=` | | `{tables: [{name, rows}]}` (its tables, as its own role sees them, at most 200) or, with `table`, `{table, columns, rows: (string \| null)[][], more}` (the first 50 rows, cells cut at 200 characters); `resource` is `kind:parent`; read as the branch's own role, read-only, 5 s at most; 409 for a branch not ready or masked yet and for a kind with no way to show it yet (Postgres only for now); `reveal` |
| `POST /v1/previews/{id}/otlp/{token}/v1/traces` | OTLP/HTTP traces: protobuf or JSON (`Content-Type`), gzip or not, at most 8 MiB | `{"partialSuccess": {}}`; 404 for anything but the preview's own token (derived from its id with the store's key, as its SMTP password is). Public in rolld's gate: the token in the path is the credential. A service gets it as `OTEL_EXPORTER_OTLP_ENDPOINT` when the project's `traces` is on. Spans are kept on the preview's holder, in memory: its newest 20,000, for 6 hours |
| `GET /v1/previews/{id}/traces?minutes=` | | `{ minutes, traces: [{ trace, name, service, start_ms, ms, spans, error }] }`: its recent traces (5 to 360 minutes, 60 by default), newest first, at most 100, the root span's name masked as logs are |
| `GET /v1/previews/{id}/traces/{trace}` | | `{ trace, spans }`: one trace's spans, oldest first, names and attributes masked |
| `GET /v1/previews/{id}/routes?minutes=` | | `{ minutes, main, routes: [{ name, count, p50_ms, p95_ms, main_p50_ms, main_count, slower, repeated?, grew?, budget_ms, over_budget }] }`: each route's time from its traces (a trace's root span's name, as an SDK names a request by its route), against the same route on main's preview (the repo's default branch's, else the project's baseline; `main` its id), the slower ones first: `slower` with at least 5 traces on both sides and a p50 half again main's and 100 ms more. Names masked |
| `PUT /v1/previews/{id}/files?service=&path=` | a tar, at most 100 MiB | `{ service, path }`: its files written under the directory `path`, said in the activity. Admin |
| `GET /v1/previews/{id}/requests` | | `{requests: [{at, service, method, path, status, ms}]}`: its latest 200 requests through roll's runtime, newest first (Vercel's request log): never the query, which can carry a token, and the path masked as its logs are (a path is its code's to choose); `ms` until the answer's headers. Kept in memory on the preview's holder (runs there) and gone with its last service; a roll apps instance's requests are roll apps' own. The sheet's Requests tab, errors at a glance; `roll preview requests <id> [--errors]` |
| `POST /v1/previews/{id}/replay` | | 202 `{head, by, at, ok: null, requests: []}`: its recent GETs (its own and main's preview's request logs, newest first, each once, 40 at most, never roll's `/__roll/` paths) asked of it and of main's preview again, in the background; `replay` on the preview has them at its head once done (`{…, ok, requests: [{service, path, status, main_status, removed, added}], error}`: `removed`/`added` a JSON answer's top-level fields main's has and its hasn't, or the other way; 0 is no answer); 409 for a fork's preview, or one already replaying; 400 for a branch's (`roll preview replay <id>`, the Requests tab's Replay) |
| `GET /v1/previews/{id}/metrics` | | `{services: [{service, cpu, memory_bytes, memory_mb}]}`: what its running services use right now (Render's and Railway's metrics): cores of CPU and resident memory, against the memory each runs with. It comes from `docker stats` for roll's runtime. For roll apps' instances it's their last finished minute (`…/environments/pv-…/metrics`), with memory per instance. A sleeping service isn't listed. 404 for no such preview. It runs on the preview's holder, and is read again at most every 10 s, however many sheets ask. The sheet shows it on each running service, read every 15 s while it's open |
| `GET /v1/previews/{id}/errors?minutes=` | | `{minutes, groups: [{service, fingerprint, kind, title, count, first_seen, last_seen, sample, trace?}]}`: its services' errors in the last `minutes` (5–1,440, 60 by default, rounded up to a 15-minute step), grouped as roll apps groups an environment's (Sentry's issues and Vercel's runtime logs, from the output alone; docs/apps/http-api.md): a stack trace (Python, JVM, Node, Go) joined from its lines, or an error line, by what it is (its type and first frames with numbers and ids taken out). `fingerprint` names a group (keyed under a random key per process, so it says nothing of the error and changes on a restart); `kind` is `python`, `java`, `node`, `go` or `line`; `sample` is the newest one whole (8 KiB at most); newest first; `trace` the kept trace its sample names, if any. Title and sample are masked as its logs are, a value a cut split included (its start at a line's end is `***` too): its code can print its secrets. Its values are read (an audited read) only when there's an error. A roll apps service's are roll apps' own (`…/environments/pv-…/errors`, its 50 most frequent); a `roll.toml` service's are read from its container's last 2,000 lines in the span (`docker logs --timestamps --since`, at most 4 MiB a stream for at most 10 s), masked before they're grouped, its newest 50. A runtime that doesn't stamp lines gives none (its tail's old crash would read as new). 404 for no such preview. It runs on the preview's holder and is read again at most every 10 s for a span, however many ask. The sheet's Errors tab |
| `GET /v1/previews/{id}/errors/count?minutes=` | | `{minutes, count}`: how many errors those groups hold. Nothing in them leaves, so nothing is masked and no values are read. Kept as long. A row of the Previews tab shows the last hour's once it's on screen, three rows at a time |
| `GET /v1/previews/{id}/activity` | | `Activity[]`, newest first |
| `GET /v1/previews/{id}/visitors` | | `[{who, last_at}]`: the last 8 people signed in to roll who opened it (`access: "team"`), by their handle, latest first; `last_at` is as of their session's last 5-minute recheck. Share links and the CI bypass aren't people and aren't listed |
| `GET /v1/previews/projects/{project}/settings` | | `Settings` |
| `PUT /v1/previews/projects/{project}/settings` | `Settings` (partial) | `Settings` |
| `GET /v1/previews/projects/{project}/mask-suggestions` | | `{suggestions: [{resource, path, rule, why}], unread: [{resource, why}]}`: columns of the project's Postgres, NoSQL and search resources (whatever their rule) that look like people's data and have no mask yet, with a rule their type and constraints take ("Masked columns", Suggestions); `resource` is `masking`'s key (`postgres:<ref>`). At most 2,000 columns read per resource, each for at most 10 s; one asleep (not woken), not read in time, unreadable or with more columns is in `unread`. As the settings are: the operator and whoever sees the project; a scoped token only sees resources its grant lets it read |
| `GET /v1/previews/projects/{project}/usage` | | `{previews, sleeping_usd_month, active_usd_hour, month_to_date_usd, bytes, spent_usd_month, max_usd_month}` |
| `GET /v1/previews/projects/{project}/bypass` | | `{enabled, created_at, header}` (never the secret) |
| `POST /v1/previews/projects/{project}/bypass` | | 201 `{enabled, created_at, header, secret}`: a new secret, shown only here; the old one stops working |
| `GET /v1/previews/projects/{project}/hooks` | | `{hooks: [{id, name, branches, created_at, fired_at}]}` (never their secrets) |
| `POST /v1/previews/projects/{project}/hooks` | `{name, branches?}` (1–64 characters; globs, `["*"]` by default; at most 20 hooks a project) | 201 `{hook, path}`: `path` is `/v1/previews/hooks/rlb_…`, shown only here (kept as its SHA-256) |
| `DELETE /v1/previews/projects/{project}/hooks/{hook}` | | 204 |
| `POST /v1/previews/hooks/{secret}` | any (ignored: a CMS's own payload) | 202 `{rebuilt: [preview ids], coalesced}`: every active preview of the hook's project whose branch a glob matches is built again at its head with nothing cached (roll apps' `no_cache`; the runtime builds at each start), each said in its activity with the caller's address. At most once a minute per hook (`coalesced: true`, nothing done). No token: the secret is the credential (rolld's gate lets `/v1/previews/hooks` through); anything but a live hook is a 404. A leaked `rlb_` secret is revoked like a token (`Access::on_leaked`) |
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
  outside?: true;                    // a fork's preview: its title and its code's words are outside text, which roll MCP's lockdown withholds (its error groups, its logs' answer, its traces, spans and routes carry it too)
  runs: string | null;                // the earlier head its services run (later pushes touched only ignored paths)
  earlier: {service: string; revision: string; url: string; share_url: string | null; at: number; own_data: string[]}[]; // earlier pushes, still reachable; own_data: the products it has its own copy of (as its head left them)
  copies: {count: number; bytes: number; usd_month: number}; // the earlier pushes' own copies of its data, said apart (counted in cost)
  browser_checks: {service: string; name: string; state: "pending" | "passed" | "failed"; error: string | null; files: {service_id: string; environment: string} | null}[]; // production's browser checks on this head (roll apps); files: where a failure's screenshot and trace are kept
  browser_checks_rule: "check" | "required" | null; // the project's rule for them, when it's more than showing them
  goes_at: number | null;             // when it goes (TTL, keep or expiry running out), once a day away (or a quarter of a shorter TTL)
  reported_by: string | null;         // the project that reports a Git URL branch's pull request, when another one does (one voice per pull request)
  desired: "active" | "deleted";
  state: "queued" | "awaiting_approval" | "creating" | "ready" | "sleeping" | "updating" | "failed" | "deleting";
  error: string | null;
  url: string | null;                 // the first service's URL
  share_url: string | null;           // url with its access token (link access; plain otherwise): the one to hand out
  services: Service[]; resources: Resource[];
  cost: { sleeping_usd_month: number; active_usd_hour: number; month_to_date_usd: number; runs_usd: number; bytes: number }; // runs_usd: of month_to_date, its browsers' and jobs' containers
  schema_changes: string[] | null;    // "+ …"/"- …" against the parents, once read at `head`; null: not known
  schema_risks?: string[];            // what in them would hurt production when it lands (dropped tables and columns, type changes, NOT NULL)
  schema_overlaps?: { preview: string; branch: string; tables: string[] }[]; // other open changes whose schema changes touch the same tables
  kept_until: number | null;          // kept past its TTL until then (POST …/keep)
  pull?: { number: number; url: string; fork: boolean; approved_head: string | null; approved_by: string | null };
                                      // a Git URL service's pull request (roll apps' push hook); a fork's runs only at approved_head
  screenshot_at: number | null;       // when GET …/screenshot was taken (github.screenshot)
  last_reset_at: number | null; last_active_at: number; expires_at: number | null;
  created_by: string; created_at: number; updated_at: number;
  baseline: boolean;                  // roll keeps it to compare the project's changes with (github.baseline)
  // Checks against main's preview (types as in apps/dashboard/src/lib/previews-api.ts):
  release?: { head: string; ms: number; before_ms: number | null; locks?: { table: string; mode: string; ms: number }[] } | null;
  rehearsal?: { head: string; by: string; at: number; ok: boolean | null; ms: number | null; error: string | null } | null; // its release on today's data (POST …/rehearse)
  main_on_change?: { head: string; at: number; ms: number; pages: { service: string; path: string; status: number; main_status: number }[]; error: string | null } | null; // main's code on a copy of this head's migrated data (0: no answer)
  replay?: { head: string; by: string; at: number; ok: boolean | null; requests: { service: string; path: string; status: number; main_status: number; removed: string[]; added: string[] }[]; error: string | null } | null; // its recent GETs replayed against main's preview, at its head
  masked_by_default?: { resource: string; column: string; why: string }[]; // columns masked as they look like people's data and no rule names them
  reaches?: "open" | "roll";
  visual_change?: { since: string; percent: number; phone_percent?: number | null } | null;
  phone_screenshot?: boolean;
  base_change?: { branch: string; percent: number; phone_percent: number | null; head?: string | null; load_ms?: number | null; lcp_ms?: number | null; a11y_added?: number | null; errors_added?: number | null; text?: TextDiff | null; links_added?: number | null; seo_added?: number | null; api?: TextDiff | null; api_breaking?: string[]; api_unread?: boolean; endpoints?: EndpointTime[] /* {path, ms, status, main_ms, shape_changes?: {path, main, now: string | null}[]} */ } | null;
  pages?: PageCheck[];
  visual_review?: { state: "unchanged" | "changed" | "accepted" | "refused"; reason?: string /* refused: why */; branch: string; percent: number; by: string | null; at: number | null; required: boolean; head: string; pages: { path: string; percent: number; by: string | null; at: number | null; unstable?: boolean }[]; untaken?: number /* stories the change touched its run didn't take: one more page, `stories:untaken` */ } | null;
  page_speed?: { now: PageSpeed; before: PageSpeed | null } | null;
  page_health?: { errors: number; first_error: string | null; failed: number; first_failed: string | null } | null;
  page_a11y?: { issues: A11yIssue[]; lang: boolean; title: boolean; seo?: PageSeo | null } | null;
  page_links?: { checked: number; broken: { path: string; status: number }[] } | null;
  slower_routes?: { branch: string; routes: { name: string; count: number; p50_ms: number; main_p50_ms: number; grew?: GrewSpan | null; budget_ms?: number | null }[] } | null;
  new_errors?: { branch: string; total: number; errors: { service: string; title: string }[] } | null;
  served_secrets?: { key: string; path: string }[]; // its secret values its pages serve at its head: names and where, never values
  skipped?: { pages?: string[]; stories?: number; first_stories?: string[]; by_files?: true }; // what its latest screenshot run left out at its head: pages past a run's ten, stories it didn't take (the first 10 by id); by_files: it chose them by the change's files alone, its Storybook serving no module graph (--stats-json)
  regressions?: "check" | "required" | null;
  release_checks?: "check" | "required" | null; // the project's rule for its release at each head, where it has a Postgres branch to rehearse on
  regressions_accepted?: { head: string; by: string; at: number } | null;
  regressions_compared?: boolean;     // compared with main's preview at its head (errors; routes too with traces)
  timeline?: { head: string; started_at: number; steps: { step: "data" | "copies" | "setup" | "deploy" | "checks" | "ready"; at: number }[] };
                                      // how its head came up: each step once, when it first finished
  intercepted?: { keys: { key: string; hosts: string[] }[]; swapped: number; refused: number;
                  on_apps?: { requests: number; swapped: number; refused?: string[] } };
                                      // keys it holds as placeholders, and what egress proxies swapped or refused
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
  skip_authors: string[];             // globs on who opened it (a pull request's GitHub login: "*[bot]"): no preview unless asked ([])
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
  github: { comment: boolean; status: boolean; deployment: boolean; screenshot: boolean; baseline: boolean; pages: string[]; hide: string[]; budget: { load_ms: number; bytes: number }; openapi?: string; endpoints?: string[]; states?: {name, page, steps}[]; login?: {page, steps} /* a type step's text may be "$NAME" */; modes?: {name: string; color_scheme?: "dark" | "light"; locale?: string; timezone?: string}[]; storybook?: string /* "web" or "web:/storybook/" */; story_runs?: number /* 1 to 4 containers side by side, 40 stories each (1) */; browsers?: ("firefox" | "webkit" | "webkit-phone")[] };   // all true but screenshot; pages, hide: []; budget 0s (none); openapi: the first service's OpenAPI JSON path, compared with main's; endpoints: up to 5 GET paths timed against main's
  access: "link" | "team" | "public"; // "link": a preview's URLs need its link; "team": a person signed in to roll who may open the project
  visual_review: "off" | "check" | "required";   // the roll/visual review; required also blocks landing in roll ("off")
  feedback: boolean;                  // a feedback button on its pages for people who came in through roll (true)
  note_mail: boolean;                 // mail about notes: a new one to the change's author, a reply or resolve to the note's (true)
  flags?: {name: string; options: string[]}[]; // feature flags a reviewer sets for their own view from that button's toolbar (x-roll-flags)
  oidc_audiences?: string[];          // clouds a trusted preview's own services may have roll's OIDC token for (ROLL_OIDC_URL), at most 5
  egress: "open" | "roll";            // what its services' code may reach: anything ("open"), or only roll's products on a network with no route out (see "Egress")
  egress_hosts?: string[];            // with "roll" (or egress_forks "roll", for forks): hosts outside roll still reachable through its egress proxy (api.stripe.com, *.sentry.io; at most 20)
  egress_forks: "roll" | "open";      // what a fork's preview reaches when egress is "open" (default "roll")
  data_forks: "schema" | "same";      // a fork's preview's data: its structure without rows (default), or what the project's own get
  warm: boolean;                      // wake the change's services as soon as a new head is deployed (true)
  crons: boolean;                     // roll apps' crons and scheduled commands run on their schedule in previews (false)
  regressions: "off" | "check" | "required";   // roll/regressions against main's preview: posted (check), or a blocker (required) ("off")
  browser_checks: "report" | "check" | "required"; // production's browser checks: shown; a roll/browser-checks status; also a blocker
  mail: "catch" | "deliver";          // services' mail to roll's catcher, never delivered ("catch")
  traces: boolean;                    // roll.toml services get the OpenTelemetry SDK's variables (false)
  data_at: string | null;             // branch the data as of then: "-24h", "-30m", "-7d" or an RFC 3339 time (null: now)
  env_for: Record<string, Record<string, string>>; // branch glob → variables, over env, under a preview's own ({})
  seed: Job | null;                   // once on fresh data
  release: Job | null;                // at each new head, before services deploy (migrations)
  rehearse_when_queued?: boolean;      // when a change is queued to land, its release rehearsed on today's data by roll (false)
  release_checks?: "off" | "check" | "required"; // check: its release rehearsed on today's data at every head; required: also keeps it from landing until, at its head, that passed and main's code didn't break on its migrated data (needs release; "off")
  teardown: Job | null;               // once when the preview goes, before its services and branches
  masking: Record<string, Record<string, string>>; // "postgres:<ref>" → {"schema.table.column": "email"}, "nosql:<ref>" → {"ks.table.column": "mask_default()"}, "search:<ref>" → {"index.field": "redact" | "remove" | "email"} (redact and email only on a text-like field of the parent's: a number, a date or a vector is refused when set, use remove; an index whose name has dots, `logs.2024.user.email`, is split off at the longest name the cluster has)
};
```

## Surfaces

- **roll code change**: `Change.preview` carries the environment: its state, the first URL,
  `environment: {id, slug, state, services: [{name, url, state}], resources: [{kind, parent, ref}],
  sleeping_usd_month, active_usd_hour, dashboard_url, github}` (docs/code/http-api.md). A change whose repo is in
  a project with previews on gets its preview from here instead of code-preview's single
  container.
- **Linear issue** (Vercel's Linear integration): `PreviewEnvironment.issues` holds the Linear
  issues the change names, `ENG-123` in its title (roll's Linear agent titles its changes so) or
  at the start of its branch's last part (`ada/eng-123-fix-cart`, Linear's own branch names). The
  gallery's Linear template "A change's preview on its Linear issue" (`linear-preview-on-issue`:
  the `preview` trigger, ready and naming an issue, then `linear.attach`) links the preview there
  with how its pages compare with main's; attaching the same link again updates it. A name that
  isn't an issue of the workspace fails that run's step and nothing else.
  `a_changes_linear_issues_from_its_title_and_branch`, `linear-templates.test.ts`.
- **Jira and Asana** (Marker.io's notes to a tracker): the gallery's "Preview notes as Jira issues"
  and "Preview notes as Asana tasks" (`preview-note-to-jira`, `preview-note-to-asana`) take each
  note left on a change's preview (the `review` trigger, a body that's a note's, not a reply's;
  never one someone outside the team wrote, a share link's guest or a stranger's review on a
  public mirror, nor a dismissal)
  to the team's tracker with automations' `webhook` step: public addresses only, no redirects,
  the answer read up to 1 MiB, and the credential an automation secret (`{{ secrets.jira }}`,
  base64 of email:API token; `{{ secrets.asana }}`, a personal access token), never in the
  spec. The owner sets their site or project; the issue's summary and description are the note
  with its page and element (`automation-templates.test.ts`). A GitHub-mirrored repo's own
  GitHub Issues take notes natively (a note's work item).
- **Replay** (Signadot's SmartTests, Speedscale's replays, without capturing production): `POST
  …/replay` asks the GETs a change's preview and main's have served lately (their own request
  logs, roll's runtime's: paths only, never a query) of each again, in the background, and keeps
  how each answered: another status, or a JSON object answer whose top-level fields main's has
  and the change's hasn't, is a difference ("/api/items 200 (main 200) · lost price"). The
  Requests tab and `roll preview replay` say them. Only GETs (a write would act twice), only a
  trusted change's preview (a fork's visitors and code choose its paths, and main's preview
  answers with main's secrets), never a path that acts on a GET (signing out, unsubscribing,
  confirming), 3 minutes at most (one asked longer ago that never said how it went may be
  asked again), and only what the previews themselves served (on the preview's holder; on a
  fleet, main's preview's lines only where it's held there too): replaying
  production's traffic needs its capture's privacy designed first
  (`engine.rs::recent_requests_are_replayed_against_mains_preview`).
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
    the comment). It's read in a world of the checks' own (an isolated world: the page's DOM,
    none of its JavaScript), so the page's code can't replace what reads it; it's still the
    page's own load, so it's bounded (10 minutes, 10 GiB, and what the run writes) and only
    ever shown. An operator's own screenshot command only has to write the PNG.
    A change's screenshots are also compared with the default branch's (Chromatic's baseline):
    `base_change`, the sheet's main view (`GET …/screenshot?base=1`), "Against `main`, the page
    differs by 8.0%" in the comment. With `github.baseline` (on by default) roll keeps that preview
    itself: once the project has a change preview, it makes one of the first such repo's default
    branch (created by `roll`; one per project, since every preview runs all the project's
    services). It follows the branch, sleeps between screenshots, holds none of `max_previews`'
    slots (it still counts against `max_usd_month`, and waits once that's spent), and lasts while
    there are change previews, then its TTL. A preview of the repo's default branch someone made
    (`roll preview create --branch main`) serves instead. Deleted, it comes back at the next pass
    while the rule is on; turning the rule off deletes it. A project whose services run on roll
    apps gets none (roll can't screenshot those pages yet, so it would cost and never compare),
    and a baseline goes with its repo when the project no longer stands for it (taken off the
    canvas, or not trusted), as every preview of that repo does: it would otherwise keep running
    with the project's secrets and data. A change is compared at its screenshot;
    one taken before the baseline had any is compared once the baseline's first one comes in, and
    its comment updated. The baseline keeps its screenshots of its last 10 heads, and a change is
    compared with the one where it left the branch (its merge base, as Chromatic compares with an
    ancestor build), so main's own changes since don't count as the change's; when that head's
    are gone, with the latest (`base_change.head` says which; `?base=1` serves that one).
    The same session then takes the page at a phone's width (390 × 844 at twice the density,
    `GET …/screenshot?view=phone`, the sheet's Desktop/Phone), with its own change since the push
    before ("At a phone's width: 3.4%" in the comment), as Percy and Chromatic check widths.
    Then it takes the project's other pages (`github.pages`, up to 10 paths on the service's own
    origin, as Percy's snapshot list; `admin:/users` names another service's, taken in a run of
    its own in that service's network, as Percy snapshots each app), at the desktop width: each is kept with the one before it
    and how much it changed, compared with the baseline's page at the same path, and counts what
    went wrong while it loaded (`pages` on the preview, `GET …/screenshot?page=/pricing`, a page
    switcher on the sheet, "2 more pages, most changed /pricing by 40.0%" in the comment's
    checks). A path no longer listed is dropped at the next screenshot.
    The first page can be taken at more widths too (`github.widths`, up to 3, 320 to 2560 px; as
    Percy's widths): each is kept as a page of its own (`@768`, "768 px" on the sheet), so it's
    compared with main's at the same width, reviewed and accepted per page like the others, and
    a tablet's broken layout waits for a review. Widths share a run's five other pages with
    `github.pages` (live: `@768` taken and compared with main's).
    Before every capture the page is held still (Chromatic pauses animations, Percy takes
    `percyCSS`): animations and transitions stop, the caret is hidden, videos pause at their
    start, and the project's `github.hide` selectors (up to 20; a clock, an ad, a random avatar)
    are blanked with their box kept, so the same page screenshots the same and only real changes
    count. Speed is timed before that, on the page as it loaded.
    A project can give that page a budget (`github.budget`: a load time and bytes, as Lighthouse
    budgets do on Netlify and Vercel): a `roll/speed` commit status fails while the page is over
    it ("Loads in 2.4 s, 1.2 MB: over its budget (2.0 s load)") and passes within it, and the
    comment's checks say "over its budget". Against main too (as Vercel's Speed Insights compare
    with production): the baseline's timing at the head it was compared with rides along
    (`base_change.load_ms`), and a difference past the noise (100 ms and a tenth) is said in the
    comment's checks, the sheet and the change page ("300 ms slower than main"). Core Web
    Vitals come from the same load (Vercel's Speed Insights, Lighthouse): a `PerformanceObserver`
    put in before the page's first script records the last largest-contentful-paint candidate and
    the largest session window of layout shifts (under 1 s apart, 5 s at most, none right after
    input, as web-vitals counts them), so `page_speed` has `lcp_ms` and `cls`; main's LCP rides
    along too (`base_change.lcp_ms`, "LCP 400 ms slower than main"), and the budget takes
    `lcp_ms` and `cls` ("over its budget (0.10 CLS)"). Lab numbers, from one load in roll's
    browser: field data (real visitors) is a different measure. Accessibility
    the same way (as Netlify's checks count what a deploy adds): the baseline's report there is
    kept, and the change answers for the problems it added, per kind and a language or title it
    lost (`base_change.a11y_added`; "5 accessibility problems, 1 new against main"), and errors
    and failed requests too, past main's own (`errors_added`; "2 errors, 1 new against main").
    Where a page changed is a picture too (Percy's and Chromatic's diff view): `?diff=base` or
    `?diff=previous` (with `view` and `page` as for the screenshot) fades what's the same and marks
    what differs, the area only one of them has included; the sheet's Diff shows it against main
    (else the push before), so whoever accepts a visual change sees what they accept. Diffs are
    made on request one at a time, up to 2048 × 8192, so asking for many can't add up to
    gigabytes of decoded screenshots. A public preview's comment shows the diff against main inside
    its folded checks through a signed link of its own (`screenshot.png?diff=base&exp=…&sig=…`, the
    variant in what's signed, so a screenshot's link can't be turned into it or back).
    `roll preview screenshot <id> -o shot.png` saves any of them
    (`--page`, `--phone`, `--base`, `--previous`, `--diff base|previous`), for scripts and for the
    agents that drive the CLI; MCP's `previews_screenshot` returns them as images.
    With `visual_review` (`off` by default, `check` or `required`) a change's pages are reviewed as
    Chromatic's UI Review does: once compared with the baseline, the change has `visual_review`
    (the most any page differs, at either width), and a `roll/visual` check run that needs an
    action while a page differs and nobody has accepted it, with an Accept button on GitHub (a
    click counts for someone who can write to the repo, by GitHub's permission API, and isn't the
    change's author) ("Differs from main by 8.0%: accept it
    in roll"), and success once someone has (the sheet's Accept, `roll preview accept`, `POST
    …/visual/accept`, by a signed-in person who can write to the change's repo in roll code or an
    admin; "Accepted by ada"), or when the pages look the same. An acceptance is of one
    screenshot's head, the one the person looked at (`head`): a newer screenshot coming in
    meanwhile makes it a 409, and GitHub's button counts only on the check run of the push the
    review is about. The comment says it too ("**Visual review:** its pages differ from `main` by
    8.0%. Review it in roll", then "accepted by ada"). Pages are accepted one at a time too, as
    Chromatic reviews each snapshot (`visual_review.pages`: each page that differs and who
    accepted it; the sheet's Accept per page, `roll preview accept --page /pricing` or `--first`,
    `path` in the body); the change is accepted once every changed page is ("1 of 2 pages
    accepted, accept the rest in roll" meanwhile), and GitHub's button accepts them all. On the
    next push each accepted page that looks exactly as it did (the first page at both widths,
    each other page) keeps its acceptance, as Chromatic carries accepted baselines forward, so
    only the pages the push changed are asked about again.
    A page can be refused instead (Chromatic's Deny, Percy's Request changes): `POST
    …/visual/refuse {head?, path?, group?, reason?}` (the sheet's Refuse, asking why in one line;
    `roll preview refuse --reason …`), every page or the ones named, with why (one line, 200
    characters at most). One refused page is the change's: `visual_review.state` is `refused`
    (`by`, `at` and `reason` the latest refusal's), `roll/visual` fails ("Refused by ada: the
    Pay button moved"), the comment says so with the reason as plain text (no link, mention or
    markup gets through), and a required review keeps the change from landing. It holds at that
    head: a new push asks again, and an accept of those pages clears it, as a refusal clears
    their accept (one verdict per page). Anyone who could accept may refuse, the change's own
    author and a token too: it only holds the change back.
    Stories the change touched that its run didn't take are under review too, though no picture
    differs (Chromatic captures every story): stories run touched-first, and those left (past the
    run's 40, or out of its time) that the change touched (their own file, a file they import, or
    without a module graph one beside them) are counted (`skipped.touched_stories`,
    `visual_review.untaken`), and the review holds as one more entry, `stories:untaken`
    (`UNTAKEN_STORIES`), until someone accepts them as not taken (the sheet's "Accept as not
    taken", `path` `stories:untaken`, or accepting all). That holds at its head only: each run
    leaves its own. The status says "3 stories the change touched weren't taken: accept them as not
    taken in roll", the comment the same.
    Pages changed the same way are grouped (Percy's grouped diffs: one header change shows on
    every page, width, browser, state and story): each other page's change against the
    baseline's is the area that differs, as it was and as it is, each an 8 × 8 average hash with
    its brightness, digested to 12 hex characters (`visual::compare`, in the same pass as the
    percent). Pages under review that share one carry it (`visual_review.pages[].group`, only
    when two or more do); the sheet offers "Accept the 4 changed the same way" on the first of
    them, and `…/visual/accept {group}` (`roll preview accept --group`) accepts them in one
    transaction. The first page joins its group by its desktop picture (its signature on the
    screenshot's row).
    `the_same_change_on_two_pages_has_one_signature`, `pages_changed_the_same_way_are_accepted_together`.
    A project can set a visual threshold (`github.visual_threshold`, a percent up to 50, as
    Chromatic's `diffThreshold` and Percy's sensitivity): a page that differs from the
    baseline's by less doesn't count as changed for the review, and a push that moves a page by
    less keeps its acceptance, for what `github.hide` can't name (live: the change's 0.1%
    differences are `unchanged` under 5%). The percentages shown stay the real ones.
    Its words too (Chromatic's DOM snapshots): the same load reads the first page's visible text
    (`innerText`, at most 400 lines of 200 characters, one printable line each), kept with the
    screenshot and with main's at each head, and a comparison with main says which lines it
    gained and lost (`base_change.text`: in page order, 20 each, with counts), on the sheet, the
    change page and folded in the comment as a diff block nothing on the page can close ("text +1
    −1 against main"). It's the page's own text, shown as text (live: the change's line in,
    `main` out).
    Its endpoints too (`github.endpoints`, up to 5 GET paths of the first service's): the
    session times each one in the checks' world, the median of 10 GETs after one to warm up (at
    most 2 s a request, 20 s in all), its last status kept, on the preview's data, which is the
    parent's: a query that got slow on production-sized data shows before it lands. Each is
    compared with main's same path (`base_change.endpoints`), one slower by half again and by
    more than 20 ms called slower: in the comment's folded checks ("1 endpoint slower than
    main"; "`GET /api/users` 140 ms (main 40 ms), slower"), the sheet and the change page (live:
    `/openapi.json` timed on both).
    And their answers' shapes (Signadot's SmartTests, Optic's traffic diff): the untimed warm-up
    GET's JSON answer (up to 1 MiB) is read for its shape, one `path types` line per field
    (`users[].email string`, `total number|null`; `[]` an array's elements, `*` an object keyed by
    ids; at most 100 lines, 8 levels, 20 elements an array, 100 keys an object), never its values.
    When both answered 2xx JSON, it's compared with main's (`shape_changes`): a field main's has
    and this one doesn't (the outermost only) or a type that widened (`number` to `string`, or to
    `string|null`) breaks whoever reads it. A field it gained, or a type that narrowed, breaks
    nobody. What sits under an array that's empty on either side isn't known there, so it's never
    called gone (the two previews' data can differ). It's on the sheet's verdict strip as a danger
    ("1 endpoint's answer changed shape"), under its endpoint on the sheet and the change page, and
    in the comment ("`GET /api/users` 40 ms (main 40 ms), its answer: `users[].email` gone,
    `total` `number` → `string`"). The shapes stay with the comparison: a preview's
    `page_speed` doesn't carry them (live: the change's `/openapi.json` loses
    `paths./users/{id}`).
    Its modes too (Chromatic's Modes): with `github.modes` (up to 3, each a name and at least one
    of a `color_scheme`, `dark` or `light`; a `locale`, `de-DE`, for the page's `Intl` formatting
    and sent as `Accept-Language` (`navigator.language` stays the browser's); and a `timezone`,
    `Europe/Berlin`), the first page is taken again in each, emulated by the browser
    (`prefers-color-scheme`, the locale and the time zone), the emulation undone after (checked in
    the screenshot image: black in `mode:dark` and white at the width after it, GMT+9 then UTC, a
    German date order).
    Each is a page of its own (`mode:dark`, "Views" on the sheet, labelled "dark mode"), compared
    with main's same mode and reviewed like a page. The sheet's picture route takes every kind of
    page key a run keeps (`page=` a path, `@768`, `state:`, `browser:`, `mode:` or `story:`; it
    took only paths and widths before, so a state's, another browser's or a story's picture didn't
    load). `the_first_page_in_each_mode_is_a_page_of_its_own`.
    Its Storybook's stories too (Chromatic's stories): with `github.storybook` (`web`, or
    `web:/storybook/` when the service serves it under a path: a static build or `storybook
    dev`), that service's run reads Storybook's index (`index.json`, Storybook 7 and later;
    `stories.json` before), and takes 40 stories (with `github.story_runs`, 1 to 4 screenshot
    containers side by side, 40 each: every k-th of the order below, so each takes its share of
    the ones that matter first, merged back in that order; each billed as a browser run, their
    times added; `STORIES_SHARD`; a share whose run fails leaves the others' stories standing, and its share of the stories the change touched, what the others neither took nor left as each knows the whole touched set, counts as untaken: the visual review holds on it), those the change touched first (Chromatic's
    TurboSnap: a story whose own file the change changed since the default branch, then one that
    imports a changed file from anywhere, then one beside a file it changed, its component or its
    styles, then the rest in the index's order; a repo's path matches a story's `importPath` and a
    module's name by its end; the change's own repo's paths, `$STORIES_CHANGED`, at most 64 KiB of
    them). Who imports what comes from the bundler's module graph when the build was made with
    `--stats-json` (Storybook's `preview-stats.json`, beside its index, as TurboSnap reads it: each
    module's `name`, those it concatenates, and the `reasons` that import it), walked up from the
    changed files; the graph is the preview's code's, so it only orders the stories, and its read
    (64 MiB, 15 s) and walk (200,000 edges, 50,000 modules) are bounded. Without it, by files
    alone. `stories_the_change_touched_go_first`. Live: a built Storybook's 41 stories in a real
    browser, the last in its index reached through `preview-stats.json` taken and the 40th
    listed said as left. (docs left out; the rest, and any not
    reached in its 100 s, are said: `skipped.stories` with the first ten ids, on the sheet and in
    the comment's **Not taken**, as are pages past a run's ten) in a run of its own
    beside the screenshot's, in the service's network and locked down the same way. Each story is
    loaded alone (`iframe.html?id=…&viewMode=story`) at the desktop width, waited for until its
    root holds something (3 s at most), and captured clipped to what it rendered (an 8 px margin,
    at most 1280 by 2000; 256 KiB a PNG), 100 s in all. Each is a page of its own (`story:<id>`,
    "Stories" on the sheet, labelled `button / primary`), with what went wrong while it rendered,
    compared with main's same story and reviewed like a page. Each is checked with axe as a page
    is (Chromatic's accessibility tests: its rules, its language and title), while more than 30 s
    of the run are left so a big Storybook still gets its pictures, 8 KB of it at most; what it
    adds against main's same story counts like a page's (`a11y_added`). Live: the live
    Storybook's stories carry it. Each story's interaction test too (Chromatic's interaction
    tests): its `play` function runs as Storybook renders the story, and the run reads how it
    went from Storybook's own preview (`__STORYBOOK_PREVIEW__.storyRenders`, Storybook 7 and
    later: the story's render by id, its `phase` and whether it has a `playFunction`), waiting up
    to 5 s for it to end before the picture. It's the one read from the page's own JavaScript
    (every other check runs in an isolated world the page's code can't reach): what it answers is
    the story's word about its own test, so a forged "passed" only hides its own failure; each
    read is bounded to 2 s and taken as one of two words. `play` on the story's page is `failed`
    when it threw (or the story's render did: phase `errored`), with `play_error` what it threw as
    Storybook's frame shows it (its error display, `#error-message`, read in the isolated world: one
    line of 300 characters at most, masked like the page's other words) and `play_step` the
    interaction or assertion it stopped at (`expect.toBeVisible`, `userEvent.click`: the call
    Storybook's instrumenter marks `error`, read in the page's world like `play` and kept to a
    dotted name of 120 characters), and `passed` when a play function ran to its end;
    against main's same story, one that fails where main's passes is `against.play_broke`. The
    sheet says which. Live: the fixture's preview object has Deep's play throw and the first
    story's pass. With `github.browsers`, the change's first 5 stories (as the run
    ordered them: those it touched first) are taken in each of those browsers too, once
    Chromium's run knows which (Chromatic's cross-browser stories): `story:<id>@<browser>`
    ("button / primary · Firefox" on the sheet), each loaded alone and clipped to its root, in
    the service's network and locked down as the other browsers' run is, and compared with
    main's same story in that browser. Main's preview takes, first, the stories the project's
    open changes keep and it hasn't (in Chromium past its 40, or in another browser): a change's
    run that has one queues main's again at its head (`stories_first`), and each change is then
    compared again (a baseline on demand). A story main's Storybook doesn't have (the change added
    it) is asked for once per head, never again until main moves.
    `main_takes_the_stories_a_change_has_and_it_hasnt`.
    Each with its interaction test in that browser too (`play`, `play_error` and `play_step`, read the same
    way, through Playwright in the page's world). Best effort: Chromium's stories stand without them. Live: the live Storybook's first story
    in Firefox and in WebKit at a phone's size. A story gone from Storybook goes
    from the preview. The index is the preview's code's: an id Storybook wouldn't make
    (`components-button--primary`) is dropped. A Storybook on another service is kept with that
    service's pages while it runs what it ran (TurboSnap), and taken at once when the rule is
    turned on (checked in the screenshot image: a docs entry and a bad id skipped, a 600 by 120
    card captured as 616 by 136 with its console error counted).
  - **Snapshots from the team's own tests** (Argos's and Percy's SDKs): a Playwright or Cypress
    test sends a picture to the preview it runs against, `POST /__roll/snapshot?name=<name>` on
    the preview's own host with the project's bypass secret (`x-roll-preview-bypass`, the one CI
    already has; a public preview's too, and never a visitor's cookie) and a PNG of 2 MiB at most
    as the body (201 `{page, head}`; `&head=<changeset or git commit>`, which Playwright's run
    knows, is a 409 unless it's what the preview serves, so a test of another head isn't filed
    as this one's). It's page `snap:<name>` of the visual review at the head the preview serves
    ("checkout (your tests)" on the sheet): compared with the one it replaces at an earlier
    head, and with main's preview's `snap:<name>`, which main's own CI run sends the same way.
    The preview's owner compares them at its next converge, once for every picture sent since
    (a change's against main's, then reported; main's kept by head, then the changes waiting on
    it compared again), so a suite of 40 is one comparison, not 40. Accepted, refused and
    grouped like any page; another picture by the same name asks again (an acceptance was for
    the one before). With `github.screenshot` on only: the review starts from roll's own
    screenshot of the first page. A name is 1 to 80 of `a-z A-Z 0-9 -
    _ . /`; 40 a preview. A screenshot run at that head keeps them; the first at a new head
    drops the earlier head's, so the review only shows what this head's tests sent. A fork's
    preview has no bypass secret, so it takes none. On a service roll apps runs, its proxy
    takes the same request on the `pv-` host of a private preview (the environment holds the
    secret's hash) and hands it to roll previews (its `PreviewSnapshots` hook, roll apps #1205),
    which takes it only once that preview is known to run on that service: an environment's
    name alone proves nothing (`engine.rs::the_teams_snapshots_join_the_visual_review`,
    `scripts/preview-apps.sh`'s snapshot step):

    `@roll/previews` (`packages/previews`, no dependencies) does both for a team's tests:
    `bypass(page, baseURL)` sets the project's secret on the preview's own origin only (a header
    set for the whole browser a page could send anywhere), `snapshot(page, name)` sends the
    page's picture (whole, animations stopped) with the head the test ran against
    (`ROLL_PREVIEW_HEAD`, else a pull request event's head commit, since on `pull_request`
    workflows `GITHUB_SHA` is GitHub's merge commit and every picture would be refused, else
    `GITHUB_SHA`), and
    `sendSnapshot(baseURL, name, png)` takes a file's bytes (Cypress' `after:screenshot`); a
    refused one fails the test, saying why (`packages/previews/test/index.test.ts`):

    ```js
    // playwright.config's fixture
    import { bypass, snapshot } from "@roll/previews";
    test.beforeEach(async ({ page, baseURL }) => bypass(page, baseURL));
    test("checkout", async ({ page }) => {
      await page.goto("/checkout");
      await snapshot(page, "checkout");
    });
    // cypress: on("after:screenshot", (d) => sendSnapshot(config.baseUrl, d.name, fs.readFileSync(d.path)))
    ```
    Page states too (Chromatic's interaction tests, Percy's scripted snapshots): much of what a
    change touches shows only after a click (a cart open, a dialog, a form's errors). A project
    names up to 3 (`github.states`: a name, a page of the first service's, and 1 to 10 steps:
    `{"click": selector}`, `{"type": [selector, text]}`, `{"choose": [selector, n]}` (a list's
    nth option, from 1, up to 500), `{"press": key}` from a short list,
    `{"wait": ms}` up to 5 s), and the first service's run reaches each one after loading its page,
    in the checks' world (the page's DOM, none of its JavaScript; a value typed the way a person's
    input sets it, so a framework's change handler sees it; Enter submits a form as a person's
    does; a dialog a step opens is dismissed rather than left holding the page), 15 s at most,
    each state given 25 s more of the run. Each is a page of its own
    (`state:<name>`): screenshotted, checked, compared with main's same state and reviewed. The
    first step that finds nothing ends the steps and is said (`step`, "its steps stopped at `click
    #open: nothing matched`"), the state taken as it was then (checked by hand in the screenshot
    image: a click opening a cart, typing reaching the page's handler, Escape closing it, a missing
    element named; live: a Tab state compared with main's, and one from the repo's `roll.toml`).
    A state's clicks and typing are timed to the next paint (`step_ms`, the slowest one: its
    handlers' work and the frame after them, most of what Interaction to Next Paint counts, in the
    lab; the browser's own Event Timing counts only a person's input, and steps are the checks'),
    beside main's same state (`against.step_ms`); the sheet says so when a state's is past 200 ms
    and half again main's (Vercel's Speed Insights' INP, without visitors' own numbers yet). A
    number the page reports is kept only when it could be a time (a minute at most). Live: the
    Tab state's click timed on both. `a_screenshot_run_s_pages_are_only_those_asked_for_each_once`.
    Signed in to the app (Percy's and Chromatic's saved logins; `github.login`: a page of the
    first service's and its steps, as a state's): a page behind the app's own login would
    otherwise screenshot its login page. A `type` step's text may be `$NAME`, a variable of the
    preview's own environment (its secrets, resolved untrusted unless the project allows
    production and the change is trusted, without dynamic credentials; then the project's and
    its own variables, a `roll://` reference never as text), put in by the engine at the run,
    added to the preview's masks, and never kept. The sign-in goes to the browser as the first
    line of its container's stdin, into a file `shot.js` reads and removes: never a command line,
    an environment variable, a log or the record (`ports::Login`'s `Debug` says only its page).
    The run signs in before the first page, and its cookies stay in the browser for every page,
    state, crop and endpoint timing after it; the sign-in page's own errors count for no page.
    The activity says how it went ("screenshots: signed in through /login before its pages", or
    the step it stopped at). A fork's preview signs in with nothing (its code would get what's
    typed): its screenshots are taken signed out, and its activity says so. The other browsers'
    run signs in the same way (the same stdin line; `browsers.js` takes the steps with Playwright
    on each browser's page before its capture or crop). An agent's run (`try`) signs in first
    too, on its holder for a roll apps instance (the signed fleet hop carries the sign-in the
    preview's node decided), and its answer's `login` says how it went ("signed in through
    /login first", the step it stopped at, or "signed out: a fork's preview…"). Not yet:
    Storybook's stories (they rarely sit behind the app's login). Live: an agent's run on a page
    that answers "signed in" only once `/login.html` took the preview's own `$GREETING` (its
    cookie kept for the run's page), and the stop at a missing field in the screenshots and `try`.
    `screenshots_sign_in_first_on_a_trusted_preview`.
    Other browsers too (Percy's and Chromatic's browsers; `github.browsers`: `firefox`, `webkit`,
    and `webkit-phone`, WebKit at 390 × 844 at twice the density as a touch device, mobile
    Safari's engine; a picture past 2 MiB is left out so the others still fit):
    the screenshot image ships Playwright's builds of both, but only Playwright drives them, so
    the first time a project asks, rolld builds `roll-previews-shot:<version>` (the screenshot
    image and `playwright-core` of the same version, about 35 s, once per node). After Chromium's
    run a second one, in the service's network and locked down the same way, takes the first page
    in each, stilled as Chromium's capture is (`browsers.js`), with its errors counted; each is a
    page of its own (`browser:firefox`), compared with main's in that browser and reviewed like a
    page. The run goes alongside Chromium's (a container each in the service's network), so the
    node's one screenshot slot is held for the longer of the two, not both: measured 2026-11-15, a
    first page with five more pages and two other browsers 12–14 s instead of about 21 s
    (Chromium's alone 12–14 s, the browsers' 6–10 s); both load the service at once, as main's
    run does, so the two stay comparable. Best effort: a browser that fails leaves Chromium's screenshot as it is, and its page
    from an earlier push is dropped rather than compared and reviewed as this one's. The image is
    built `FROM` the configured screenshot image, `playwright-core` pinned and installed without
    its scripts, within 180 s; a build that fails (a node without npm's registry) isn't tried
    again for an hour, so screenshots don't each wait on it (live: Firefox's page compared with
    main's).
    A page that moves by itself (Argos's and Chromatic's flake detection): after each page's
    capture the session waits a second and captures it again, stilled as before; Chrome encodes
    the same pixels the same way, so different data is a page that changed with nobody touching
    it (a clock, a carousel, an animation `github.hide` didn't name). Its visual review's page
    says so (`unstable`), and the comment and the sheet say "moves by itself, so the difference
    may be only that", pointing at `github.hide`. The page still counts as changed: what moves
    could hide a real change (checked by hand in the screenshot image: a page with a ticking
    clock moves, a still one doesn't).
    Its API too (Optic's and Bump's API diff), when the project names its OpenAPI document
    (`github.openapi`, a JSON path of the first service's: `/openapi.json`, `/v3/api-docs`): the
    same session fetches it from the page's own origin (at most 4 MB, 5 s) in the checks' world and
    keeps every `METHOD /path` it declares (at most 1000, each `METHOD /path` or dropped), with the
    screenshot and with main's at each head; a comparison with main says which operations it
    adds and which it removes (`base_change.api`), a removed one being breaking for whoever calls
    main's; when main's document was read and the change's wasn't (gone, broken, past 4 MB or 5 s),
    that's said instead of nothing (`base_change.api_unread`). It's unfolded in the pull request comment ("Its API against `main`: removes `DELETE
    /users/{id}` (breaking for callers of `main`'s); adds `POST /orders`", each in a code span the
    document can't close) and shown on the sheet and the change page (live: the operation the
    change removes and the one it adds).
    Inside the operations both keep, what breaks their callers (oasdiff's breaking-change rules,
    `roll_previews::openapi`): the session also writes a line per fact of each operation (a
    parameter's or request body field's type, whether it's required and the values it takes,
    each 2xx response field's type; `$ref`s inside the document followed, fields to a depth of
    4, at most 5000 lines, each line's shape checked by roll), and a parameter or body field that
    becomes required (or a new required one), a type that changes, a request value no longer
    taken and a response field that goes are named, by operation (allowed values sorted and
    compared only when there are at most 200 of them) (`base_change.api_breaking`, at
    most 30): in the comment's API line, the sheet and the change page (live: a query parameter
    that becomes required). An optional new field, a new response field and fewer response
    values break nobody, so they aren't. GitHub's branch protection can require
    `roll/visual`; with `required` roll's own landing does too (a `visual` blocker in
    `Code::blockers`, from the project's rule, never the change's). A review is someone else's:
    the change's author can't accept it (403 in roll, ignored from GitHub, and an older such
    acceptance doesn't clear the blocker), as with approvals; and a required one is a person's,
    which a scoped or OAuth token (an agent's) can't give (403). Automations see it too: the `preview` trigger's
    `preview.environment.visual_review.state` field, and the built-in "Tell me when a visual change
    waits for a review" template (a notice linking to the preview, or a Slack step instead). The
    event's `became` (`ready`, `failed`, `visual_change`, `regressions`) names what an update reached that the
    preview wasn't already at, at its head, so that template, "Post a change's preview to Slack
    when it's ready" and "Tell me when a change's preview fails" fire once a
    push, as Vercel's Slack app posts once a deployment, not at every state, cost or screenshot
    change, nor when a sleeping preview wakes.
    The same session checks what a screen reader can't name (images without alt text, form
    fields without a label, buttons and links with no name) and whether the document has a
    language and a title (`page_a11y`, counts and the first one's markup, bounded; "**Accessibility:**
    3 images without alt text …" in the comment), the most common WCAG failures, as Vercel's
    toolbar and Netlify's checks show.
    With axe-core (4.14.0, vendored under `crates/roll-previews/vendor/axe-core`, MPL-2.0; what
    Lighthouse's accessibility audit runs) the session checks its WCAG rules instead: contrast,
    ARIA, landmarks, headings, duplicate ids and the rest, one issue per rule that fails (its id,
    how many elements, the first one's markup, its impact and axe's words for it; at most 50).
    It's 640 KB, so it reaches the browser container on stdin, not in an environment variable,
    and DevTools evaluates it outside the page's CSP, in the checks' isolated world (a page that
    locks a fake `window.axe` of its own changes nothing). The page's session reports all of it, so
    rule ids and impacts are checked against axe's own shapes and the words never go in a pull
    request comment ("4 elements failing `color-contrast`, serious"); the dashboard shows them as
    text. If axe doesn't load, the four checks above run as before. Against main the same way:
    rules main fails already aren't the change's.
    The same session checks the first page's own links (Netlify's link checker): every
    `<a href>` on the preview's origin, at most 50, five at a time, asked with `HEAD` only (never
    followed with a `GET`, so a `/logout` link logs nothing out; a 405 means it's there), 5 s
    each. Those that answer 4xx/5xx or nothing are `page_links` (paths only, a query can hold a
    token), against main's (`base_change.links_added`), on the sheet, the change page and in the
    comment's checks ("1 broken link, 1 new against main"). Its own requests are taken after
    what went wrong while the page loaded was copied, so they're never the page's failed
    requests (live: `/gone` 404, and the page still "0 failed").
    And its SEO basics (Lighthouse's SEO audit): a meta description, a viewport, one `h1`, and no
    `noindex` in the page's own markup (roll's header keeps previews unindexed; the app's own
    would hide production too), kept with the accessibility report (`page_a11y.seo`) and
    compared with main's by problem (`base_change.seo_added`), in roll's own words on the sheet,
    the change page and the comment's checks (live: the bare test page has none of them, nor
    main's: none new).
    The project's other pages (`github.pages`, `github.widths`) get the same checks in their own
    runs (axe-core, broken links, SEO, words; each bounded in time, the run's limit 30 s a page
    more), kept with each page shot (`pages[].a11y`, `.links`) and compared with main's page at
    the same path (`pages[].against`; main's per-head history keeps each page's report beside its
    picture), on the sheet's page switcher and per page in the comment ("1 broken link: `/gone`
    (1 new)") (live: `/pricing`, the same 404 page as main's, read and nothing new).
    On the sheet they sit under one "Checks" heading that says how many problems the first page
    has and how many are new against main ("5 problems · 2 new against main", or "nothing
    found"), each check's line under it, problems first; the change page starts its lines with
    the same count. In the Previews tab's list, a preview whose first page added problems
    against main is tagged with how many ("2 new problems"), beside "to review", so a project
    with many previews can be scanned without opening each.
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
- **Dashboard**: a Previews tab on each project, with the list (a filter once there are four or
  more: every word against title, branch, slug, repo or `#number`, and All, Needs you, Failed,
  Awake, Asleep with their counts; `lib/preview-filter.ts`), status, cost, how many errors
  its services wrote in the last hour, logs, and open, sleep, wake, reset and delete
  actions. A preview's sheet has its Overview, Activity, Logs (live while a service runs; a filter
  box, text or `/regex/`, and "Problems only", warnings and worse read as roll apps reads a line's
  level, `lib/preview-logs.ts`; "stderr" while it follows, the lines the service wrote there; errors
  in red; all in the browser on the masked lines), Shell
  (a terminal on a `roll.toml` service's shell: "A shell in a preview"; it shares roll apps'
  terminal, `components/terminal-screen.tsx`; under it, a path in the service downloaded as a tar,
  and the `roll preview cp` and `forward` lines), Requests and Errors (its services' errors grouped, newest first, each with how many times, when
  last seen, the service and its title, opening to its newest sample; the last hour, 6 or 24),
  plus a "Previews" overlay on the canvas that draws each
  preview's branches next to their parents.
- **CLI**: `roll preview ls|show|open|create|rm|sleep|wake|refresh|reset|keep|accept|screenshot|share|env|connect|mail|notes|points|logs [-f]|activity|requests|visitors|settings|usage|bypass|wait|up|down|masks|errors|metrics|traces|hooks|shell|cp|forward|fetch|try|swap`.
  - `roll preview up [--project P] [--repo owner/name] [--name N] [--no-wait] [--timeout S] [--force]`
    previews a checkout's working tree as it is, as `netlify deploy` and `vercel deploy` do from a
    directory: uncommitted and untracked files included, what `.gitignore` leaves out left out.
    - **The commit.** It's made on the side: in an index of its own (`GIT_INDEX_FILE` in a
      temporary directory) that starts as a copy of the user's (as `git stash` does, so only
      what changed is hashed again; HEAD's tree without one), then `git add -A`, `git
      write-tree` and `git commit-tree -p HEAD` (no parent without a HEAD) with the message `roll
      preview up`. The user's index, HEAD, refs and files never move.
    - **Untracked files are pushed.** Every file `.gitignore` doesn't leave out goes up, and
      anyone who can read the repo can read the branch. `up` lists the untracked files it adds
      and their total size, and refuses one over 50 MB, or one named like a secret (`.env` and
      its local variants but an example or template one, private keys and keystores, `.npmrc`,
      `.netrc`, `*.tfvars`: `named_like_a_secret`), unless `--force`. Submodules go as they're
      committed (their pointers): a submodule's own uncommitted work isn't in it.
    - **The push.** To `refs/heads/preview/<handle>/<name>` at the repo's clone URL. `<handle>`
      is the caller's (`GET /v1/access`: the signed-in person, or a person's own token), else
      `_operator` (no handle can be that: handles are `a-z0-9-`). `<name>` is `--name` or the
      checked-out branch (`local` when detached), made one ref segment (`feat/login` is
      `feat-login`). The CLI's own credential goes as the Basic password, in the push's
      environment (`GIT_CONFIG_*`: never argv, the user's config or a remote), as
      `http.<clone URL>.extraHeader`, so git sends it to that URL only. A git config that would
      rewrite the clone URL (`url.<base>.insteadOf` or `pushInsteadOf`, anywhere git reads it) is
      refused rather than pushed past. No hook runs (`core.hooksPath=/dev/null`: a hook would see
      the credential in its environment), so git-lfs's pre-push doesn't either: LFS files go as
      their pointers, and their objects aren't uploaded. Credential helpers and askpass programs
      aren't asked, redirects aren't followed, and no submodule or tag goes along. The push is
      forced and names that one ref: a run again moves the branch (roll code takes a
      non-fast-forward push to any branch but the default one).
    - **The repo** is `--repo`, else the one a remote of the checkout points at: a remote whose
      URL is that repo's `clone_url`. **The project** is `--project`, else the one project whose
      canvas has the repo.
    - **Then** it makes the branch's preview, or reuses the one it has (`create --branch`'s),
      waits until it's ready (unless `--no-wait`) and prints its URL, as `wait` does.
    - **The branch's change.** A pushed branch is a roll code change, as any is, and its checks
      run, on the uncommitted work. It gets no preview of its own: a `preview/…` branch with a
      preview of its own (`up`'s) has the one, so the rules make none for a change of it, and one
      they made just before (roll code nudges the engine on the push, before `up` asks for the
      branch's preview) is deleted. Both have to hold: a team's own `preview/new-nav` keeps its
      change's preview, and a preview made by hand of a pull request's branch never takes that pull
      request's preview, or its reports, away.
  - `roll preview down [--name N] [--repo owner/name]` deletes that preview and the
    `preview/<handle>/<name>` branch: a delete push, as `up`'s, always sent (a branch already
    gone is gone).
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
- link access, and team access (people signed in to roll, `visit.rs`);
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
- search mapping changes with the Postgres schema changes;
- NoSQL column masks per preview that hold against the preview's own queries (filters and
  indexes compare masked values; masked keys and conditions refused), a mask on a key column
  (a table's, or a view's of it) refused where it's set, naming the column, and search field masks (`search:<cluster>`, rewritten in the
  branch only, then every segment that still held a masked value expunged as the branch's own,
  since a term dictionary (`_terms_enum`, suggesters) keeps deleted documents' terms: the
  branch's term dictionary is checked empty of them live);
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
- accessibility by axe-core's WCAG rules (vendored, on stdin; the rule ids and impacts checked,
  its words kept out of comments);
- a preview opened on a phone (its link with access as a QR code, `GET …/qr.png`, on the sheet
  and the change page);
- the first page at more widths (`github.widths`, each compared and reviewed like a page);
- a preview's data from a laptop (`roll preview connect`, `GET …/connect`, the sheet's copy
  button: each branch's own credentials, never the parent's);
- previews never email real people (a mail catcher in rolld, every service's SMTP roll's, mail
  providers' keys removed, what was caught on the sheet);
- feedback resolved from the page (its Resolve opens roll, where it's resolved for the page);
- the first page's words against main's (`base_change.text`, lines gained and lost);
- broken links on the first page (its own origin's, `HEAD` only, against main's);
- data as of a point in time (`data_at`, where the product keeps history: Postgres, NoSQL, Redis);
- SEO basics on the first page (a description, a viewport, one `h1`, no `noindex` of its own);
- previews on roll apps never email real people either (the deploy's variables get roll's SMTP,
  providers' keys replaced over roll secrets');
- the first page's checks on the project's other pages, against main's same page;
- the sheet's checks under one heading that counts problems and what's new against main;
- the Previews tab tags a preview with the problems its page added against main;
- a look at the sheet from screenshots of a live preview with today's checks (desktop and phone):
  the data branch rows kept their names whole and their notes in word groups;
- caught mail bounded by bytes (10 MiB a preview) as well as count;
- page states, pages and hidden selectors in the repo's own `roll.toml` (`[[preview.states]]`,
  `pages`, `hide`; a roll apps service's `[app.previews.screenshots]`), versioned with the code
  they name, with the project's;
- the other browsers' run alongside Chromium's (a screenshot run 12–14 s instead of about 21 s),
  each run's time in rolld's log and the activity, and a node's screenshot slots from its cores
  (one every 8, 1 to 4; `ROLL_PREVIEWS_SCREENSHOT_SLOTS` to set it), the browsers image built once
  at a time whatever runs side by side, and one screenshot task per preview at a time (two
  refused each other's container name under load: main's baseline then never got one, and
  changes weren't compared with it; fixed 2026-11-16);
- dynamic credentials (roll secrets' leases) issued to each start of a service and each job,
  renewed while it runs, revoked when it stops (a sleeping preview holds none);
- with roll apps' #26 on main: Promote (a trusted change's preview's build deployed to
  production as is, through roll apps' own promote with `expect`, as the person), screenshots of a
  roll apps service in its instance's network (`Apps::reachable`), and the feedback button on its
  environment. On a fleet the instance runs on its service's holder (lease `apps:service:<id>`),
  so the preview's node sends the run there as a signed hop to `/internal/previews/apps-shot` (`require_signed`: rolld's gate lets `/internal` through without a token, so an unsigned request is refused there; only a `pv-` environment; two runs at once per node).
  This is the way roll search sends a cluster's REST requests to its owner. The holder takes it
  in its own Docker and answers the `Shot` as JSON (its PNGs base64). Fargate still has no
  container to join, and that's left to the cloud thread;
- alarms on previews' failures in the cloud stack (`preview-converge-failures`,
  `preview-screenshot-failures`: most of a quarter-hour's runs failing, never rolling a deploy back);
- idle services frozen before they stop, so a visit in the next half hour is back in tens of
  milliseconds instead of a 3–4 s cold start;
- a picture of each note's element, cropped by the next screenshot run and shown with the note;
- the sheet leads with what changed (a verdict strip: failed services, breaking API changes,
  migration risks, new problems, pages to review, slower endpoints, pages that move by
  themselves; most serious first, each jumping to its section; `lib/preview-verdict.ts`);
- the page's open notes in the pull request's comment, with a signed picture of each one's
  element for a public preview;
- egress rules: `egress: "roll"` runs a preview's services on a Docker network of their own with
  no route out (roll's products and roll's proxy still reach them);
- allowed outside hosts (`egress_hosts`) through a per-preview egress proxy, what it refused said
  in the activity;
- a note's picture beside it on the change page in roll too (its review in the timeline);
- only the pages a push can change are taken again (Chromatic's TurboSnap): another service that
  runs what it ran at the push before (its revision, what it's deployed with, the same data)
  keeps its pages, checks and all, without a browser run;
- search masks on an index whose name has dots (`logs.2024`): split off at the longest index name
  the cluster has, for masking and for the check when set;
- a tenth review (states, browsers, search masks) and its fixes: a dialog a state's step opens
  dismissed (it held the whole screenshot), Enter submitting a form, an earlier push's browser
  page dropped when the browsers run fails, the image build bounded in time and built from the
  configured image without install scripts, WebKit at a phone at twice the density, room for 10
  pages a run and time for each state's steps, states re-checked before a run, the search mask
  check through aliases and with the right string types, the secrets retry on the exact error;
- the first page in Firefox and WebKit (`github.browsers`), each compared with main's in that
  browser, through a screenshot image with `playwright-core` built once per node;
- page states reached by a few steps (Chromatic's interaction tests): `github.states`, each
  taken after its steps, compared with main's and reviewed like a page, a step that finds nothing
  said;
- search masks checked when set against the parent's mapping: `redact` or `email` on a field that
  isn't text (a vector from semantic search, a number, a date) is refused with what it is, where it
  used to fail the masking and the preview; checked live on a number field;
- a ninth review (of the work since the eighth) and its fixes: migration risks read from every
  line, not the 200 shown, `NOT NULL` read before a default, view columns apart; each API line
  bounded in the session so a huge document can't blank the page's report; a change whose API
  document can't be read said so; allowed values sorted and compared whole; a failed job's output
  in a fence it can't close; on the stack, Promote only for a trusted change's preview and only
  for the build the sheet shows;
- endpoint times against main (`github.endpoints`: the median of 10 GETs on the preview's data,
  a clearly slower one said so in the comment, the sheet and the change page);
- pages that move by themselves told apart (Argos's and Chromatic's flake detection): each
  page captured twice a second apart in its session, and one whose captures differ said to
  "move by itself" in its visual review, the comment and the sheet (its change still reviewed);
- breaking changes inside the operations a change keeps (oasdiff's rules): a parameter or field
  that becomes required, a type that changes, a request value no longer taken, a response field
  that goes;
- the API against main (Optic's and Bump's API diff): `github.openapi`'s operations compared with
  main's, a removed one named as breaking, unfolded in the pull request comment;
- migration risks named over the schema changes (a dropped table or column, a type change, a
  column made NOT NULL), on the sheet and unfolded in the pull request comment;
- replies on a page's notes (Vercel Comments' threads): comment reviews naming the note, listed
  under it on the page, in the sheet (with a reply box) and by `roll preview notes`;
- a teardown's failure told to the change: the `preview` event's `teardown`, and the pull
  request comment's "deleted. Its teardown failed:" with the job's output fenced;
- the page's checks (speed, Core Web Vitals, accessibility, SEO, links, words) in a world of
  their own, an isolated world with the page's DOM but none of its JavaScript: a page that
  replaced `JSON.stringify` and locked a fake `window.axe` reported a 1 ms load, its own words
  and no problems before, and its real load, text and missing `alt` after;
- an eighth review (of this branch, #28) and its fixes: search masks through arrays of objects,
  literal dotted keys and unindexed fields, failing on a version conflict; `connect` only for a
  branch masked with today's rules; a screenshot run's output bounded (a page's code could have
  made rolld read gigabytes); the mail catcher's slots time-boxed (15 s to sign in, 5 minutes a
  connection, 8 per address); no control characters in a caught mail's addresses or in what
  `roll preview mail` prints; an author's own token can't accept their visual change; an ANN
  search can't be ordered by a masked vector; the diff link's pictures kept, not redrawn per
  fetch;
- a `teardown` job when a preview goes (Heroku's `pr-predestroy`): once, before its services
  and branches, only at a head it ran (a fork's unapproved push never), checked live (it saw
  the branch's URL, its failure didn't keep the preview);
- the sheet's caught mail and open notes from a terminal (`roll preview mail`, `--read`;
  `roll preview notes`, `--resolve`), checked live;
- the stacked `feature/roll-previews-on-apps` refreshed against roll apps' `-next`: it found
  apps' forwarded route turning every unrouted path's 404 into a 401 (previews read secrets'
  404 as "off", so every converge failed), fixed on `feature/roll-apps-next` (9911f6e9d, with a
  test);
- a light previews list (each page's checks only in a preview's own view, which the sheet reads);
- a phone-width screenshot with its own visual change;
- every change compared with the default branch, whose preview roll keeps itself (`github.baseline`);
- the comment's page checks folded under one line of counts;
- screenshots of the project's other pages (`github.pages`), each compared like the first, other
  services' included (`admin:/users`);
- a visual review per push (`visual_review`: the `roll/visual` status, Accept in roll, and
  `required` keeping the change from landing in roll);
- what each data branch hands the preview's code (`resources[].data`: `none`, `masked` or `real`),
  said on the sheet and the change page ("production data, unmasked") and, with the way out,
  in the settings (masks, Schema only, a seed, Skip);
- a visual review per page (Chromatic's per-snapshot review): each changed page accepted on its
  own, the change once all are, and a push asking again only about the pages it changed (live:
  "changed, 1 of 2", then accepted);
- comparisons with main at the change's merge base (the baseline's last 10 heads kept);
- stable screenshots: animations stopped and `github.hide` selectors blanked before each capture;
- where a page changed, as a diff image (`?diff=base|previous`, the sheet's Diff);
- a speed budget per project (`github.budget`, the `roll/speed` status);
- Core Web Vitals per push (LCP and CLS from the screenshot's load, LCP against main's, both in
  the budget);
- a visual threshold per project (`github.visual_threshold`): a page that moved less isn't
  reviewed again;
- previews kept out of search engines (`X-Robots-Tag: noindex` on every response through a
  preview's host);
- how long the release (migrations) took on the preview's data, per push, against the push
  before;
- screenshot comparisons off the async runtime, and a test for reading a run's pages;
- the previews waiting for a visual review first in the list, tagged "to review";
- `roll preview screenshot` (any screenshot or diff to a file);
- on roll apps: one deployment per change of what's deployed (an in-flight poll redeployed each
  time: 26 deploys of a preview's environment to 3, ready 63 s to 19 s), and the feedback
  button and screenshots on the stacked `feature/roll-previews-on-apps`;
- the settings' screenshot rules in a section of their own ("Screenshots and checks");
- a second service in the live check (`wiki:/` taken in its own run and compared);
- the diff against main in a public preview's comment (a signed link of its own), and the
  visual review's state in the comment, with the way to review it;
- Accept on GitHub: `roll/visual` a check run with a button, for writers who aren't the author;
- an acceptance of what was seen only (the head looked at; an older push's button ignored);
- from a look at a live preview: an Accept button on the change page's "Before it can land"
  visual blocker (the list's "to review" and baseline tags read as meant);
- a preview's view reads its screenshot row once and never its images (50 previews' views: 300 ms
  to 25 ms on SQLite; about half the queries, which is what counts on the fleet's Postgres);
- metrics: converges, screenshots and their failures as EMF lines (`PreviewConvergeFailures`,
  `PreviewScreenshotFailures`; docs/cloud/README.md);
- visual reviews in automations (the `preview` trigger's field, a built-in template);
- the preview on the change's Linear issue (`issues`, the Linear template);
- an acceptance carried over a push that changes nothing visible;
- speed against main ("300 ms slower than main") accessibility problems and errors new against it;
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

Ranked again after an eleventh product review (2026-11-26, night), against Tonic and Neosync's
masking, pgroll and Reshape's expand/contract, PlanetScale's safe migrations, Vercel's bypass,
Argos and Percy's SDKs, Marker.io, Signadot. Its fixes landed with it: a rehearsal swaps the
variables roll makes from a branch's URL too (it could run on the preview's own branch), the
change becoming queued wakes its settled preview, a rehearsal shows only at its head, and schema
overlaps stay within a repo.

Ranked again on 2026-11-27, late evening, after a fourteenth product review (against Vercel and
Netlify's toolbar, comments and protection, Render, Railway and Heroku review apps, Neon and
PlanetScale, Signadot and Speedscale, Chromatic, Percy and Argos, Jam and Marker.io). Its first
fix landed with it: `@roll/previews`' snapshots on `pull_request` workflows carry the pull
request's head, not GitHub's merge commit (#1252).

In progress, each in a pull request of its own:
- a leaked CI bypass secret (`rpb_`) revoked, as deploy hooks are (the review's one high finding);
- team access on a project's own preview domain (the sign-in's ticket refused its host);
- a snapshot's head checked on roll apps too (an older push's picture could stand for the head's);
- what a note's reviewer saw, from their own browser (Jam, Marker.io);
- writes replayed safely on fresh copies of both previews' data (Signadot, Speedscale), which
  also takes GET replay off main's preview's own data (a replayed GET can act);
- a member's preview of a project with a search cluster, checked end to end.

1. **A note resolved when its fix lands** (Marker.io's and Jam's two-way sync): a note's work
   item or GitHub issue closing resolves it, so the pull request comment stops listing fixed
   notes; Jira and Asana through an automation trigger after.
2. **The page's own review in the toolbar** (Vercel's toolbar, Chromatic's UI Review): a team
   visitor sees whether this page changed against main and what it adds, with Accept opening
   roll (never clickable by the page's own script).
3. **A live dashboard, in review**: the Previews list follows the event feed (a long poll, `wait`)
   instead of reading the list every 20 s; a calm list is read once a minute in case the feed
   missed something, a busy one every 3 s as before. A screenshot that arrives after a preview is
   up without a visual change still shows at the next read.
4. **roll's reviewer reads the visual and API results** (Percy's Visual Review Agent): which of a
   change's visual changes its diff explains, as the verdict's first line; model output never
   accepts anything.
5. **An earlier push started from the sheet, the CLI and MCP**, not only from its own 410 page.
6. **Restore points for search and buckets**: search's on roll search's reset from a copy (#1149;
   200 put back, 409 dropped or busy, 503 recorded); buckets need a put-back on roll storage's
   side.
7. **Previews for the members thread's next products**: search is a member product (#808).
   Checked on 2026-11-27: a member's previews of a project with a cluster keep their role (the
   data sheet is `reveal`, and a search branch answers 409 there; mask suggestions read only
   clusters the project trusts, which every member role reads; the columns are read as the
   operator behind that filter). Teardown is previews' own: the operator changes the password,
   then deletes. A branch that fails, a 503 included, fails the converge with the product's
   error and is tried again on the backoff. By design, the preview's branch is in no project:
   roll search homes only a person's branches (`Control::branched`), so through `/v1/search` it's
   a 404 to every member, and members reach it through previews' own routes, by role (its
   credentials are `reveal`). Homing it would take two changes outside previews:
   - roll-access finding a cluster's project from its home, not only from canvases (previews'
     copies can't go on the canvas, or the next preview would branch them);
   - search's project-delete hooks skipping previews' branches (previews deletes them only after
     the project is gone).

   That isn't worth it while previews' own routes cover what members need.

   roll apps follows once its routes become member routes.
8. **Watches on pg-control**: the data step's Postgres compute start; cheap branches on ext4
   through overlay volumes; recovery to a time seconds old (unowned).

Not to build: more lab checks on the first page (each adds noise to every pull request), or a
Kubernetes cluster per preview (roll apps' Compose import covers multi-service apps, and a
cluster would break "costs nothing asleep").

Landed 2026-11-27, late evening: the Next list ranked again (#1250), and snapshots' head on `pull_request` workflows (#1252).

Landed 2026-11-27, evening: the thirteenth review's fixes (#1237), `@roll/previews` (#1240), older pushes started on demand (#1232), and lean tests with SQLite's allocations no longer counted under one process-wide lock (#1248: 188 Rust tests to 87, the engine suite 33 s to 4 s).

Landed 2026-11-27, early: a failed story shard's share of the touched stories counted as untaken (#1175), and masking that's safe by default for NoSQL and search, with the project told of each column masked by default (#1177), and main's code on a copy of a change's migrated data, on roll apps (#1183) and roll's runtime (#1185), webhook URLs into private previews (#1187), the twelfth review's fixes (#1200), snapshots from the team's own tests (#1192), a note's work item made by roll (#1196), a team note opening a GitHub issue on a mirrored repo (#1209), and snapshots, notes and share links on roll apps services checked by service (#1212), release_checks (#1204), and preview notes to Jira and Asana (#1220).

Landed 2026-11-26, midday: restore points for streams (#1112), the TurboSnap test's race under
load (#1114), the tenth review's fixes (#1119), the visual review holding on stories a change
touched that its run didn't take (#1120), a visual change refused (#1125), masked bases (#1130), more stories per run (#1135), open
changes that change the same tables named on each other (#1136), the toolbar's Main (#1138), a release
rehearsed on today's data (#1140), and roll apps previews' feedback button for team access
(#1151), a rehearsal when a change is queued to land (#1155), the eleventh review's fixes (#1160), masking that's safe by default (#1166), and the locks a
release holds, measured (#1169).

Landed 2026-11-26, morning: stories' interaction tests (#1087), what a failed one threw (#1091), both in the other browsers (#1100), the step a failed one stopped at (#1105), and restore points for Redis (#1109).

Landed 2026-11-26, early: a baseline on demand for stories (#1082); a change's first stories in the other browsers (#1064); roll apps lookups that miss remembered (#1066); a settled running preview's usage read alone too (#1069).

Landed 2026-11-25, night: a note's reply and resolve in roll's small window (#1052); branches' handed-out credentials sealed at rest (#1058).

Landed 2026-11-25, afternoon and evening:
- browser runs and jobs in the cost (#1010), and roll apps services' own bill, builds included, with
  the sign-in proven live behind a login (#1011);
- stories by the module graph, and the first live Storybook run (#1014);
- a note in one click (#1016);
- sleeping previews' usage read alone between hourly converges (#1017);
- a fork's data its structure only (#1020: `data_forks`);
- a capped preview's roll apps services paused there (#1027);
- NoSQL restore points (#1028);
- every story checked with axe, and a run chosen by files alone said (#1031; the stories run had
  never had axe-core);
- a fork's pages on roll apps held to its own hosts, checked live (#1034, roll apps' #1032), with
  `semantic_text` masking on a search branch (roll search's #988, checked live in #1047).

Landed 2026-11-25, noon: the stories a change touched taken first (#1005).

Landed 2026-11-25, late morning: what a screenshot run left out, said (#997: `skipped` on the record, the sheet and the comment); a branch's `PGOPTIONS` and `PGSSLMODE` (#999, found by roll apps' review).

Landed 2026-11-25, morning: screenshots signed in to the app (#980: `github.login`, `$NAME` from
the preview's environment, trusted previews only), in every engine (#986), and agents' runs
(#991).

Landed 2026-11-25, night: the fifth review's security fixes (#943); roll apps on par (#947: one
feedback button); `note_mail` in the page and `roll preview try` (#948); share links on the
previews roll apps serves (#952); a note's typing (#953) and choices (#955: a `choose` step) taken
again for its crop; a fork's preview's pages kept to its own hosts (#963: a Content-Security-Policy);
a preview over its `max_bytes` stays asleep (#964); GitLab and Bitbucket merge requests hear back
(#967).

Landed 2026-11-24, night: a note's element cropped in its reviewer's engine (#938: WebKit for Safari and iOS, Firefox; their viewport, after their clicks).

Landed 2026-11-24, evening, later: a note's element cropped as its reviewer saw it (#932: their viewport, after their clicks).

Landed 2026-11-24, evening: the first page in its visual group (#926), and a note's people told by mail (#929).

Landed 2026-11-24, afternoon, later: guests' notes outside the team's for MCP's lockdown (#916), and pages changed the same way grouped and accepted together (#919; the first page in them with #926).

Landed 2026-11-24, afternoon: a page state's steps timed to the next paint (#915, INP in the lab, against main's).

Landed 2026-11-24, early afternoon: notes from people outside the team (#911: a share link's guests, `guest:<name>`).

Landed 2026-11-24, midday: notes as Slack threads both ways (#904, `slack.note`) and a resolve said on the change (#906).

Landed 2026-11-24, morning, latest: restore points (#897: a preview's Postgres data saved and put back in place, pg-control's reset `from`; 12.8 s asked to ready again).

Landed 2026-11-24, morning, later still: agents drive a preview's page (#885: `POST …/try`, steps in a browser, its text and picture; #893: roll apps services' too, on the node that runs the instance).

Landed 2026-11-24, morning, later: a note says what the reviewer did before it, and the note's form works again after #861 (#879, the toolbar now driven in a browser live).

Landed 2026-11-24, morning: screenshot modes (#877: dark, a language, a time zone), and the sheet's pictures of every page kind.

Landed 2026-11-24, early morning: OIDC tokens for a preview's own services (#865, roll secrets' `Subject::Preview`), a roll apps service's page read through roll (#870), a fork's preview and its code's words marked outside for roll MCP's lockdown (#873).

Landed 2026-11-24, later: feature flags a reviewer sets for their own view (#861: the toolbar's Flags, `x-roll-flags`).

Landed 2026-11-24: a data branch looked at from the sheet (#858: its tables and first rows, as the branch's own role; live, `public.users` masked).

Landed 2026-11-23, after midnight: a page read through roll for agents (#852: `GET …/fetch`, `roll preview fetch`; roll MCP maps it as `previews_fetch`).

Landed 2026-11-23, at midnight: a note or an error made a work item (#851), and a user name isn't a served secret.

Landed 2026-11-23, near midnight: secret values in what a preview serves (#846: named with where, never the value; live, `DATABASE_URL` in `/env.txt`).

Landed 2026-11-23, late night: a service swapped for a process on a laptop (#842: `roll preview swap`, checked live through the preview's host and back).

Landed 2026-11-23, night: Storybook stories as snapshots (#837: each story a page, compared with main's and reviewed).

Landed 2026-11-23, evening, later: endpoint answers compared with main's by shape (#834: a field gone or a type widened; live, `/openapi.json` loses `paths./users/{id}`).

Landed 2026-11-23, evening: feedback notes that carry the page's uncaught errors, failed requests and browser (#823, checked in headless Chromium).

Landed 2026-11-23, late afternoon: skew protection for `roll.toml` services (#822: a page loaded before a push keeps reaching that push; checked live).

Landed 2026-11-23, mid-afternoon: a failed browser check's screenshot and trace from the sheet (#815: the record names where roll apps keeps them).

Landed 2026-11-23, afternoon: production's browser checks checked live in `preview-apps.sh` (#812: one passed, one failed with its error).

Landed 2026-11-23, early afternoon: browser checks in the event feed (#807: `browser_checks_passed`, `browser_checks_failed`, once a head).

Landed 2026-11-23, midday: the project's `browser_checks` rule (#802: `check` posts `roll/browser-checks`, `required` also a roll blocker), and a preview's tasks tenant giving its services its own webhook signing keys (#801, for roll apps' #792).

Landed 2026-11-23, late morning: production's browser checks on each preview head, on the record, the sheet, `roll preview show` and the PR comment (#791, on roll apps' #790).

Landed 2026-11-23, mid-morning: an evicted push's copy that can't be deleted doesn't hold the push up (#788).

Landed 2026-11-23 morning, later: a copy that fails doesn't hold the push up (#783), and `preview-intercept.sh`'s echo step allowing the cut that follows roll secrets' #705.

Landed 2026-11-23 morning: what the earlier pushes' copies hold, said apart on the sheet and in `roll preview show` (#776, found in the 2026-11-23 review pack).

Landed 2026-11-23 night: the copies as a timeline step of their own (#770: live, data 2 ms · copies 2.6 s · setup 1.8 s).

Landed 2026-11-22 evening to 2026-11-23: earlier pushes on their own data, for every product a preview branches and for roll apps services (#726 Postgres, #731 their cost, #740 Redis and search, #746 shown on every surface, #752 buckets, streams and NoSQL, #756 roll apps on roll apps' #750), the copies taken all at once (this PR).

Landed 2026-11-22, late afternoon: a preview event feed (#713, roll MCP's ask: `GET /v1/previews/events`, created, ready, failed, visual_change and deleted, each seq taken in the write that made it so; checked live by `scripts/preview-cost.sh`), which roll MCP's Events follow.

Landed 2026-11-22, afternoon: a preview's interception CA kept until its hosts change or it nears its end (#686, on roll secrets' #705: no more hourly restarts of an intercepted preview's services), and a roll apps service's earlier pushes at each build's own deployment URL (#709, on roll apps' #711: they had been `roll.toml` services' only).

Landed 2026-11-22, late morning: a search branch's inherited connector checked live, its sync waiting for adoption while the parent's runs (#690, roll search's #318), and an index production makes after the branch (a connector's first sync, a rollover) no longer read as a drop; schema changes that name only what the change made (#695: each branch's own schema kept as it's made, so another change's migration landing on production first isn't read as this one dropping its table); and a search branch masked again in place keeping the schema it was made with, #702).

Landed 2026-11-22, mid-morning: a share link works only while its maker may still open the project's previews (#679: someone who left the project no longer keeps a working link), and the live run checks that another consumer's placeholder is refused on a preview's socket (#685, roll secrets' #665).

Landed 2026-11-22, later that morning: roll secrets' response scrub wired into `roll-intercept` (#630: an echoed key stopped, its placeholder ended; and the `maker_unreachable` fallback checked live), a preview's crons and scheduled commands off unless the project says so (#656, on roll apps' `scheduled`), the API contract's `Preview` and `Settings` complete again with a test that keeps them so (#656: 23 fields had fallen out), and a leaf's private key zeroized in memory with `preview-apps.sh` no longer leaking Docker networks (#671: 23 had used up the address pools).

Landed 2026-11-22 morning: a preview's bucket key checked live against production's bucket (#643), and `roll preview show`/`wait` printing the timeline (#651).

Landed 2026-11-22: how each head came up, step by step (#641: the preview's timeline on its record, sheet and every live run), and a preview waiting on a starting service going on as soon as it's up (#646: the health checks went from 20.2 s to 6.1 s in the live run; 13 s had been a poll's wait).

Checked 2026-11-22: every credential a preview's code holds is its branch's own. The live run now proves it for the bucket (the preview's key lists its branch and is refused on production's bucket), beside search's data-only key (#607); Postgres gets a role of its own, Redis, NoSQL and queues their branch's own login, streams and tasks a key of their own.

Dropped 2026-11-21: reusing main's build for a service a change's first deploy doesn't touch. A cached rebuild of an untouched service costs a few seconds of CPU and runs at once with the changed service's build, which sets the wall time (two roll apps services, a push with cached builds: live in 4.2 s, see step 6 above), so a person would see no difference.

Landed 2026-11-21 night: previews on members (#524, after members v1 #501): a member's role decides each previews route by roll-access's catalogue, team access by role, a create only in the project roll-access decided it on.

Landed 2026-11-21 evening: keys on roll apps used as the rules' writer (#608, after apps' #610,
#612 and #617), and the sheet looked at with real data at desktop and phone widths, dark and
light: on a phone the sheet takes the width (a sliver of the list showed beside it), earlier
pushes' links truncate instead of running off it, a wrapped line of details never starts with
"·", the project's footer stays one line, and the screenshot's choices are grouped (pages, views,
states).

Landed 2026-11-21 midday: a search branch's link holds only its data-only credential (#607,
roll search's report: never the branch's own login; old links branched again; its data key revoked
and its password changed before a delete, since roll search's deletes are restorable), the keys a
preview never holds on its record and sheet (#599), roll apps' closed environments on
`roll-intercept` (apps' #603), roll apps' interception counts for a preview's services there on
its record and sheet (#613), and an erasing write's checkpoint tried again until the WAL is empty
(#615: a busy checkpoint had left what #581 rewrote sealed in the WAL).

Landed 2026-11-21 morning: credentials sealed at rest (#581: the rules' pasted values, a
preview's own variables and the screenshot links' key, refused in the clear without a key, an
upgraded database left with nothing in its file or WAL), keys a fork's preview never holds (#535:
placeholders swapped by its egress proxy over roll secrets' batch 6, the intercepting step its own
crate `roll-intercept` shared with roll apps, live-checked by `scripts/preview-intercept.sh`), and
a key bound to a host its egress doesn't allow said in the activity (#593).

Landed 2026-11-21 (main c4b6561c9; turn 15): the gone page tested at the edge (#559), the live
check's leaked build networks fixed (#567), `@roll preview` in any of a change's threads (#568)
and watch paths per service (#570: a push outside them keeps the service's build).

Landed 2026-11-21 (main e947ac207, ca30f77f1; turn 14): an independent review's fixes to
references (#557: a Git URL service's preview read as its service, starts waiting for the
converge that leaves out a reference whose writer lost it, a deleted preview's rows) and
`@roll preview redeploy|reset|wake|sleep|keep` from a pull request (#558).

Landed 2026-11-20 night (main b820c8883, c80a3fb99): a link to an earlier push no longer kept
answers 410 naming the push and linking the service's latest (#545), on a project's own preview
domain too (#553).

Landed 2026-11-20 evening, later (main 089b7fa80): a preview's own variables take `roll://`
references, each read as its setter at every converge (#544).

Landed 2026-11-20 evening (main ed1868373, gated green with both live checks): a rule's reference
to another environment reaches a preview only while its writer still may (#537: roll secrets'
`digest_as` as them at every converge; left out otherwise, said in the activity).

Landed in previews' lock turn 13 of 2026-11-20 (main c256b2f0d, exactly its gated tree): route
budgets (#512: a route held to a p50 of its own, failing `roll/regressions` and blocking without
main's preview, accepted with its budget named) and the author told apart by every name they go
by (#507: their roll handle and each linked GitHub login, refreshed when GitHub renames it). An
independent review of both found 8 things, all fixed before they landed; the live check found
the operator's token refused for a required visual review (#507's own fix) and was changed to
accept as a check.

Landed in previews' lock turn 10 of 2026-11-19 (main 020da3033, exactly its gated tree): the
routes slower than main's preview in the verdict strip, the change page and the comment (#431),
new errors against main's preview (#438), repeated queries in traces (#441), an error group
opening its request's trace (#433), `roll://` references in the preview rules (#436, reviewed by
roll secrets) and a preview's own variables sealed at rest (#452). An independent review of
them found 13 things, all fixed before they landed or written down as limits; a look at them on
real data found the route table cutting names to two characters (fixed on #441).

Landed in previews' lock turn 9 of 2026-11-19 (main 84d246b60, exactly its gated tree): turns 7
and 8's review fixes (span bytes bounded, traces off for roll apps, the intake's token first,
stderr's event order, masks from the store; #419), each route's time from its traces against
main's (#424), and `roll preview up` putting back the remote-tracking refs a push made (#427).

Landed in previews' lock turns 7 and 8 of 2026-11-19 (main 490b68b75 and 7b4063ef2, each exactly
its gated tree; turn 7 landed ahead of roll apps' chain4 with apps' and MCP's agreement, turn 8
rebuilt on the post-chain4 main and gated again):
- turn 6's review fixes: drains compare stamps per stream, forward relays swept, the forward
  WebSocket capped at 1 MiB a message and ended with its preview, the sheet's roll apps shell only
  for a service with the preview's environment (#386);
- a session's WebSocket opens only from the dashboard's origins, in rolld's gate, for every
  product's upgrades (#388, agreed with apps; reviewed by MCP);
- stderr apart in a followed log: `event: stderr`, the sheet's filter, the CLI's stderr (#390);
- files and ports from the sheet (#400);
- traces of a preview's `roll.toml` services, and its roll apps services' on the same list (#403,
  #404); MCP maps them as `previews_traces` and `previews_get_trace`;
- holder routing and the visibility check read a preview's id percent-decoded, as its handler does
  (#405, found by MCP in roll apps).

Landed in previews' lock turn 6 of 2026-11-19 (main 741f8b789, exactly the gated tree; built and
gated before the lock came, rebuilt on the new main when it did, the stack's own diff checked line
for line against the green tree, gated again, landed one at a time, stacked PRs retargeted to
main before the queue):
- a preview's `roll.toml` services and jobs in the project's log drains (#363);
- a shell in a preview's service: the API, `roll preview shell` and the sheet's Shell tab, a roll
  apps service's being roll apps' own (#364);
- search and levels in the sheet's logs (#369);
- files in and out of a preview's service, `roll preview cp` (#372);
- a port of a preview's service on a laptop, `roll preview forward` (#378);
- a cold start that meets a container name still being removed tries again (#374).
Looking at the real sheet (screenshots of a live preview) found three bugs before review: a
terminal that remounted closed its own shell, closed tabs held shells open for ten minutes, and a
short command's last output raced its end.

Landed in previews' lock turn of 2026-11-19 (main 5eb41bf5f, exactly the gated tree: one
combined tree, each step tagged, gated once with both live checks, landing simulated with
`git merge-tree` before the chain, then landed one at a time, each identical to its step):
- team access, people signed in to roll (#285), its feedback button (#300) and who opened it
  (#335);
- `roll preview up` and `down`, a preview of the working tree (#314);
- mask suggestions, every one applying on real Postgres (#324);
- a preview's errors grouped on its sheet (#326);
- `roll preview metrics` and `hooks` (#344);
- `skip_authors` on a Git URL service's pull requests (#305) and a bot's branch push by its
  pusher (#343), after apps' #282 and #317;
- a project's own preview domain wired to roll apps' ingress (#341), after apps' #278;
- the Next list (#296).
The gate's live script caught one thing no suite did: #285's review fix retires a project's
links when it leaves `link` access, and `preview-cost.sh` still used the old cookie (fixed on
#285).

Landed in previews' lock turn of 2026-11-18 (main 440e13ebf; one combined tree, each step
tagged and gated once with both live checks, then landed one at a time, each identical to its
step):
- a Git URL preview's `roll/preview` and pull request comment through roll's GitHub App (#232);
- one voice per pull request: a Git URL service on two projects' canvases is reported by the
  project that has had it longest, one comment across a handover, the other `quiet` (#246);
- a page's words masked whole before they're cut (#233);
- what a preview's services use, CPU and memory (#245);
- a preview made by hand only of a repo the project stands for, never another project's (#259);
- `skip_authors` for dependency bots' pull requests (#266);
- deploy hooks (`rlb_`), a secret URL a headless CMS calls to rebuild a branch's previews (#272);
- a preview's services deploy at once: a cached push live again in 4.2 s against 5.8 s (#280);
- Roll only on roll apps' builds, `closed: true` on each deploy (#234), and `report: false` on
  every `pv-` environment (#251);
- previews' half of a project's own preview domain (#275).

Landed in previews' lock turn of 2026-11-17 (main 42a3bbe1b, one combined gate with both live
checks):
- what a preview shows, masked: logs, job output, a start failure's output tail, page words,
  health answers, refused hosts, request paths (#200, #203, #215);
- roll apps variables sealed;
- forks reach roll only by default, jobs included, with DNS forwarding nowhere (#206);
- Roll only on roll apps' instances (#216, which also gets roll apps' refusals into the
  activity);
- concurrent branching and health checks (#196, #204: change → ready 11–14 s, from 15–19 s);
- `became` ready and failed once a push, with Slack and failure templates (#197);
- fleet screenshots on the service's holder, signed hops only (#214);
- the request log (#215), and Linear, search keys, note pictures, the freeze window and the list
  filter (#118, #153, #111, #174, #192).

Looked at and not planned: sharing the default branch's running services with a preview
(Signadot's sandboxes). A shared service writes to the baseline's data, not the preview's
branches, so it's only right for a stateless service.

Looked at and kept: previews' copy of a preview's variables in its roll apps environment, next
to the `pv-` secrets environment roll apps reads itself (#31's `ForApps`). roll apps reads
secrets only for a project `Workspace::trusts` (the operator's, an admin's); for anyone else's
project the copy is the preview's only source, and the two hold the same values.

Looked at and dropped: branching from a stopped parent without starting it. pg-control already
copies a stopped parent's directory directly; the slow first numbers (9.6–16.8 s) were brand-new
parents being initialized on their first start, and a preview of an existing project is ready in
about 5 s (branch compute 1.3 s, its history's base backup 1.8 s, the other products about 1 s).
