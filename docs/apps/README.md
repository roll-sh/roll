# roll apps: run your own code

roll apps hosts the user's own code next to their roll databases: a **service** in a workspace
project, built from a roll code repo (deploys on push), a GitHub repo mirrored into roll code, a
Dockerfile, a public image, or an auto-detected build. HTTP services **scale to zero, wake on the
first request, and cost nothing while idle**. Workers stay up, cron jobs run on a schedule, and
every deployment is immutable with instant rollback.

- Code: `crates/apps-build` (source detection and build plans; no I/O but reading the tree),
  `crates/apps-control` (the store, deploy queue, runtimes, the waking proxy, logs, metrics,
  metering, `/v1/apps`), mounted by `crates/rolld/src/apps.rs`.
- Contract: [http-api.md](http-api.md), with `apps/dashboard/src/lib/apps-api.ts`. Change both
  together.
- CLI: `roll up`, `roll deploy`, `roll logs`, `roll domains`, `roll apps …` (`crates/roll-cli/src/apps.rs`).
- Dashboard: services on the project canvas (`/dashboard/projects/$id`) and a service page at
  `/dashboard/apps/$id` (deployments, logs, metrics, variables, jobs, shell, domains, usage,
  settings).

```
  browser / curl ──▶ <slug>.up.roll.site, custom domains  (Cloudflare, proxied)
                          │
                    rolld (any node) ─ edge: Host → service ─┐
                          │                                   │ dispatch to the owner of
                          ▼                                   │ apps:service:<id>
               the service's owner node ◀────────────────────┘
               ├─ the waking proxy: queue the request, wake an instance, forward
               ├─ the deploy queue: build → image → roll out → health check → switch
               ├─ the autoscaler: concurrency per instance → instance count (min..max)
               └─ the meter: CPU-seconds, GB-seconds, requests, egress, cold starts
                          │
               Runtime: Docker on this host (one node, development)
                        Fargate tasks, CodeBuild builds (ROLL_APPS_RUNTIME=fargate; no customer
                        code on a node)
```

## What we learned from the others

| platform | what it gets right | what we take |
| --- | --- | --- |
| Railway | the project canvas; services, variables and private networking per project; environments (production, staging, PR envs) that copy a project; Railpack auto-detection; a deploy per push, rollback to any deployment; usage billing per second of real CPU and memory | the whole model: a service is a card on the project canvas, deploys per environment, variables per service and environment, `${{ postgres.DATABASE_URL }}`-style links, `railway up` as `roll up` |
| Render | blueprints, preview environments per PR, health-checked zero-downtime deploys, cron jobs as a service type | service types `web`, `worker`, `cron`; a health check gates every switch |
| Fly.io | Firecracker machines that **suspend** (memory snapshot) and resume in a few hundred ms; the proxy starts machines on request (`auto_start_machines`) and stops them when idle | the waking proxy and the tiered idle states: running → suspended → stopped |
| Cloud Run | request-based autoscaling on concurrency per instance, scale to zero, per-100 ms billing, startup CPU boost, min instances to avoid cold starts | concurrency target as the scaling signal; min/max instances; billing only while serving |
| Knative | the activator: requests for a scaled-to-zero revision are held in a buffer while the autoscaler brings a pod up; revisions are immutable | the owner's proxy is the activator; deployments are immutable revisions |
| Vercel | immutable deployments with their own URLs, instant rollback by moving the alias | every deployment keeps its image; rollback moves the environment's pointer, nothing rebuilds |
| Koyeb, Northflank | scale-to-zero on microVMs, BYO cloud, build + deploy pipelines with logs streamed live | build logs streamed as they happen; a deploy queue per service and environment |

## Decisions

### A service, an environment, a deployment

- **Service** (`svc_…`): what to run and how. It belongs to one workspace project and is a
  resource kind on its canvas (`service`, docs/workspace.md). Source, build settings, run
  settings (type, port, start command, scaling, resources, health check, volume) live on the
  service and apply to every environment.
- **Environment**: a name (`production`, `staging`, `development`, a custom one, a preview's
  `pr-42`). A service runs in an environment once it has been deployed there. Per environment:
  the branch that deploys on push, the variables, the active deployment, the domains, and the
  instances. This is Railway's model and roll secrets' (feature/roll-secrets): a service deploys
  into one of the project's environments and that environment's secrets are injected.
- **Deployment** (`dep_…`): an immutable record of one rollout: the source revision, the image
  it built (by digest), and a frozen copy of the service's settings and the variable names. A
  rollback deploys an older deployment's image and settings again, without building: it is a
  new deployment whose `rollback_of` names the old one, so history only grows. The volume is
  the exception: it's the service's as it is now (its data lives there), so rolling back past the
  volume's addition keeps it mounted (`a_rollback_keeps_the_services_volume`). A rollback holds
  the environment's deploys (Vercel's Instant Rollback; `Environment.held_after`): a push, a CI
  wait whose checks pass, a deploy hook (409) and a moved tag aren't put live, each said in the
  environment's log, so the next merge doesn't put back what was just rolled away from. A person's
  deploy, redeploy or promotion lifts it, as does Resume deploys (`PUT …/environments/{env}
  {held: false}`, `roll apps env --resume-deploys`, the service page). A secrets redeploy still
  runs: it's the live image again, with new values. A crash loop's rollback holds too.
  `a_rollback_holds_the_next_push_until_deploys_resume`.

### Builds

- **How long a build may take**: the node's limit (`ROLL_APPS_BUILD_TIMEOUT`, in minutes, 30 by
  default), or the service's own `build.timeout_s` (Settings → Build timeout, `roll apps set
  --build-timeout 1800`; Render's, Cloud Build's) from a minute to six hours, held to the node's
  (the build log says when it is). A build past it fails as `the build took longer than N
  minutes`.
- **Auto-detect** (`crates/apps-build`): Railpack/Nixpacks-style providers read the tree and
  produce a **plan**: a base image, install, build and start commands, the port, and the
  Dockerfile they make. Providers, in order: Dockerfile (when one exists, it wins), Node (npm,
  pnpm, yarn, bun; Next, Remix, Astro, Vite, Nuxt, SvelteKit), Python (uv, poetry, pip; Django,
  FastAPI, Flask), Go, Rust, Ruby (Rails, Rack), PHP (Laravel, plain), Java (Maven, Gradle;
  Spring Boot), static sites (an `index.html`, or a framework build's output dir served by a
  tiny static server). The generated Dockerfile is shown in the UI and in the build log, so a
  build is never magic, and the service settings or `roll.toml` `[app]` can override any
  command. A multi-stage Dockerfile whose last stage isn't what runs is built at its stage with
  `build.target` (`--target`, a stage's name and never a flag; the tree's own Dockerfile only;
  Fly's `build.target`, Render's Docker target; `docker.rs::a_dockerfiles_stage_is_built_by_its_target`).
- **System packages** (Railpack's `aptPkgs`, Heroku's Aptfile): `build.packages` or roll.toml
  `[app] packages = ["ffmpeg", "chromium"]` (Settings → Build, `roll apps set --packages`), so
  an app that needs ffmpeg, Chromium or libvips doesn't need a Dockerfile. A generated
  Dockerfile installs them after each `FROM` of an image (a stage built on an earlier one has
  them), with apt (`--no-install-recommends`, the lists removed) or apk on an Alpine image; the
  plan's notes name them. A name is lowercase letters, digits and `+ - . : = ~ _` (a pinned
  `libvips42=8.14.1-3` too), at most 50, so nothing in one is a shell's. A static site (no
  container) and a distroless runtime (Go's, no package manager) refuse them; the tree's own
  Dockerfile is left alone (it says what it installs).
- **Config as code** (`roll.toml` `[app]` in the service's root, like `railway.json`): `builder`,
  `dockerfile`, `install`, `build`, `start`, `pre_deploy`, `health_path`, `watch`, `packages`. Read at each build;
  what it sets wins over the service's settings and is named in the build log and the plan's
  notes (`POST /v1/apps/plan` shows it too). It's checked as the API checks settings, an unknown
  key is an error (a typo never silently does nothing), and it must be a regular file. The
  parser is `apps_build::app`, so the CLI reads the file as a build does: `roll apps plan DIR`
  applies it (`-e` for an environment's block) and `roll up` refuses a malformed one before
  anything is made or uploaded; its values (a port, a size) are checked at the deploy, by the
  API's rules. Editors get its shape from `docs/apps/roll.schema.json` (JSON Schema: Taplo and
  Even Better TOML take a `#:schema` line), whose keys and types tests hold to the structs.
  `[app.previews.screenshots]` (only there) is roll previews' (`pages`, `hide`, `[[states]]` as
  its `[preview]` takes them): opaque to roll apps and never read by a build; roll previews reads
  it from the tree at the preview's own revision (`apps_build::app::screenshots`, the same
  regular-file and 64 KiB rules) and checks it with code-preview's rules. The
  file's other sections are roll code's; code-preview allows `[app]` in it. `[app.run]` sets any
  run setting (`type`, `port`, `min_instances`, `concurrency`, `cpu`, `memory_mb`, `schedule`,
  timings) but the volume, and `[app.environments.<name>]` (with its own `.run`) goes on top in
  that environment only, as Railway's environment overrides do. Without a file, an environment
  sets its own size and scale (`PUT …/environments/{env}` `run`: instances, CPU, memory,
  concurrency, suspend and sleep; `roll apps env --run min_instances=1`; "Its own size" in the
  dashboard) over the service's, checked with them as one run. The file still goes on top of
  both, as config as code wins on Railway. Changing one marks the environment `needs_deploy`,
  and an environment's own `min_instances` keeps the service owned, as the service's does. `[app.previews]` (and `.run`)
  goes on top in preview environments (ephemeral ones, `pv-<id>`), before a named block. A
  preview scales to zero unless that block sets `min_instances`, whatever production keeps up,
  so an open pull request costs nothing until someone opens it (Render's `previews.plan` and
  `numInstances`); an image service's previews do the same. A redeploy or a promotion
  applies the file at the deployment's revision for the target environment, so neither drops
  what the file set. What a push can't touch stays the service's: its volume, domains, access
  and variables.
- **Why a generated Dockerfile instead of Nix or buildpacks' lifecycle**: BuildKit's layer cache
  makes dependency installs incremental, multi-stage builds give slim runtime images (cold
  starts scale with image size), and one build path (Dockerfile → BuildKit) serves detected and
  user Dockerfiles alike.
- **Where builds run**: locally, `docker build` (BuildKit) on this host, at most
  `ROLL_APPS_BUILDS` (2) at once per node; more wait their turn, saying so in their log.
  BuildKit takes no per-build CPU or memory limit through `docker build`, so the cap on
  concurrent builds is what bounds them on one node. On the
  fleet, a build is customer code and never runs on a node: it is an AWS CodeBuild build
  (serverless, per-minute, privileged BuildKit inside CodeBuild's own VM) that pushes to the
  service's ECR repository (`<ROLL_APPS_ECR_REPO>/<service>`, made on the first build), behind
  the same `Builder` trait (`runtime/codebuild.rs`). CodeBuild was chosen over Kaniko in Fargate
  (Kaniko is unmaintained since 2025 and slow on big layers) and over BuildKit in Fargate (needs
  privileges Fargate doesn't grant).
- **A CodeBuild build**: the context goes up as a zip (CodeBuild's S3 source takes zips;
  symlinks stay symlinks) to `<prefix>builds/<deployment>-<random>.zip` in the stack's bucket.
  `StartBuild` on `ROLL_APPS_CODEBUILD_PROJECT` takes it as the source with an inline buildspec
  roll writes: log in to ECR, then `docker buildx build` in a `docker-container` builder with
  the job's Dockerfile, `--cache-from`/`--cache-to type=registry,ref=<repo>:buildcache`
  (mode=max, ECR's OCI form, so a rebuild on any node reuses the layers), and `--push` of
  `<repo>:<deployment>`, the image the deployment keeps. `BatchGetBuilds` is polled until it's
  done and its CloudWatch log is followed into the build log; the build timeout is CodeBuild's
  timeout and a local deadline, and a cancel is `StopBuild`. Deleting a deployment's image is
  ECR `BatchDeleteImage` of its tag (the repository and `buildcache` stay). The zip is deleted
  when the build ends.
- **Uploads** (`roll up`): the CLI sends what git would (`.gitignore`, a parent's too, as in a
  monorepo), less `.dockerignore` and `.rollignore`, and never a `.env` file other than
  `.env.example`/`.sample`/`.template`. rolld unpacks it under hostile-tree rules (roll-fetch:
  plain relative paths, no hard links, nothing written through a symlink, names and link targets
  at most 4 KiB read without trusting a header's size, PAX refused) and deletes it once its build
  ends; a sweep removes those of deployments that never built. A build root or Dockerfile is
  reached only through real directories, never a symlink in the tree.
- **Lean Node images**: after the build, devDependencies are pruned (`npm prune --omit=dev`,
  `pnpm prune --prod`), as Heroku's Node builds do, unless the start command uses one (`ts-node`,
  `nodemon`: the plan says so) or only a framework's output directory is copied. An express app
  with TypeScript and ESLint as devDependencies: `node_modules` 45 MB → 4.4 MB, the image 381 →
  332 MB (the rest is `node:22-slim`). A Next.js app whose `next.config` sets
  `output: "standalone"` ships only `.next/standalone`, `.next/static` and `public`, started with
  `node server.js` (a real Next 15 app: `/app` is 53 MB, its page and `public/` answer).
- **A build is skipped when its inputs were built before** (Railway's skipped builds): every
  build has a key (its context's every file, its Dockerfile, target, builder and build
  variables; `deploy::build_key`), and a deploy whose key a kept image of the same project has
  runs that image, saying which deployment built it. So a web service and a worker of one
  repository build once, and pushing the same tree again builds nothing. roll's names for the
  service itself (`ROLL_SERVICE`, `ROLL_SERVICE_ID`, `ROLL_PRIVATE_HOST`, `ROLL_PUBLIC_URL`) aren't
  in the key, so a build that bakes one of them in should deploy with `no_cache` (which always
  builds); the environment's name is, so a preview's build is never production's.
  `api.rs::the_same_inputs_are_built_once`.
- **Variables in builds**: every install and build step sees the environment's variables
  (`NEXT_PUBLIC_*`, `VITE_*`, a Prisma `DATABASE_URL`, an `NPM_TOKEN`), as Railway's builds do.
  They're BuildKit secrets mounted as environment variables (`RUN --mount=type=secret,id=K,env=K`),
  so no value is in a layer, the image's history or the build cache's keys. A secret one a step
  prints is `***` in the build log (see "Secret values in logs"). Each stage declares `ARG ROLL_BUILD_ENV`, a hash of the values, so a changed
  value rebuilds what read it and an unchanged set keeps the cache (`ROLL_DEPLOYMENT` and `PORT`
  stay out). On Docker each value is a secret read from a 0600 file, and the `docker` CLI runs
  with a cleared environment (how to reach the daemon, nothing else), so no variable steers a
  process of roll's (`LD_PRELOAD`, `BASH_ENV`, `DOCKER_HOST`). A tree's own Dockerfile also gets
  the build arguments its `ARG` lines declare, minus names that steer a process
  (`runtime::steers_a_process`), each set for that one `docker` command and never on argv. Docker
  records an `ARG`'s value in the image's history where a `RUN` uses it, so a secret belongs in
  `RUN --mount=type=secret,id=NAME,env=NAME`, which every variable is too. In CodeBuild
  the values are in a Secrets Manager secret of the build's own
  (`roll-apps/builds/<service>-<deployment>-<random>`, a JSON object), and each variable is an
  environment override of type `SECRETS_MANAGER` naming `<secret>:<NAME>`: no value is in the
  `StartBuild` request, the buildspec or the build's record, and the secret is deleted without
  recovery when the build ends. `CODEBUILD_*` and `AWS_*` aren't passed there either.
- **Private registries** (`PUT …/registry`, the settings page): an image source can pull with a
  login (a username and a password or token), sealed like variables and never returned. It goes
  to dockerd with each pull only (`X-Registry-Auth` on the Engine API), never `docker login`,
  which would keep it in the host's docker config for everyone. An image on a registry on this
  host or a private network (loopback, RFC 1918, link-local, unique-local, `localhost`) is
  refused: dockerd would fetch it on a tenant's say-so, and so is one whose name resolves to such
  an address. The login goes only to the registry the image names (a source moved to another
  registry forgets it), and New service (`registry` on create) takes it before the first pull.
- **Public images** skip the build: the deployment pins the image's digest at deploy time, so a
  moving tag never changes a running deployment.
- Build logs stream line by line to the dashboard and `roll deploy` as they're written
  (server-sent events), and are kept with the deployment.

### Deploys

- **A directory knows its service** (`.roll/app.json`, local state, never committed): `roll up`
  writes it the first time, `roll apps use <service>` (Railway's `railway link`) for one made
  elsewhere (the dashboard, a repo), `--clear` forgets it. Every command but `roll up` finds it
  in a parent directory too, as git finds its repo, so a monorepo's app directory and its root
  both work; `roll up` uploads its own directory, so it reads only that one's, and refuses a
  repo or image service (its own source deploys: `roll deploy`).
- **One queue per service and environment.** A new deploy waits for the one in flight; a newer
  push supersedes queued ones that haven't started (`superseded`), as on Railway, and stops a
  push's build that's still running (`superseded`, "a newer push replaced it": its build minutes
  saved, the newer one next). Newer is by landing (a push whose checks passed late never stops a
  later push's build), else by when it was queued. The build's owner looks for one every 3 s,
  whichever node queued it, and a build that lost its status while it ran never rolls out; a
  deploy already rolling out finishes, and a deploy someone asked for (not a push) is never
  stopped. Different environments deploy in parallel.
- **What a deploy brings** (Render's deploy view): `GET …/deploys/{dep}/commits` and the
  dashboard's build log list the commits a repo deployment has over the live one (for the live
  one, over the one live before it), newest first, at most 50, through roll code's first-parent
  history (`Sources::commits`); one not in its history (a rollback) says so. `roll apps
  deploys --commits DEP` prints them.
- **A deploy says what changed** (Heroku's "Set LOG_LEVEL config vars"): a redeploy without a
  message of its own records what `…/pending` named, as `Redeploy: API_KEY added; LOG_LEVEL
  changed; run.memory_mb; the project's secrets` (names only, never a value), so the deployments
  list reads as a history; `roll apps deploys` shows each one's age.
- **A failed deploy says how to fix it** (Heroku's error codes, Render's pointers): a deployment's
  `fix` (read from its error and detail, never stored; `model::fix_for`) names the setting and its
  command for what roll knows: a pull the registry refused (a private image's login), memory
  (exit 137), the build's time limit, a lockfile `npm ci` or `--frozen-lockfile` refuses, a
  health check never answered (listen on 0.0.0.0 at `$PORT`, the health timeout), no start
  command, a tree nothing builds. Shown under the failure in Deployments, `roll apps deploys` and
  `roll up`/`deploy`'s end.
- **States**: `queued → building → deploying → active`, or `failed`, `canceled`, `superseded`;
  an active deployment becomes `inactive` when another replaces it. The dashboard maps them onto
  the shared mark states through `MarkStateProvider` (`deploying` while building or rolling out,
  then `success` or `error`).
- **Zero-downtime rollout**: new instances start next to the old ones, each must pass the
  health check (`GET <health_path>` answering 2xx–3xx, or the port accepting a connection when
  no path is set) within `health_timeout`; then the environment's pointer moves in one write,
  new requests go to the new instances, and the old ones drain (requests in flight finish, up
  to 30 s) and stop. A failed health check fails the deploy and leaves the old deployment
  serving. A service scaled to zero rolls out without starting anything: the pointer moves, and
  the next request wakes the new deployment (a health check runs at the first wake instead, and
  failing it there rolls the pointer back automatically). On real Docker, a request every 10 ms
  through a redeploy is answered 200 every time
  (`docker.rs::a_deploy_under_traffic_answers_every_request`).
- **Environments**: production comes with the service; more are made from the switcher (a
  name, the branch that deploys to it, and the environment whose variables it starts with:
  `variables_from`) or `roll apps env`. One for the whole project at once (Railway's) is
  "Every service in the project" in the switcher or `roll apps env --all-services`: each
  service's own `PUT …/environments/{env}`, each result reported and any that failed named.
  Deliberately not a project route: the permissions catalogue decides by the service and the
  environment in the path (`apps:service:<id>`, production deploys being `deploy`), and one
  request touching every service would need its own check per service inside the product,
  which products never make (decided 2026-11-14; reversible). An environment's Settings panel changes its branch,
  makes it private (a fresh 32-character token from the browser's CSPRNG; the link is shown
  once, only its hash kept) or public again, promotes another environment's live deployment to
  it, and deletes it (production stays). **Promotion** (`POST …/promote {from}`, `roll apps
  promote --from staging`, Railway's) deploys the image `from` runs, with no build or pull, under
  the target's own variables and the service's current settings: staging's tested image goes to
  production as it was tested. With `expect` (a deployment id; `roll apps promote --expect`,
  roll previews' sheet) it's that deployment or nothing: a 409 when `from` runs another, checked
  against the read the promotion is made from, so a push that went live between a reviewer's
  look and the click isn't what's promoted.
- **Who deployed** (`Deployment.actor`): a push names its commit's author as `git:<name>` (what the commit claims, shown as such; it can't pass for a token or `operator`); the API, the CLI and
  the dashboard name the caller rolld's gate saw (`operator` for the operator's token; on
  roll-mcp a scoped token's or connection's name, from `roll_auth::Grant`), never anything a
  client says about itself; deploys roll makes on its own (cron, a settings redeploy it starts)
  name nobody. The deployments list, `roll apps deploys` and notifications ("deploy failed … by
  Ada") show it.
- **Health checks while running** (Render's, Fly's `checks`): with a `health_path`, every
  running web instance is checked every `health_every_s` (2–300, default 10); `health_failures`
  in a row (1–10, default 3: an app that hangs or answers errors with its port still open)
  replace it: out of rotation, stopped, counted as a crash (so a loop of them notifies), and the
  next request or `min_instances` starts a fresh one. Each failure is a line in the
  environment's log. Without a path, a port that stops answering is noticed by the proxy and the
  reaper. A worker has no port, so its check is `health_command` (Docker's `HEALTHCHECK CMD`,
  Kubernetes' exec probe): a shell command Docker runs in the instance at the same interval,
  healthy on exit 0, counting only after `health_timeout_s`. One that fails `health_failures`
  times in a row (a consumer stuck on a lock with its process up) is replaced as a crash, by its
  `restart` policy, and says so in the log. The command runs in production, so changing it is a
  deploy in the permissions catalogue; the interval and failures are a write. The command needs
  a shell in the image, and Fargate doesn't run it yet.
- **One-off jobs** (`POST …/jobs`, `roll apps run -- <cmd>`): a command in a fresh container of
  the active deployment, with its variables on the project network (no port, no volume: the
  service's instance may have it), as Render's one-off jobs and `heroku run` are. Its output is a
  log of its own (followed over SSE, kept like build logs); it ends at its exit, `timeout_s` or a
  cancel, and its container goes. `roll apps run` exits with the command's exit code. A node
  that takes the service over marks jobs a lost node was running as failed. With `tasks`
  (`roll apps run --tasks N`, Cloud Run Jobs'), it's N containers, ten at a time, each with
  `ROLL_TASK_INDEX` (0…) and `ROLL_TASK_COUNT` to split the work by; their output is the job's one
  log, each task's end a line in it, and the job fails as its first failing task does.
- **Files to and from an instance** (Fly's `sftp`, Render's `scp`): `GET …/files?path=` answers
  what's at an absolute path in an instance as a tar, `PUT …/files?path=` (a tar, at most
  100 MiB) writes its files under a directory there; Docker's `docker cp` both ways. The
  instance is the one a shell would get (a web service woken and held for the copy), or
  `?instance=`. `roll apps cp ./seed.db :/data/` and `roll apps cp :/app/report.csv .` (`:`
  marks the instance's side; unpacked here with nothing outside the target). Admin, as a shell
  is (files hold what the app keeps, its secrets too, and what's written runs next), behind roll
  secrets' reveal rule, and each copy is `audit.files.read`/`audit.files.written`. The Shell tab's
  Files panel does both from the browser: a path downloaded as a tar, files picked and written
  under a directory (a plain tar made in the page, `lib/bundle.ts::tarOf`), in the instance chosen
  for the shell.
- **A shell** (`GET …/shell`, a WebSocket; `roll apps ssh`), as `railway ssh` and
  `fly ssh console`: `bash` (else `sh`) in a running instance through a TTY exec over the Engine
  API, the terminal raw on the CLI's side and resized with the window. A web service asleep is
  woken and held awake while the shell is open; a worker's running instance is used; a cron
  service has none (a job does that). It needs Docker on a local socket. The dashboard's Shell
  tab is the same shell over HTTP sessions (a browser can't send the token on a WebSocket): the
  screen over SSE with its scrollback, keys and resizes as POSTs, at most 8 open per
  environment, closed on leaving the page or after 10 idle minutes.
- **Sidecars** (`run.sidecars`, roll.toml's `[[app.run.sidecars]]`, Settings → Sidecars), as Cloud
  Run's sidecars and Fly's multi-container machines: up to four containers beside each instance
  of a web service or a worker (a database proxy, an agent, a collector), in its network
  namespace, so the app reaches one on localhost: its sandbox's when it has one, else its own.
  Each is started after the app (again at every start: a container joined to one that restarted
  keeps a dead namespace), frozen and thawed with it, stopped after it when it sleeps (an idle
  service still costs nothing) and removed with it, restarted up to five times if it fails. Its
  image is pulled and pinned with each deployment (stored in the deployment's run, so a rollback or
  a redeploy runs the same one). It gets its own `env` (plain settings, shown as they are) and only
  the environment's variables it names (`variables`, its secrets too), never the rest; hardened as
  the instance is (no new privileges, dropped capabilities, a PID limit), sized by `cpu` (0.1–2,
  default 0.25) and `memory_mb` (64–4096, default 128), its usage metered with the instance's.
  Its output is in the environment's log under its own container's name (`<instance>-sc-<name>`).
  Sidecars stay out of the runtime's lists (nothing takes one for an instance of its own) and go
  by their instance's label. Docker only for now: the fleet refuses a deploy with them, as it
  does volumes. `sidecars_run_beside_each_instance_with_only_what_they_name`,
  `a_sidecar_answers_the_app_on_localhost_across_a_sleep` (real Docker, with and without a
  sandbox).
- **A console** (`…/shell?console=true&command=`, or `"console": true` on a session; `roll apps
  run -i -- rails console`), as `heroku run bash` and `fly console`: a fresh container of the live
  deployment (its image, variables and network; no port, no volume, no traffic) with the command
  in a terminal (a login shell when there's none), sized by `cpu` (0.25–8) and `memory_mb`
  (256–16384) or the environment's own size, and removed when the terminal closes. The serving
  instances are never touched. Metered as an instance while it runs; capped and paused services
  have none. Opening one is `audit.console.opened` (whether a command was given and its size,
  never the command, which can hold a credential); the Shell tab's "A new console".
- **Single-page apps**: a static build (served by busybox httpd) whose `package.json` has a
  client-side router (`react-router-dom`, `@tanstack/react-router`, `vue-router`, …) is a
  single-page app (`Plan.spa`): roll's proxy answers a GET for a page (HTML, no file extension)
  that the app 404s with its `/index.html` and a 200, so deep links work (Vercel's and Render's
  rewrite). A missing file (`/logo.png`) stays a 404. `build.spa` or `roll.toml` `[app] spa`
  forces it either way.
- **JS workspace monorepos**: a service whose root directory (`apps/web`) is a package of a
  pnpm, npm or yarn workspace at the repo's root (`pnpm-workspace.yaml`, or `workspaces` in the
  root `package.json`) builds with the whole repo as its context, as Vercel and Railway build
  one: the install is the workspace's, once at the root (the lockfile and `packages/*` are
  there), the build runs the app's `build` script through the manager's workspace command
  (`pnpm --filter <name> run build`, `yarn workspace <name> run build`,
  `npm run build --workspace <dir>`), and the start runs its `start` script in its directory
  (`cd apps/web && exec npm run start`: npm ships in the image, so nothing downloads at a cold
  start; Yarn Berry's Plug'n'Play uses yarn). A command set in the service or `roll.toml` wins,
  and a root with its own Dockerfile keeps the root as its context. The plan (`POST
  /v1/apps/plan`) says so in its notes.
- **A push can skip its deploy** (Render's `[skip render]`): `[skip deploy]`, `[skip ci]` or
  `[skip roll]` (or `[ci skip]` and the rest the other way round), in any case, anywhere in the
  commit's message. The environment's log and the timeline (`deploy.skipped`, with the revision
  and the tag) say so; nothing is built.
- **Watch paths** (monorepos; `build.watch`, `[app] watch`, the settings page): a push deploys a
  repo or Git URL service only when a path that differs between what the environment runs and
  the pushed commit matches one of its patterns (gitignore-style from the repo's root, roll code's
  CODEOWNERS matcher, so a node without roll code deploys every push; roll.toml's are read at the
pushed commit, the service root's for a Git URL too). For a Git URL the paths come
  from one shallow fetch of both commits with no blobs, trees compared by id (an unchanged folder
  isn't walked; past 10,000 paths, deeper than 64 folders, or if the fetch fails, the push
deploys, and the log says why). A Git host that takes no fetch filters gets the same fetches
whole. The environment's log
  says which pushes it skipped, and the hook answers `{ ignored }`. Nothing set: every push
  deploys, as before.
- **Pre-deploy command** (`run.pre_deploy`, `roll apps set --pre-deploy`, the settings page):
  once per deploy, after the image is ready and before anything starts, in a container of the
  new image with the deployment's variables on the project network (not the volume: the old
  instance may still have it), as Railway's pre-deploy command and Render's do. Its output is in
  the deploy log; a non-zero exit, a timeout (the build timeout) or a cancel fails the deploy, and
  the old one keeps serving. For migrations, which must suit both the old and the new code.
- **Redeploy** (`POST …/redeploy`, `roll apps redeploy`, the dashboard's banner after a change
  of variables or settings): the active image again with the service's current run settings and
  variables, same revision, no build. It's how a `roll up` service takes new variables.
- **Retries**: `POST …/deploys` takes an `Idempotency-Key`; a retry answers the first call's
  deployment.
- **Instant rollback**: the old image is still in the registry, so a rollback is a rollout with
  no build: seconds, not minutes.
- **A failed deploy says why**: a build's failing step (how it ended, and that step's last line
  of output, as BuildKit said it), not only its exit code; a rollout that fails copies the new
  instances' output into the deploy's log and its last line into the deployment. What the app or
  the build printed is `error_detail`, shown to its owner and never sent out: the commit status,
  the `deploy.failed` event and its notifications carry only roll's words (a stack trace or a
  printed connection string must not reach a public repo's checks or a chat). While the
  health check waits, the sandbox's listening sockets are read: an app on its port but only on
  loopback fails at once ("listen on 0.0.0.0:8080"), one on another port is named when it times
  out ("listens on 0.0.0.0:3000, not on port 8080: listen on $PORT"), as Render's "no open ports"
  (`api.rs::a_deploy_whose_app_listens_elsewhere_says_where`, `docker.rs::an_app_on_loopback_fails_its_deploy_saying_so`).
- **What's kept to roll back to**: each environment's newest 10 built deployments
  (`ROLL_APPS_KEEP_DEPLOYMENTS`), its live one, any pending, live or ready (a no-traffic deploy),
  and any that ended in the last day (draining, skew protection) unless its environment was
  deleted. Every ten minutes the service's owner prunes the rest: still in the history (paged with
  `?before=`), `pruned_at` set, its image, static site and build log gone once no deployment left
  names the image (a promotion or a rollback runs the same one). A rollback to a pruned one is a
  409 saying to deploy its revision again; the dashboard and `roll apps deploys` show it as pruned
  (`api.rs::old_deployments_are_pruned_past_what_an_environment_keeps`). Before this, nothing but
  a service's deletion removed an image: every push, and every deleted preview, kept its own.

### The runtime: scale to zero, wake on request

- **What a visitor sees when roll answers instead of the app**: one small page with the Horizon
  mark (`assets/horizon.svg`, `@roll/brand`'s export, a test keeps it equal), dark or light as
  their system is: "Starting up" or "Restarting" (503, `Retry-After`, and it reloads itself after
  5 s), "Paused" (402, the project's spend limit), private (401), not deployed (404). An app's
  failure (a connection closed before it answered) is a 502 that says so without the error, which
  goes to the environment's log.
- **A request that finds its instance gone** (killed or out of memory, before roll noticed:
  nothing listens on its port, or a kept-alive connection to it broke and the instance is gone:
  the runtime says it isn't running, or, since a runtime learns of a kill a moment late, nothing
  accepts a fresh connection to it; one that accepts is alive, slow or closing the connection
  itself, and gets a 502, nothing counted against it) goes, if it's a GET, HEAD or OPTIONS with no body (by its headers and the body
  itself: a safe request, which RFC 9110 lets be repeated, as browsers do when a reused connection
  closes), once more to a fresh instance (Fly's proxy retries the same way), the instance counted as
  crashed and the environment's log saying so (and why, when no fresh one starts); a request
  with a body gets the "Restarting" 503, never sent twice
  (`api.rs::a_get_to_a_vanished_instance_goes_to_a_fresh_one`, with `Fake::vanish`: a kill roll
  isn't told of). The fake runtime's kill ends kept-alive connections too, as the kernel's does.

- **The waking proxy** (Knative's activator, Fly's proxy) runs on the service's owner node. A
  request for a service with no running instance is held (up to `wake_timeout`, 60 s, then a
  503 with `Retry-After`), one wake starts, and every held request goes through as soon as the
  instance answers its port. Requests arriving while it wakes join the same wait; nothing
  starts twice.
  - The wake runs on its own task, so a client that goes away mid-wake leaves nothing half
    started. At most 1,000 requests wait per environment (more get a 503 with `Retry-After`),
    and after a failed wake the requests behind it get its error for a backoff (1 s doubling to
    30 s) instead of each trying again. Calls to Docker have timeouts, so a hung daemon fails a
    wake instead of holding it.
  - An instance that dies on its own (killed, out of memory) is noticed by the next reaper tick,
    or by the proxy when its port refuses a connection (a 503 with `Retry-After: 1`); the next
    request starts a fresh one.
  - A service with a volume has one writer: a rollout stops the old instance and keeps the
    environment locked until the new one is live, so requests wait for the new deployment
    instead of starting the old one on the volume.
  - Deleting an environment deletes its row first, then tears down its live state, which
    refuses to make or adopt anything afterwards. A node that loses a service's lease cancels its
    deploys in flight and doesn't switch an environment it no longer owns; on a shared cluster
    backend the switch is fenced too (`fences`: the service's lease epoch, checked in the
    switch's own transaction), so an older owner finishing late can't move the pointer.
- **Idle states**, cheapest last:
  1. **running**: serving, billed per second of CPU and memory it actually uses.
  2. **suspended**: after `suspend_after` (default 30 s) without a request, the instance is
     frozen (Docker `pause`, the cgroup freezer: memory kept, no CPU). Resuming takes
     milliseconds. Not billed to the user; roll pays for the memory, so it lasts only until
     `sleep_after`.
  3. **stopped**: after `sleep_after` (default 10 min) the instance stops. Its container stays
     created, so waking is a start of the app, not a pull or a create. This is the cold start.
  An HTTP service with `min_instances: 0` reaches stopped; `min_instances ≥ 1` keeps that many
  running (no cold starts, billed while up).
- **Why tiered** and not one idle timeout: most wakes come soon after the last request
  (a user clicking around, a webhook burst), and a frozen process answers them in
  milliseconds; the long tail of idle time costs nothing because it ends stopped. This is Fly's
  suspend/stop split, done with the freezer locally.
- **On the fleet** (`ROLL_APPS_RUNTIME=fargate`, `runtime/fargate.rs`), instances are Fargate
  tasks, with the isolation of docs/cloud/README.md's "Isolated work" but no template:
  - A task definition per deployment image, registered by roll and reused: family
    `roll-apps-<service>-<hash>` (the hash over the image, the size, the port, whether a start
    command replaces the image's own, the roles, the log group, the architecture, the guard and
    its idle). `app` has the image's own entrypoint (or `sh -c <start>`), `awslogs` to
    `ROLL_APPS_FARGATE_LOG_GROUP`, the execution role, and a task role only if one is set (none
    by default: no AWS credentials in the task). A web task also has `guard`, roll's small image
    (`roll-fetch proxy`, `ROLL_APPS_FARGATE_GUARD_IMAGE`): the only port nodes reach is its
    `ROLL_APPS_FARGATE_PORT` (the stack's PreviewPort, 9970, the one the isolated subnets'
    security group and network ACL admit, as for previews), forwarded to the app's own port
    over the task's shared localhost; it ends the task once nothing has moved for well past the
    service's `sleep_after` (never for `min_instances` above 0), so a task whose node went away
    still stops. `create` registers or finds it and runs nothing.
  - `start` is `RunTask` (awsvpc, `ROLL_APPS_FARGATE_SUBNETS` and `_SECURITY_GROUP`, on demand,
    `startedBy roll-apps-<service>`, tags `sh.roll.apps=<service id>` and
    `sh.roll.apps.instance=<name>`). It waits for RUNNING and answers the task's private address
    on the guard's port, which the owner's proxy health-checks and forwards to as it does a
    container's. `stop` is `StopTask` (SIGTERM, then SIGKILL after the task definition's 30 s
    `stopTimeout`), and the next start is a new task. `wait` polls `DescribeTasks` to STOPPED and
    answers the `app` container's exit code (cron runs, jobs, pre-deploy commands). `list` is
    `ListTasks` by `startedBy` with tags, so a node that takes a service over finds an earlier
    owner's tasks and stops them.
  - **Variables never go in a `RunTask` request** (it shows in `DescribeTasks` and CloudTrail).
    Each start writes them to an object of its own, `<prefix>env/<instance>-<random>.env` in the
    stack's bucket, and the request names only its ARN as the `app` container's environment
    file. ECS reads it with the execution role before the app starts, and roll deletes it as
    soon as the task is RUNNING (or stopped, or failed to start). This is code-preview's bundle
    with ECS as its `fetch`. It was chosen over a `fetch` container because an app keeps its
    image's own entrypoint (sourcing an env file needs a shell, and distroless images have none)
    and no roll image goes in the task. An environment file is one line per variable, so a value
    with a line break (a PEM key) comes from the deployment's own secret instead (below).
  - Public images are pinned by a manifest `HEAD` on their registry (anonymous, or with the
    registry's anonymous token), through a client that refuses private addresses.
  - Fargate can't freeze a task, so there the states are running → stopped (`can_suspend` is
    false, and `settle` does nothing: no CPU boost, the size is the task's). A cold start is a
    task launch (tens of seconds for a large image; SOCI indexes cut the pull), so services that
    must answer instantly set `min_instances: 1`. A warm window (a task kept up a few minutes past
    `suspend_after`, unbilled) isn't built: an idle task stops at `sleep_after`. The fast path on
    the fleet is in `## Next`: Firecracker microVM hosts with snapshot restore (what Fly and
    Lambda SnapStart do), behind the same `Runtime` trait.
  - Metering: `usage` answers nothing on Fargate (a task's cgroup isn't read). Fargate bills size
    × time, so the meter charges the allocation while an instance runs (its CPU × seconds and
    its memory).
  - Settings (`runtime::aws::FleetConfig::from_lookup`): `ROLL_APPS_RUNTIME=fargate` (default
    `docker`), `ROLL_APPS_FARGATE_CLUSTER`, `_SUBNETS` (comma separated, the isolated subnets),
    `_SECURITY_GROUP` (the isolated one), `_PORT` (9970), `_GUARD_IMAGE`,
    `_EXECUTION_ROLE`, `_TASK_ROLE` (optional), `_LOG_GROUP`, `_PUBLIC_IP` (`1`), `_ARCH`
    (`ARM64` or `X86_64`), `ROLL_APPS_CODEBUILD_PROJECT`, `ROLL_APPS_ECR_REPO` (`apps`),
    `ROLL_APPS_BUCKET` (default `ROLL_S3_BUCKET`), `ROLL_APPS_PREFIX` (`apps/`) and `AWS_REGION`.
    Unset, rolld runs and builds on Docker as before.
- **Autoscaling**: the owner counts requests in flight per instance every second; the target is
  `concurrency` (default 50) in flight per instance. Instances needed = ceil(in flight /
  concurrency), between `min_instances` and `max_instances`, scaling up at once and down only
  after 60 s below target (no flapping). Workers run exactly `min_instances` (at least 1).
- **Workers** have no port and no proxy: they run `min_instances` (default 1) and restart on
  exit with backoff (1 s doubling to 60 s). A crashed instance waiting out its backoff counts
  toward the floor, so nothing new is made in its place meanwhile (which would skip the backoff
  and, once it started again, run two where one was wanted).
  Crashed ones beyond what's wanted (a backlog fell while they waited) are removed, not started
  again to be scaled in; their crashes stay in the crash list.
- **Commands on a schedule on any service** (Heroku Scheduler, Vercel crons): `run.schedules`,
  at most 20 of `{name, schedule, command, timezone?, timeout_s?}` (the Jobs tab's "On a
  schedule", `roll apps jobs --schedule NAME --at "0 3 * * *" --command …`, roll.toml
  `[[app.run.schedules]]`), so a nightly cleanup doesn't need a cron service of its own. The
  owner's reaper pass starts each due one as a job of the live deployment (`jobs`: its
  variables, its network, its log `job:<id>`), skips it while the last job it started still
  runs, and counts a schedule from when it's first seen, so nothing is made up for the time
  before it existed or while the environment was paused. An environment's own run can set its
  own list. A start that fails (spend limit, five jobs already running) is said in the
  environment's log. The environment's `schedules` say when each is next due and the job it last
  started (Heroku Scheduler's "Next due"): the Jobs tab shows both with a Run now (the same
  command as a job, its schedule untouched), `roll apps jobs` lists them.
- **Schedules that call a path** (Vercel's crons): a web service's schedule can be `{name,
  schedule, path: "/api/cron"}` instead of a command (`roll apps jobs --schedule NAME --at …
  --path /api/cron`; in the Jobs tab, a "command" starting with `/`). Each due time the owner
  GETs it through the environment's own proxy (it wakes a sleeping service), past its allow
  list, firewall, rate limit and access gate (`public::Scheduled`, an extension no client can
  send), with `User-Agent: roll-cron/1.0` and `Authorization: Bearer $CRON_SECRET` when the
  environment has a `CRON_SECRET`, as Vercel sends it. A call still out skips the next due time;
  each call's status and time are the schedule's `last_call`; one that answers 4xx/5xx or not at
  all (`timeout_s`, default 300) is `cron.failed`, naming the path. vercel.json's `crons` become
  `vercel-N` schedules at build time unless the service already calls that path, so an app from
  Vercel keeps them. `api.rs::a_schedule_calls_a_path_and_a_failing_answer_is_said`.
- **Cron jobs** are a service type with a schedule (five-field cron, in `run.timezone` — IANA, its daylight saving kept, Cloud Scheduler's — or UTC). Each run is a fresh
  instance of the active deployment that runs to its exit, with a timeout (default 1 h) and its
  output as a log; a run that is still going when the next is due is skipped (Railway's rule).
  Each run is read on its own (`…/runs/{run}`), its output is its own (`…/runs/{run}/logs`: its
  container's lines, named by the run's id, which stay in the environment's log for drains, so
  read as that log is), a hung
  one is canceled (`…/runs/{run}/cancel`), and a run a dead node left `running` fails with its
  `cron.failed` when the service is taken over, instead of holding every later run back; `roll
  apps runs [--now | --logs RUN [--follow] | --cancel RUN]`, and the dashboard's Scheduled runs
  open one's output (Render's, Cloud Run Jobs').
  A run that fails or times out is tried again as `run.retries` (0–5; Cloud Run Jobs'
  `--max-retries`; `roll apps set --retries`, Settings' Retries) allows: after 30 s, then twice
  as long each time (10 min at most), each try its own run with its `attempt`, and only the last
  failure is a `cron.failed` (a team hears about a run that kept failing, not each try). A
  canceled run isn't tried again, nor one whose service moved or whose environment was paused
  meanwhile. `a_failed_cron_run_is_retried_and_only_the_last_failure_is_an_incident`.
  The schedule lives with the service and fires on the owner node; roll tasks wasn't used for
  this because a cron run must start an instance on the owner (a runtime action, not a webhook
  delivery), and keeping the schedule next to the deployment keeps a rollback's schedule right.
- **Resources**: `cpu` (0.25–8 vCPU) and `memory_mb` (256–16384) per instance, as Docker
  `--cpus`/`--memory` (no swap) and a pids limit, or the Fargate size rounded up (the default
  0.5 vCPU and 512 MB is a 0.5 vCPU, 1 GB task).
- **How full a volume is** (Railway's and Render's disk charts): measured once an hour on the
  service's owner (`Runtime::volume_used`: on Docker `du -skx` in a helper container that
  mounts it read-only, after `docker volume inspect`, since `-v` would make a missing one), kept
  in `state` and answered as the environment's `volume_used`; the volume's backups panel shows
  it as a meter. Passing 90% of its size is `volume.full` (notified by default) once, again
  only after it went under 80%: a host without disk quotas doesn't stop a volume growing past
  its size, and a full disk takes its database down. **Egress per minute** is in Metrics (the
  proxy's bytes answered, what egress is billed by) and the Prometheus text.
- **Volumes**: a service may have one volume (a mount path and a size). Locally it's a named
  Docker volume `roll-apps-vol-<service>-<env>`; a service with a volume runs at most one
  instance (`max_instances` is forced to 1, as on Railway). On the fleet it's an EFS access
  point per service and environment (in `## Next`).

### Isolation

- Customer code never runs on a fleet node: instances, cron runs and builds are Fargate tasks
  and CodeBuild builds. Locally they're Docker containers with no host mounts (sources are
  copied into the build context, never bind-mounted), CPU and memory limits (memory without
  swap), a pids limit, `no-new-privileges`, Docker's default seccomp/AppArmor profiles, and the
  capabilities nothing here needs dropped (`NET_RAW`, `MKNOD`, `AUDIT_WRITE`, `SETFCAP`). Disk
  isn't capped locally: a writable-layer quota (`--storage-opt size=`) and `volume.size_gb`
  need overlay2 on XFS with project quotas, which a plain ext4 host doesn't have; on the fleet
  Fargate's ephemeral storage bounds it.
- Variables reach a container as values of the Engine API's create call: never argv, never a
  file, and multi-line values (PEM keys) work. Only over the CLI (a daemon that isn't a local
  socket) do they go through a 0600 env file, removed right after the create; that path refuses
  a value with a line break.
- Every container, network, volume and image roll apps makes is named `roll-apps-…` and carries
  the label `sh.roll.apps=<service id>`, so leftovers are found and removed by exact name.
- A service's variables reach only its own instances. They're stored sealed
  (`ROLL_SECRET_KEY`, AES-256-GCM; locally a key file under `<home>/apps/key` made 0600 on first
  start), never logged, and never in a `RunTask` or `StartBuild` request (a task's come from its
  own environment file, a build's from its own Secrets Manager secret, each deleted once read; an
  environment file a node died before deleting is swept by any node every ten minutes once it's
  twice the start timeout old, `Runtime::sweep`).
  They're returned in the clear only by the `reveal` route.

### Networking

- **Public URLs**: every service in an environment gets
  `https://<service>-<project>[-<env>].up.roll.site` (production omits the env, and the project
  is left out when it has the service's name, as `roll up` names both; a taken name
  gets a 4-character suffix). The name can be changed, as Railway's can: "Change" under roll
  domain in Domains, `roll domains rename NAME`, or `hostname` in `PUT …/environments/{env}`
  (`an_environments_generated_name_can_be_changed`). The old name stops answering at once and is
  anyone's after. A few names are roll's (`www`, `api`, `status`…), and a name with `--` would
  read as a deployment's own (`<id>--<name>`). Where custom domains' CNAMEs name the old one
  (locally; the fleet's target is fixed), the rename waits until they're removed. `up.roll.site` keeps apps off the bucket namespace
  (`<bucket>.roll.site`) and on roll.site, which only serves user content. Cloudflare proxies it
  (`*.up.roll.site` needs Advanced Certificate Manager on the zone, a fixed $10 a month; the
  origin has the wildcard from ACME DNS-01 like the other site names). Locally the domain is
  `apps.localhost` on the apps listener (`ROLL_APPS_LISTEN`, `127.0.0.1:7080`), and each service
  can also get a port of its own (`ROLL_APPS_PORTS=58100-58199`) so it opens on a machine without
  wildcard DNS (a Tailscale name).
- **Notifications** (`…/projects/{project}/notifications`; Project settings → Services, and a service's Settings → The project): a project's
  failed deploys and crash loops, and whatever waits on a person (`rollout.waiting`: a manual
  rollout's step passed; `deploy.ready`: a deployment built beside the live one, under a hold or
  `no_traffic`), and when asked every new live deployment, go to its targets:
  a webhook (JSON, signed with HMAC-SHA256 over `<timestamp>.<body>`), a Slack incoming webhook,
  a Discord webhook, a Microsoft Teams workflow (`teams`: an Adaptive Card, no mentions) or a
  Google Chat space's webhook (`googlechat`: text with `<` and `>` taken out, so an event can't
  mention `<users/all>`), email to the project's owner, or an on-call service (Heroku's and Cloud
  Monitoring's PagerDuty and Opsgenie): `pagerduty` with its integration's `routing_key` (Events
  API v2) or `opsgenie` with an API integration's `api_key`, each sealed as a target's secret and
  never returned, `region` `us` or `eu`, sent only to the service's own API (no URL of the
  tenant's). A failure opens an incident keyed `roll:<service>/<environment>/<event>` (a repeat
  updates it), its severity by what failed (a crash loop or the spend limit critical, a failed
  deploy, error rate or job an error, the rest warnings); `alert.resolved` closes its alert's, and
  a deploy that goes live closes a failed deploy's (an on-call target that pages on `deploy.failed`
  hears `deploy.active` for it). An on-call target's deliveries go one at a time in the events'
  order, a retry included, so a close never overtakes its open; others go side by side.
  `roll apps notifications --add - --kind pagerduty` reads the key from stdin.
  `on_call_targets_open_and_close_incidents`. Email is on by default (Render's,
  Vercel's): a project's first service adds an `email` target with the default events, once
  per project (`state` remembers it, so a removed one stays removed; `roll apps notifications
  --email` adds it back). It goes only to the owner's own account address, looked up at each
  send through roll-accounts' mailer (`integrations::Mailbox`, `Accounts::mail_user`), never to
  an address a caller names, so no tenant can make roll mail a stranger; an operator's project
  (no owner) and a deployment without mail have none. One node (the singleton `apps:notify`) follows the event log after a
  cursor in `state`; a page of events' deliveries and the cursor's move are one transaction, so
  an event is queued once and never skipped. A delivery is tried at once, then after 30 s, 2 min,
  10 min, 30 min and 1 h, then marked failed; each target keeps its delivery log, and Test sends
  one now. URLs are sealed (a Slack URL is a credential), HTTPS, fetched with public addresses
  only and no redirects (`apps_control::net`, which the Fargate runtime's registry client uses
  too), answers read bounded; Slack must be `hooks.slack.com`, Discord `discord.com/api/webhooks`,
  Teams a Workflows (`….logic.azure.com`, `….powerplatform.com`) or `….webhook.office.com` URL,
  Google Chat `chat.googleapis.com/v1/spaces/…` (`api.rs::teams_and_google_chat_targets_get_their_own_formats`).
  A target follows the environments and services it names (`environments`, `services` by name
  or id, kept by id; empty: all), and leaves preview environments out unless `previews` is set
  or it names one, so each pull request's failed deploys don't reach the team's channel; a
  project's own events (spend) always come. Targets made before this follow everything, as
  they did (`roll apps notifications --add URL --only-env production --previews`; Vercel's and
  Render's). A target's latest deliveries (status, tries, how it answered) are in Project
  settings and `roll apps notifications --deliveries ID`.
  - **Why this, not roll code's webhooks or chat ops** (decided 2026-11-13; reversible): roll
    code's delivery queue is per repository, and an upload or image service has none; chat ops
    drives roll from chat through installed apps and bound threads, more than an alert needs,
    and a Slack or Discord incoming webhook needs no install. Railway and Render notify the same
    way. The event log is the seam: a platform-wide notifications product (email, roll code's
    owner-level webhooks, chat ops) reads the same cursor, and this deliverer goes then.
- **The event log** (`GET /v1/apps/events?after=`): `deploy.active` (written in the switch's
  transaction), `deploy.failed` (in the failure's), `crash_loop` (once per loop, by the reaper),
  in order, kept 7 days, for anything that must not miss one (roll code's webhooks, chat ops, an
  MCP client) to read after its cursor. Sequence numbers come from a counter row updated in the
  event's own transaction, not an identity (taken before commit, so on Postgres a slow
  transaction could land an older number after a reader moved past it): they commit in order
  with no gaps.
- **Crash loops**: every crash (an exit, a start that fails or never answers, a port that stops
  answering) is counted per environment, with why: the runtime's exit code and whether the kernel
  killed it for its memory ("it ran out of memory (its limit is 256 MB)", from the Engine API's
  `State.OOMKilled`/`ExitCode`, or Fargate's container `reason`/`exitCode`); the Memory chart
  draws the limit; three in ten minutes is a crash loop, said once in the
  environment's log and on `Environment.crash_loop` (the count, when, and why), which the
  service page shows as a banner with its logs a click away.
- **Private environments** (previews): an environment with an access token answers only
  visitors who bring it: `?roll_access=<token>` once sets a host-only `HttpOnly; SameSite=Lax`
  cookie and redirects to the URL without the token; a request without it is a 401 that wakes
  nothing. Only the token's SHA-256 is stored, compared in constant time, and the cookie is taken
  out of what the app receives. roll previews sets it on every `pv-…` environment. An automation
  (a CI run's end-to-end tests) can be let in with the project's bypass secret, as Vercel's
  protection bypass does: `X-Roll-Preview-Bypass: <secret>` on the request, only its SHA-256
  stored (`bypass_sha256`), the header never reaching the app.
- **Never indexed**: every answer of a preview's (ephemeral) environment, and of a deployment's
  own URL, carries `X-Robots-Tag: noindex`, whatever the app sets (Vercel's previews do the
  same): the app's pages, roll's pages, the 401 and the access link's redirect. Production is
  left as the app says.
- **Custom domains**: the user adds `www.example.com`, points a CNAME at the service's
  `up.roll.site` name, and roll verifies it (the CNAME, or a `_roll-challenge.<host>` TXT with the
  domain's token for apex domains behind an ALIAS/flattening provider), checking pending ones
  every minute. Domain states: `pending` (waiting for DNS) → `active`, or `failed` with the
  reason. A wildcard, `*.example.com` (Railway's and Render's), serves every name one label
  under it that no exact domain claims: a CNAME on `*.example.com`, and the TXT proof on
  `_roll-challenge.example.com`. Its certificate needs DNS-01, so where custom domains get
  certificates and they can't be wildcards (`DomainTls::wildcards`, the edge's HTTP-01 today)
  it's refused at once rather than left pending; on a node serving plain HTTP it just works. A domain that doesn't verify says what DNS answered and the likely mistake: where its CNAME
  points instead, addresses where a CNAME belongs (Cloudflare's proxy on, an apex without
  flattening), no record yet, the TXT on the hostname instead of the challenge name or with more
  around the token (quotes); `domain.active` when it goes live, `domain.expiring` a day before an
  unverified one goes (with what DNS said) and `domain.expired` when it does, `domain.tls_failed`
  when its certificate fails (once, in the same transaction as the state), the last three
  notified by default, and the Domains tab counts the days left (Vercel's "Invalid
  configuration"; `model.rs::domain_diagnosis`, `api.rs::a_custom_domain_routes_once_its_dns_points_at_roll`).
  A domain still unverified 7 days after it was added is removed (`PENDING_CLAIM_TTL`), so
  a squatted hostname comes free for its real owner; one verified once never expires. Removing an
  environment or a service removes its domains (rows and provider records alike). Where the CNAME
  target is shared by every tenant (`cloudflare` below: `up.roll.site`), a CNAME proves nothing
  (a hostname someone already points there, or left pointing, would go to whoever claims it
  first), so `Domain.record` is `both` and only the TXT challenge activates it. A service holds at
  most 50 custom domains and a project 200 (`MAX_DOMAINS_PER_SERVICE`, `_PER_PROJECT`):
  certificates and their orders are shared, and the edge's pass over active hostnames takes turns
  (one past its `max_domains` waits a pass, never loses its certificate). A custom domain can redirect
  to another of the environment's hostnames (`PATCH …/domains/{host} {redirect_to}`, the Domains
  tab): a 308 with the path and query, as Render does for www and apex; it can't point at
  itself, at one that redirects, or redirect while another points at it.
- **Custom domains' certificates** (`Domain.tls`: `none`, `pending` → `issued`, or `failed`, with
  `tls_error`): one seam, `integrations::DomainTls`, chosen by `ROLL_APPS_TLS`:
  - `cloudflare` (the fleet): each domain is a **Cloudflare for SaaS** custom hostname on the site
    zone (`roll_edge::saas`: `POST /zones/{zone}/custom_hostnames`, HTTP DCV, a DV certificate),
    created when roll apps accepts and stores the domain, never before; verifying (and the
    minute loop, until it's issued) reads its state; removing the domain deletes it. Customers'
    CNAME target is the zone's fallback origin `up.roll.site`. Cloudflare issues and renews at its
    edge and connects to the origin with the custom hostname as the server name; the edge answers
    names it has no certificate for with the fallback origin's (`CertStore::set_unknown_name`).
    Answers are read at most 1 MiB, and only `[a-z0-9.-]` hostnames outside roll's zones are sent.
  - `acme` (a node alone, no Cloudflare): the edge orders a certificate per **active** custom
    domain over ACME HTTP-01 (`roll_edge::domains::DomainCerts`, on the `acme` singleton like the
    edge's own sets). It learns the domains through `roll_edge::domains::CustomDomains`, which
    rolld implements over `Apps::active_custom_domains` and `Apps::set_domain_tls` (apps-control
    and roll-edge don't depend on each other). The plain-HTTP listener (`ROLL_EDGE_HTTP`, port 80
    from outside) answers `GET /.well-known/acme-challenge/<token>` for any host from the
    in-memory tokens of the orders in flight, and redirects everything else to HTTPS. Each
    certificate is stored sealed (`system/certs/domains/<host>.sealed`), served by SNI, renewed 30
    days before it expires, and uninstalled and deleted when the domain goes. At most 5 orders a
    pass (every 30 s); a failing hostname waits 5 minutes, doubling to a day. The ACME account
    (`ROLL_ACME_DIRECTORY`) is the one the edge's DNS-01 certificates use.
  - `none` (the default): `tls` is `none`; locally services are plain HTTP on the apps listener.
- **Private networking**: a project's services reach each other at
  `<service>.<env>.internal:<port>` and by bare `<service>`. Locally every project and
  environment has a Docker network `roll-apps-net-<project>-<env>` and each instance joins it
  with its service name as an alias. On the fleet, tasks of one project and environment share a
  security group that admits only itself, and names resolve through Cloud Map (in `## Next`).
- **A private caller wakes a sleeping service** (Railway's private network, Fly's Flycast).
  rolld never sees that traffic, so it reads the instance's own network numbers instead
  (`runtime/netns.rs`, the sandbox's `/proc/<pid>/net/…`, readable by anyone and still while the
  app is frozen). Cheaply: TCP `PassiveOpens` in `snmp` counts the connections the namespace
  accepted (34 µs a read), marked when the instance goes to sleep and asked five times a second.
  The socket tables (`tcp`, `tcp6`) would say who's connected, but for another namespace the
  kernel walks the whole host's table: 5.6 ms a read on ai-server, so the first version, which
  read them five times a second per sleeping instance, would have spent a core on 36 of them.
  They're read now only where nothing else tells: a running instance about to sleep (at most
  every 5 s), and arming a waker (a few reads, off any request's path):
  - Frozen (suspended): a connection in the app's backlog resumes it (about 100 ms more than
    an awake call).
  - Stopped: a waker in the sandbox (`<instance>-wake`, busybox `nc` with no capabilities, a
    read-only root and 16 MiB) listens on the app's port. Accepting the first caller closes its
    listener, so rolld sees a connection with nothing listening and starts the app, and the
    waker joins the held caller to the app once it listens. The first call is answered, not
    refused.
  - Before any other start (a public request, a cron run) the waker lets go of the port: rolld
    calls it once through the published port, which frees it in a millisecond (killing its
    container took Docker 70), and the app's bind never races it.
  - Running: connections from peers other than the host (rolld's own come through the
    network's gateway) count as use when it would otherwise sleep, so a service in use only
    privately doesn't. An idle pooled connection counts too, as on Railway.
  - Measured on real Docker (`a_private_caller_wakes_a_sleeping_service`, a caller on the
    project network with `docker exec … wget`, which is about 90 ms of the time): awake 86–115
    ms, stopped 281–457 ms, frozen 185–214 ms. A public cold start with the waker armed was
    167–210 ms, and five alternating pairs with and without the waker (sandbox medians 165 and
    200 ms against 175 and 212) showed no cost.
  - A sandbox's pid is believed only while its cgroup names the sandbox's container (after a
    Docker restart it may be another process's).
  - Not on the fleet yet (Cloud Map, in `## Next`). A second caller arriving while the app
    starts from stopped is refused (the waker holds one); the app's port must listen on
    loopback too (`0.0.0.0` or `::`, as every generated start command does).
- **roll databases**: a service links to the project's Postgres, Redis, search clusters, buckets,
  streams, queues and NoSQL. A link injects the resource's connection variables (`DATABASE_URL`,
  `REDIS_URL`, `ELASTICSEARCH_URL` and `ELASTICSEARCH_API_KEY` or an OpenSearch-flavor cluster's
  `OPENSEARCH_URL`, `OPENSEARCH_USERNAME` and `OPENSEARCH_PASSWORD`, `S3_*`, `KAFKA_*`, `AMQP_URL`,
  `CASSANDRA_*`), named with the resource's prefix
  when a service links two of a kind (`ORDERS_DATABASE_URL`). A bucket's, a stream's and a search
  cluster's link get credentials of their own, through roll secrets' providers as its managed links
  get them (rolld's `RollResources::link`): a storage key named `roll apps <service>` for that
  bucket only (`S3_BUCKET`, `AWS_ENDPOINT_URL`, `AWS_REGION`, `AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY`), a stream's own SASL user (`KAFKA_BROKERS`, `KAFKA_USERNAME`,
  `KAFKA_PASSWORD`), a search cluster's data-only API key (an internal user with a data-only role
  on an OpenSearch-flavor cluster), never the cluster's password. Each is made once when the link
  is, kept sealed with it, and revoked when the link goes. A NoSQL database, which has no keys of
  its own, gives its contact point and login (`CASSANDRA_*`). Postgres and Redis give their URLs,
  read again at each deploy. A queue's link gets a vhost user of its own through roll secrets'
  `queue` provider (`AMQP_URL` with that user, never the vhost admin's; `permission` configure by
  default), deleted when the link goes, which closes its connections. On one node a URL naming the host's
  loopback reaches it through a relay (below, "Links on one node"); on the fleet they're the public
  edge names over TLS, as previews use, so there's no path from a task into the fleet's network.
- **Service links** (Render's `fromService`, Railway's reference variables): a `service` link
  to another web service of the project gives `<NAME>_URL`, its private
  `http://<name>.<env>.internal:<port>` in the linking environment, and `<NAME>_PUBLIC_URL`
  when it's public there. They're read at each deploy (a renamed service's new name, a new
  port), checked to be the same project each time, and the links to a deleted service go with
  it. The canvas draws them like a database's. A scoped token needs `reveal` on the linked
  service, as on any linked resource.
- **Credentials per link**: a kind whose product can make a credential per consumer does so
  once, when the link is made (`Resources::link`: a roll search API key, a native user for
  OpenSearch), instead of the resource's own password. Its variables are kept sealed with the
  link (AAD names the service, kind and resource) and are what deploys and listings read;
  setting the link's prefix again keeps the same credential. Unlinking, or deleting the service,
  revokes it (`Resources::unlink`; a revoke that fails is logged, and the link is gone either
  way). A link that can't be stored revokes what it just made. Kinds that don't (Postgres and
  Redis today) read the resource's variables fresh at each deploy. A `search` link makes its own: it goes
  through roll secrets' search provider like a bucket's (rolld's `secrets::Search`, through search's
  keys route with `access: "data"`): an API key with a data-only role, or on an OpenSearch-flavor
  cluster an internal user with a data-only role of its own. The keys route runs on the cluster's
  owner on a fleet, wherever the service's owner is. roll.toml declares one as a
  `[databases]` entry with `kind = "search"` (`roll up --all` makes the cluster if it's missing,
  and `roll apps export` writes it).

### Secrets and environments

- **roll secrets** (feature/roll-secrets, docs/secrets.md) owns per-project secrets per
  environment. roll apps reads a deploy's secrets through one port,
  `integrations::SecretSource::resolve(project, env, service)`, and never stores a second copy
  of a project secret (rolld's `secrets::ForApps`; `NoSecrets` stands in where roll secrets
  isn't on).
- Instances' OIDC tokens (`sub = roll-apps:<project>:<service>:<env>`, for clouds' federation
  and roll secrets' workload logins) come from roll secrets' issuer through
  `SecretSource::issue(project, service, env, aud, ttl)`: docs/secrets.md, "roll's tokens for
  other products".
- Service variables per environment (Railway's service variables), sealed at rest, are roll
  apps' own: a service's config (`PORT`, `NODE_ENV`) belongs to the service; project secrets
  come from roll secrets and service variables win on a clash.
- Reference variables (Railway's): a variable's value may hold `${{ VAR }}` (another of the
  environment's variables as it resolves: roll's, a link's, a secret, its own) or `${{ api.VAR }}`
  (another service of the project in the same environment: its own variables, `PORT`,
  `ROLL_PRIVATE_HOST`, `ROLL_PUBLIC_URL`), filled in at each deploy one level deep (`refs.rs`).
  One naming nothing is empty, and the deploy's log says so; one naming a service the project
  doesn't have is refused when it's set. A reference reads another service's values, so setting
  one needs what reading them would (a token's `reveal` on that service); once set, the value is
  part of this service's own (its reveal shows it, as with a link). The listing shows the
  template (names, never a value), and the dashboard edits that, not what it resolves to.
- A variable may name one of roll secrets' keys in its own environment
  (`DATABASE_URL=roll://production/db/URL`). Each deploy, it's read as the service
  (`SecretSource::resolve_ref`, which is roll secrets' `resolve_for` with
  `ResourceRef::new(Kind::Service, id)`, audited as the service) and sealed. It's never kept,
  and it's shown only as roll secrets reveals it. This is how a key gets its own name or comes
  from a folder; it reaches nothing the service's environment doesn't already. Setting one that
  names another environment, or a project (`roll://acme@production/…`), is a 400, and so is one
  that reads as a reference only once trimmed or lowercased (` roll://…`, `ROLL://…`): roll
  secrets' parser trims, so only the exact form is let through, read the same everywhere. Naming
  another environment needs the
  person who set it bound to it (roll secrets' maker references, #407). A reference that doesn't
  resolve fails the deploy, naming the variable. A preview copies its base's variables, so a
  reference there names the base and is left out, as an untrusted preview's secrets are.
  `api.rs::a_roll_reference_variable_reads_its_environments_secret`.
- The variables panel edits one at a time, stages a pasted `.env` for review, copies another
  environment's (as a reveal there), or edits them all as one `.env` text (Railway's raw editor:
  `toEnv` writes what `parseEnv` reads back exactly, quotes and line breaks included; saved as
  the difference, a line removed is unset; links, secrets, roll's own and sealed ones aren't in
  it).
- A reference is completed as it's typed (Railway's): in a value, `${{` offers this service's
  variables and the project's other services, `${{ db.` that service's variables and roll's own
  (`ROLL_PRIVATE_HOST`, `PORT`…); arrows, Enter or Tab take one (`lib/var-refs.ts`). The names
  are read the first time a reference is typed, never a value.
- Secret-looking names are sealed by default (Vercel's sensitive-by-default): a new variable
  named like `*_KEY`, `*TOKEN*`, `*PASSWORD*`, `*SECRET*`, `*_DSN` comes with Seal checked in the
  dashboard (a pasted `.env` too, unless its seal is changed by hand), and `roll apps vars` seals
  such new names unless `--plain NAME` keeps one revealable (one already there keeps what it
  was). The rule is `lib/secret-names.ts` and the CLI's `looks_secret`.
- Sealed variables (Railway's): `PATCH …/variables {seal: [names]}` (`roll apps vars --seal
  NAME`, the lock on a row) seals one of the environment's own variables for good. The app's
  builds and instances get it as before; nothing shows it again: `…/variables/values` leaves it
  out, for every caller, and the listing shows `sealed` and the same eight dots whatever its
  length. A new value keeps it sealed, removing it is the only way out. A reference that reads a
  sealed value (`${{ db.PASS }}`, its own or another service's) is sealed in its own listing, so a
  reference is no way round it. A rollback brings variables back sealed as they were (and keeps
  what's sealed since), and `variables_from` leaves them out (as Railway's copies do: deploying the
copy would read them, and that can take less than deploying where they are). A shell into an instance, or a
  job, runs with them as the app does, which is why those need admin and reveal.
- What the next deploy changes (Railway's staged changes): `GET …/pending` names the
  environment's own variables added, removed or changed since its live deployment (from the
  snapshot sealed at its switch, values never shown) and the run settings the service sets that
  it doesn't run with; the dashboard says them under "needs a deploy", `roll apps pending` as
  `+`/`-`/`~` lines. Across a project (Railway's "Deploy N changes"): every list's environments
  carry `needs_deploy` (cached a minute per environment, as an environment's read is; a node
  that doesn't own the service sees another node's change within the minute), the canvas's cards
  say "changes to deploy", Home counts them per project, and the project's staged-changes bar
  counts them with its new resources; "Review and deploy" opens one sheet naming what changed in
  each environment (not previews), fires each picked one's own redeploy (the same image, no
  build, so permissions stay per service) and says how each went, a failed one offered again.
  `roll apps open` opens the environment's address (`--print`: only prints
  it; Railway's and Vercel's `open`).
- The wiring (rolld's `secrets::ForApps`, on main with #62): a new apps environment that isn't a
  preview is ensured in roll secrets (`SecretSource::ensure`; `qa` is made, the defaults come
  with the project, a preview's `pv-<id>` is previews' to make with its base); apps never
  deletes a secrets environment; a missing one means no secrets; `needs_deploy` compares
  `Secrets::digest` instead of resolving values; `Values.linked` says which addresses to rewrite
  for containers, and `sensitive` keys are sealed in apps' listing. Previews' environments are untrusted in roll secrets itself (an ephemeral
  environment not made `trusted` never reads a protected one), so the port carries no trust flag.
- What a deploy injects, lowest to highest precedence: roll's own (`PORT`,
  `ROLL_SERVICE`, `ROLL_ENVIRONMENT`, `ROLL_DEPLOYMENT`, `ROLL_PUBLIC_URL`,
  `ROLL_PRIVATE_HOST`; for a repo or Git URL service `ROLL_GIT_COMMIT`, the deployment's commit, and
  `ROLL_GIT_BRANCH`, the branch the environment follows, with `ROLL_GIT_URL` for a Git URL; and in each container
  `ROLL_INSTANCE`, its name as roll's log lines give it: Railway's `RAILWAY_GIT_COMMIT_SHA` and
  `RAILWAY_REPLICA_ID`, Heroku's `DYNO`, for an error tracker's release and a log's source),
  links, the environment's project secrets, the service's variables. The commit and branch never
  mark an environment "needs deploy", and the commit stays out of the build's steps and their
  hash (or no build would keep the cache past a push); a tree's own Dockerfile gets it as an
  `ARG ROLL_GIT_COMMIT` it declares, which Docker caches by its value.
- A deployment records the variables' names and a hash of their values: changing a variable
  marks the environment "needs deploy" (like Railway's staged changes; `roll apps vars K=v
  --deploy` saves and redeploys in one step, as Heroku's `config:set` does). When a deployment goes
  live, the environment's own variables are kept with it, sealed under the deployment
  (`deploy_vars`), and a rollback to it brings them back as they were then (Heroku's and
  Railway's rollbacks: a bad variable change is undone by rolling back), the change in the
  activity as `variables.restored`; `keep_variables` (`roll apps rollback --keep-variables`) runs
  its image with today's. Links' and roll's own variables are resolved again, as at any deploy;
  a deployment that never went live (a no-traffic try-out) has none kept.

### Preview environments

The previews thread (feature/roll-previews) drives roll apps through two calls, both on the
public contract so they need no private hook:

- `PUT /v1/apps/services/{id}/environments/{env}` with `{ "branch": null, "ephemeral": true }`
  makes the environment (an ephemeral one is never the target of deploy-on-push and is
  listed apart), then `POST /v1/apps/services/{id}/environments/{env}/deploys` with
  `{ "revision": "<sha>" }` deploys that revision into it.
- `DELETE /v1/apps/services/{id}/environments/{env}` tears it down: instances stopped and
  removed, domains released, variables deleted, deployments kept in history as `removed`.
- Preview environments scale to zero like any other, so an open pull request costs nothing
  until someone opens its URL.

roll previews' feedback button works on a preview environment of roll apps too, when roll
previews turns it on for the project (`feedback` on the environment): a visitor who came in
through the preview's access link (its cookie; an automation's bypass header doesn't count) gets
`<script src="/__roll/feedback.js" defer>` before `</body>` of a page load (a browser's document,
else a GET that asks for HTML), the page asked of the app uncompressed and sent `private,
no-cache` with no length or ETag; the script itself comes from the preview's own host. It holds no
credential: notes go to the preview in roll's dashboard. The button also lists what's been said
about the page (Vercel's comment pins): `GET /__roll/feedback.json?path=<pathname>` on the same
host and for the same visitors, answered from roll previews through a port
(`integrations::FeedbackNotes`, `Apps::set_feedback_notes`) as previews' own proxy answers it:
`no-store`, the latest 200, a 404 where previews isn't on the node and a 502 when it fails (the
button then shows no list). The script and the tagger are roll previews', copied
(`assets/feedback.js`, `feedback.rs`, equal to #28's) until one copy serves both.

### Permissions and MCP

roll apps adds no token or scope checks of its own. Its routes sit behind rolld's gate
(`crates/roll-auth`; scoped tokens and the catalogue in `crates/roll-access` on
feature/roll-mcp), and every route is classifiable from its pattern alone:

- the service id is in the path, and the environment too wherever an action targets one;
- production is the environment name in the path, so `deploy` (deploy, rollback, promote to
  production) is separable from `write` (deploy to a non-production environment) by the route;
- revealing variables is its own route (`…/variables/values`), so `reveal` is separable from
  `read`;
- deleting a service, an environment or a domain is a `DELETE` on its own path.

The product name is `apps`; the actions are `read`, `write`, `deploy`, `delete`, `reveal` and
`admin`. The route table with each route's action is in http-api.md, "Routes and actions".

### On the fleet

- Ownership: lease `apps:service:<id>`. The owner runs the service's deploy queue, waking proxy,
  autoscaler, cron schedule and meter. Any node accepts a request for a service's hostname and
  hands it to the owner with `Cluster::dispatch` (WebSockets included). The forward goes to the
  owner's own route for it, `/internal/apps/public` (`public::with_forwarded`, merged into
  rolld's internal API behind `require_signed`: only a node's signed request gets in), with the
  service, environment and deployment (`x-roll-apps-target`) and the visitor's own path and
  query (`x-roll-apps-uri`) in its headers, as storage's internal routes carry theirs; roll's own
  headers are taken off a visitor's request where it comes in. Never as a call to the API (an
  app's paths aren't the API's), and never trusting a header another product's forward could
  carry: a first version marked public traffic with a plain header that any product's forward
  (S3, deploy hooks, roll code) passed on from a visitor, which would have reached any
  service by id past a private environment's access (an independent review found it before it
  reached main; `a_visitor_cant_reach_a_service_through_the_internal_route` keeps it out).
  Without the route at all, a visitor who reached a node not owning the service got the API's 404
  (`apps-control/tests/cluster.rs`: two nodes in one process, each with its own server and fake
  runtime, sharing a store: a visitor and an API call through the other node, a static site's
  rewrite to a service the other node owns, a killed owner's service woken and deployed on the
  node left once its lease expires, a deploy in flight on a killed owner finishing on the node
  left with the dead one fenced off, and a joining node taking services with no visitor's request
  failing, a static site served by the node that takes it over from its tar in object storage
  with nothing built again, a tunnel opened at a node that doesn't own the service, and a cron
  service's schedule and runs on the node that takes it over). The
  join test found a roll-cluster bug: mid-handoff the joining node still sent a
  service's requests to the old owner, which sent them back, until the hop limit answered 508;
  now a forward that would go straight back to the node that sent it asks the backend instead of
  the node's memory (`Cluster::dispatch`). A node that takes a
  service over starts from the store, and keeps what the previous owner left running of each
  environment's live deployment (its instances' names say their deployment; one that answers is
  taken as running: `EnvLive::adopt` with `Runtime::adopt`, which makes it this process's own,
  its output followed from then, the instance numbers after it), so a node's death or a rolld
  restart costs visitors no cold start. Docker checks the container runs and reads its port (a
  web instance's is its network sandbox's, `<instance>-net`;
  `docker.rs::a_restart_keeps_the_running_container`, frozen too);
  Fargate takes a task `list` found from another process (its newest RUNNING one, its private
  address; any other task of that name stops), so a sleep or a scale-in stops it as any other.
  Only a web service's or a worker's, of the deployment its label or tag names (a cron run's or
  a job's container is `…-run<n>`, never kept); a frozen one is kept asleep with nothing asked of
  it, a TCP service is checked by its port accepting, an HTTP one by an answer; new names start
  past every one seen, and never one this process made (a request that loads the environment
  mid-deploy once removed the deploy's own instance being health-checked). Anything else of the service's (older deployments, jobs, one that doesn't
  answer) is removed, and so is all of it if the environment can't be loaded (tried three times,
  a second apart at most), unless the service moved on meanwhile. On a runtime the nodes share (Fargate), a node giving a service up (a
  handoff, or fenced) lets its instances go untouched (`EnvLive::let_go`; on a runtime it alone runs, `EnvLive::retire`:
  its instances leave rotation and stop once their requests in flight end, not under them. No node
  but the owner ever has live state, and a request with no body that met the service moving is sent
  on to the new owner a moment later; a takeover's sweep spares a deploy this node is running,
  which a new service's lease taken mid-deploy used to remove.
  `cluster.rs::a_joining_node_takes_services_with_no_request_failing` passes 72 parallel runs, was
  1 in 12 failing; `Runtime::release`
  forgets the tasks and stops following their logs, which the next owner reads from its
  adoption on; stops and removes it had already spawned, a drain or a scale-in, don't run), and
  only a service's owner adopts (a request that reached the old node as the service moved is
  refused, not served by taking the instances back), so it never stops what the next owner just kept
  (`a_takeover_keeps_the_instances_the_dead_owner_ran` and
  `a_handoff_on_a_shared_runtime_keeps_the_instances`, two nodes on one runtime;
  `fargate.rs::a_restart_keeps_the_running_task`, on the fake ECS; an independent review found
  the job, handoff, TCP-probe and naming cases). Deploys in flight on a lost
  node fail and are retried once by the new owner (builds are idempotent: same revision, same
  image tag).
- Every catalog write that belongs to a service is a `code_sql` write closure; the switch of an
  environment's active deployment is a compare-and-swap on the previous pointer, so two nodes
  that briefly both think they own a service can't both switch it.
- The store is `code_sql`: SQLite at `<home>/apps.db` on one node, the system database's `apps`
  schema in a cluster.

## Observability and cost

- **Runtime logs**: each instance's stdout and stderr, line by line with the time, stream and
  instance, kept in memory per environment (the last 5,000 lines) and appended to log chunks in
  object storage (`apps/logs/<service>/<env>/<day>/<seq>.ndjson`, kept 7 days). `roll logs -f`
  and the dashboard follow them over server-sent events. They're read back by time (`before=`,
  `since=`: memory, then the stored chunks newest first, one that starts after the window
  skipped by its name) and narrowed to an `instance`, a `deployment` or a `stream`, a search
  (`q=`) included; a route's own narrowing (one cron run's lines, the access log's `status`)
  holds for all of them (`roll apps logs --before 2h --instance 1`; the Logs tab's Read back to
  and Earlier; Railway's, Render's, Cloud Run's).
- **Metrics** per service and environment, per minute: requests, 2xx/4xx/5xx, latency p50/p95/p99
  (a fixed-bucket histogram at the proxy), CPU (cores used), memory (bytes resident), instances,
  cold starts and their duration (from wake to the first byte the instance answers). Kept 7 days
  at minute resolution in the store. The dashboard's charts mark each deployment that went live
  in the window with a dashed rule, listed under them with its commit, message and who (Vercel's
  and Render's deploy markers: did that deploy move the errors or the latency).
  Each instance's own CPU and memory over the last hour (Railway's per replica, Render's, Fly's)
  come with them (`by_instance`): the samples the reaper already takes for metering and
  `cpu_target`, kept by minute on the instance (60 a piece, in the owner's memory, gone with the
  instance), under Metrics as a table of each one's CPU (of its cores) and memory (of its limit)
  now, its peak and its line: one instance pinned while the others idle is a hot key or a stuck
  request, not load. A runtime that can't measure an instance (Fargate) has none.
- **Errors and latency by route** (Vercel's Observability, Render's slowest requests):
  `GET …/routes?minutes=` (5–60) groups the environment's access log by method and route, an
  id-like segment folded as `:id` (a number, a UUID or hash, a generated `usr_8f3k2j9d`; never a
  word, `v2` or `page-2`) and the query dropped: requests, 5xx, 4xx, p50, p95 and the slowest.
  It answers the busiest, the most 5xx and the slowest (5 requests or more), 50 of each, read
  from the newest 50,000 lines (`cut` says when it stopped there) with at most 1,000 routes kept
  apart (the rest one `*`). Nothing new is stored: the proxy already writes the line. Metrics
  shows it under the charts (busiest, most 5xx, slowest); `roll apps routes --sort errors`.
  The same requests come split by the deployment and the instance that answered
  (`deployments`, `instances`; Render's instance breakdown, Railway's per-replica graphs): each
  access line names its deployment, so during a gradual rollout the canary's 5xx and latency
  sit beside the live one's, and Metrics shows "By deployment" and "By instance" when more than
  one answered. CPU and memory stay per environment.
- **Metering**: usage per service and environment per hour: CPU-seconds used, GB-seconds of
  memory resident while running, requests, egress bytes, build minutes. Suspended and stopped
  instances meter nothing.
- **Prices** (roll cloud, per second of real use; the dashboard shows them and the month so
  far):

  | what | price |
  | --- | --- |
  | CPU | $0.000012 per vCPU-second used ($31 per vCPU-month flat out) |
  | memory | $0.000004 per GB-second resident ($10.4 per GB-month) |
  | egress | $0.05 per GB |
  | builds | $0.005 per build minute |
  | requests | free |

  An idle service costs $0.00: it is stopped, and a stopped instance meters nothing. A
  low-traffic site (a request a minute, 50 ms of CPU each at 100 MB) is about 1,440 s of
  running a day if each request keeps it awake 1 s, a few cents a month.
- **What roll pays for idle**: locally nothing but disk (images, stopped containers). On the
  fleet, the image in ECR ($0.10/GB-month) and the warm window's Fargate seconds.

## Errors

An environment's errors, grouped from its log with nothing to install (Cloud Run's Error
Reporting; `errors.rs`): `GET …/errors?minutes=`, the Metrics tab's Errors panel (the newest of
each opens whole), `roll apps errors [--sample]`.

- **A trace is one error.** Each instance's and stream's lines are followed: Python's
  `Traceback (most recent call last):` to its last line (`ZeroDivisionError: …`), a JVM's
  `Exception …` or `com.acme.Oops: …` with its `at`, `Caused by:` and `... N more` lines,
  Node's `TypeError: …` with its `at` lines, Go's `panic:` with its goroutines; another
  instance's lines in between don't break it.
- **The same error is one group:** a trace's error type and first three frames, with line
  numbers, addresses, ids and quoted values taken out (so `line 10` and `line 42` of the same
  bug count together); an error line with no trace (its level `error` or worse; a JSON line's
  `msg`) by its message the same way (`payment 17 failed` and `payment 98 failed`).
- **Each group:** its count, first and last seen, how many instances, and the newest whole as
  its sample (8 KiB). The most frequent first, at most 50, from at most the newest 100,000 lines.

- **A trace is one entry in the log too.** The same joining marks each line as it's written
  (`trace: start` on the first, `more` on the rest from its instance and stream), so the Logs tab
  shows a trace as one entry at its first line, however other instances' lines fall between
  (its lines past the first three folded, and a link to the Errors panel), and `roll apps
  logs` prints its lines together under the first (`┆`, no times of their own; a followed log
  marks them as they come). Drains and stored chunks keep the mark.
- **A new one tells someone** (Sentry's and Error Reporting's new-issue alerts): each minute
  the owner reads what's new in the log, and a fingerprint the environment has never had is an
  `error.new` event, once ever (`error_groups` keeps them, so a restart doesn't repeat one; at
  most five said an environment a minute and 1,000 kept, so an error line whose words vary can't
  flood anyone), for
  notifications (on by default) and webhooks. It names the error's type (`ValueError`, `panic`,
  `an error line`), never its message, as `alert.log` names its pattern and never a line.

`errors.rs`' unit test joins and groups Python, Node, JVM and Go traces and JSON error lines;
`api.rs::errors_group_from_the_environments_log` reads them back over the API, and
`a_new_error_is_an_event_once` sees one event for an error, none for it again, one for another,
and never the card number in its message.

## Log levels

An app's line says its level the way its logger writes it (Cloud Run's severity, Railway's
`@level`): a JSON object's `level`, `severity`, `lvl` or `log.level` (a name, or pino's and
bunyan's numbers), logfmt's `level=`, or an upper-case level word among its first three
(`2026-11-14 10:00:00 ERROR …`, `[WARN]`), or a bracketed one in any case (nginx's `[notice]`,
Apache's `[error]`); prose that mentions an error isn't one. It's read
once, as the line is written, and kept with it (`LogLine.level`, absent when it says none), so a
read costs nothing more; lines written before levels were read have none. `?level=warn` (`roll
logs --level warn`, the dashboard's level filter) keeps warnings and worse, live and back
through the stored days, and the dashboard marks warn, error and fatal lines and colours a line by
its level when it says one (only error and fatal red: an nginx notice on stderr isn't an error),
by its stream when it doesn't.
`an_apps_log_filters_by_the_level_its_lines_say`.

**By a line's fields** (Railway's `@attr:value`, Cloud Run's `jsonPayload`): `?where=level:error
user_id:42` keeps the lines whose fields say all of it, a JSON object's (a dotted key reaches
in: `req.method:POST`) or logfmt's pairs; `key:*` asks only that it's there, `-key:value` that it
isn't that, a quoted value holds spaces, values compare as text in any case (so `user_id:4`
isn't `42`, and `ok:false` matches JSON's boolean). Read back through the stored days and
followed live (`roll apps logs --where …`); at most ten terms (`log_where.rs`). The dashboard's
filter box takes both: its field terms go to `where`, its other words stay a text search, and
the live view matches them the same way (`lib/log-where.ts`).

## A project's logs together

A project's Logs view (beside Canvas and Previews; Railway's Log Explorer) follows every
service's runtime log in one environment and merges them by time, each line named by its
service, with the level and text filters a service's Logs tab has. It reads each service's own
stream (the same route and permission as that tab, so nothing new can read them) and keeps the
newest 2,000 lines on screen; reading back through the stored days stays a service's Logs tab's.
Looked at against a live rolld with two nginx services (2026-11-15), wide and at phone width,
where the time moves into the service's title and lines wrap only where they must.
`roll logs --all-services [-p project]` does the same from a terminal: each service's newest
`--tail` merged by time, or followed together with `-f`, with the filters `roll logs` takes
(`--level`, `--http`, `--since`, `--grep`). `a_projects_services_logs_read_together`.

## Request logs

Every request through the proxy is a line in the environment's access log (`http:<service>/<env>`,
`?http=1` on its logs route, the Logs tab's Requests view, `roll logs --http`), as Railway's HTTP
logs: method, path (no query string: it can carry tokens), status, milliseconds, bytes out, the
instance, the request id (`X-Request-Id`, made when the client sent none) and a cold start's wait.
Every line carries who asked: the client's address (the one the fleet vouches for), its user
agent and the host, which is what an allow list or a firewall rule is written from. An answer
roll gives itself before any instance is a line too, with `by` saying why (`allow_list`,
`firewall`, `rate_limit`, `private`, `maintenance`, `wake`, `capped`, `not_deployed`, `moving`,
`unavailable`), and it's metered: a service that can't wake shows its 503s as errors, so
`alert.errors` fires (`api.rs::requests_are_access_lines_filtered_by_status`). `status` (`500`,
`5xx`, `400-499`), `min_ms` and `by` (one reason, or `any`) filter it, read or followed. It's
apart from the app's own output, so a busy service's requests don't push its logs out of memory.

## Metric and log alerts

Each minute the owner of an environment reads its last five finished minutes against the live
deployment's `run.alerts` (Railway's monitors, Heroku's alerts): memory at or above
`memory_pct` of the limit per instance in every minute (default 90), 5xx answers at or above
`error_pct` of at least 20 requests (default 5), p95 latency at or above `p95_ms` in three of the
minutes (default off). Each is one event (`alert.memory`, `alert.errors`, `alert.latency`) when it
starts, notified like a crash loop, and can fire again once it has cleared. Settings has the
thresholds, as does `roll.toml` `[app.run.alerts]`.

**Metrics past a week** (Cloud Run keeps six weeks): minute rows are kept a week; before they go
they're rolled up into hours in the same transaction (`metrics_hourly`: counts summed, latency,
CPU, memory and instances their peak), kept 90 days. A range past a week (Metrics' 30d and
90d) answers a point an hour (`step: 60`), those hours and the week's minutes grouped
(`metrics_past_a_week_come_from_hourly_rollups`).

**Saturation** (Cloud Run's max-instances warning, Heroku's queue time). At `max_instances` a
request isn't queued: it goes to the least busy instance even past its `concurrency`. Each such
request is counted that minute (`over_capacity` in Metrics' "Over capacity" chart, and
`roll_apps_over_capacity_per_minute` for a scraper). `alert.capacity` fires when at least
`capacity_pct` of five minutes' requests (at least 20) were over capacity. It's on by default at
10%, notified by default, and its message names the instances and concurrency to raise.
`saturation_is_counted_and_alerted` covers it.

A hard limit instead (Cloud Run's, Fly's `hard_limit`): with `run.queue_s` (1–60; Settings'
"Queue at the limit"), a request that finds every instance at its `concurrency` at
`max_instances` waits up to that long for one to finish a request (no polling: each finished
request wakes one waiter). If none does, it gets a 429 ("Every instance of this service is busy")
with `Retry-After` and counts as over capacity, so `alert.capacity` reads it. The wait is in the
access line's `waited_ms`. Below `max_instances` nothing waits: a new instance starts and the
least busy takes the request meanwhile, as without it. Every request the app gets carries
`X-Request-Start` (Heroku's: unix ms when roll took it), so an APM agent shows the time it queued.
`api.rs::a_request_at_the_hard_limit_waits_then_gets_a_429`.

A log alert (`alerts.log_match`, `alerts.log_count`: Settings' "Alert: its log says") is
`alert.log` when its output, stdout and stderr in any case, holds the text that many times
(default 10) in the five minutes, judged with the same minute from the lines still in memory
(nothing stored is read, so a quiet minute costs a scan of at most 5,000 lines). roll's own and
the build's lines don't count (the
alert's own names the pattern), and the event holds the pattern and the count, never a line,
since a line may hold anything the app printed. It's on for new notification targets.

An alert that was firing and isn't (its five minutes under the threshold again) is an
`alert.resolved` event naming it (`data.alert`), said once, and sent to the targets that get that
alert (there's nothing else to ask for). The firing is remembered on the owner, so an alert that
clears after the owner restarts or hands the service over isn't said to (its next firing is).
A target's events and filters change in place (`PATCH …/notifications/{nid}`, `roll apps
notifications --edit ID --events …`, the dashboard's Events), keeping its URL and secret.

## Commit statuses and waiting for CI

A repo deployment reports on its commit as a roll code status (`roll/apps/<service>/<env>`,
mirrored to GitHub with the rest), pending while it's on its way and then success or failure,
linking to the environment, as Railway's and Vercel's GitHub deployments do. An environment with
`report: false` (Settings → the environment → Report deploys) posts none of it, nor a GitHub
Deployment, on any host: roll previews sets it on every `pv-` environment it makes, so a pull
request hears one voice (its `roll/preview` status and comment), as Vercel's does. An environment with
`wait_for_ci` (the Environment panel, `roll apps env --wait-for-ci true`) deploys a push only
once its commit's checks pass (Railway's "Wait for CI", Render's "after CI checks pass"): the
statuses other than roll's deploy statuses (roll apps' own `roll/apps/…`, and roll previews'
`roll/preview`, which is about a preview environment, not the code). With none, it deploys at
once. One that's pending waits (a
newer push takes its place, as it would supersede the deploy; `Environment.waiting_for_ci` names
it), is settled by the landing follower every couple of seconds, and is dropped after an hour. A
failed one isn't deployed, which the environment's log says.

A Git URL service on roll's GitHub App (`PUT …/github`) waits the same way (Railway's "Wait for
CI" on GitHub): its pushes, from the App, its hook or the watch, go into the wait, whose checks
are GitHub's check runs and commit statuses for the commit, read through the installation
(`Code::github_ci`), roll's own (its App's check runs, `roll/…` statuses) left out. GitHub's
checks may not exist yet when the push arrives, so none at all is waited on for a minute before
it counts as none. `api.rs::a_git_url_service_connects_to_rolls_github_app` pushes through the
App with `wait_for_ci`: waiting while checks run, deployed once they pass, dropped when one fails.

GitLab and Bitbucket work the same way with the service's Git login (`ci_hosts.rs`; GitLab is
gitlab.com or a host named `gitlab.…`): its token reads the commit's statuses (GitLab's
pipeline jobs, each one's newest run; Bitbucket's builds), roll's own left out, and roll's deploy
status (`roll/apps/<service>/<env>`, running, then success or failed) is posted there, all
through the client for tenant-chosen URLs (public addresses, no redirects, bounded reads). The
token needs the API (a GitLab personal or project access token with `api`, a Bitbucket app
password with repository and pipeline access), not only Git. `ci_hosts.rs`' tests read and post
against fake GitLab and Bitbucket APIs, each with its token as that host takes it.

## Audit trail

Every change someone makes to a service is an event in the event log, `audit.<what>` with the
actor (as `Deployment.actor`: what rolld's gate saw): settings (the keys a PATCH set), variables
(the names set and removed), a reveal of the values, an environment's settings or deletion,
domains (added, removed, redirected), links and the registry login. Never a value: variables by
name, an access token as "access changed". `GET /v1/apps/services/{id}/activity` and the Activity
panel in Settings read it, newest first; audit events are kept 90 days (the rest of the log 7).

## Incidents

A crash loop's banner on the service page has the logs a click away and **Roll back** to the
deployment that was live before (its image again, no build); `roll apps rollback` with no id
does the same, as `heroku rollback`. The server picks the target (`POST …/rollback`): it follows
the `rollback_of` chain, so after B → A the next goes before A, never forward to B. A failed or
timed-out cron run is a `cron.failed` event,
notified by default; a failed one-off job is `job.failed`, off by default (someone ran it and saw
it). Both carry the exit code.

## Functions in a static site

A static site's functions (Netlify Functions in `netlify/functions/`, Vercel's `api/`) run as a
service of their own beside it, so the site stays files that answer with nothing running. The
site's plan names them (its `functions`), and "Run its functions" on the deployment (or `roll
apps functions`) makes a web service of the same source, named `<site>-fn`, built by the
`functions` builder, and routes `/.netlify/functions/*` and `/api/*` in the site's environment to
it (the environment's routes). That builder writes a Node 22 image with a small server
(`apps_build::functions`) that imports each function once and calls it as its platform does:
- Netlify's `export default (request, context)`: a Web `Request` to a `Response`.
- Netlify's `exports.handler(event, context)`: Lambda's event, an answer of `{statusCode,
  headers, body, isBase64Encoded}`.
- Vercel's `export default (request)`: Web.
- Vercel's `(req, res)`: Node's, with `req.query`, `req.body` (JSON parsed), `res.status()`,
  `res.json()` and `res.send()`.

A function is `<name>.mjs|.js|.cjs|.ts|.mts`, or a directory with an `index` of one. TypeScript
runs as Node strips its types. Its `package.json` dependencies are installed. A request is at most
6 MiB, an unknown function is a 404, and one that throws is a 500 with its error in the log.
Edge functions run in the same service, on Deno ("Edge functions", below).
`docker.rs::a_static_sites_functions_build_and_answer` builds one of each style and calls them.

## Edge functions

A static site's edge functions (Netlify Edge Functions, Vercel's Middleware) run before its files
on the paths they name, to answer themselves, change the request, or change the file's answer:
- Netlify's `netlify/edge-functions/<name>.{ts,js,mjs,mts,tsx,jsx}` (or `<name>/index.*`), on
  their `export const config = { path, excludedPath }` or netlify.toml's `[[edge_functions]]
  function path`, as `export default (request, context)`: `context.next()` (the site's own
  answer, to return or change), `rewrite(path)`, `geo.country.code` (the edge's country), `ip`,
  `cookies`, `json()`, `requestId`, and `Netlify.env`. Nothing returned is the site's answer.
- Vercel's `middleware.{ts,js}` at the root, on every path or its `config.matcher`: a `Response`
  answers, `next()` (`x-middleware-next`, with `x-middleware-request-*` headers set on the
  request) and `rewrite()` (`x-middleware-rewrite`) as `@vercel/edge` makes them.

Paths are patterns: `*` is any rest (`/shop/*`), `:name` one segment, `:name*` any rest, and a
segment may glob (`*.png`). The first function whose patterns take a path (and whose excluded ones
don't) runs; at most 20 functions, 20 paths each. The site's plan lists them (`edge`), and Run its
functions makes the same `<site>-fn` service the site's functions run in; its image adds Deno
(pinned by digest, its binary only) and a server of edge functions on 127.0.0.1, which the Node
server hands `/.roll/edge/<name>` to. The build fetches their imports (`deno cache`: URLs,
`npm:`, Netlify's types), so they start without the network. They run with network, the
environment and reads of their own files (`--allow-net --allow-env --allow-read=/app,/roll`).

A request on an edge function's path goes to the functions service's environment of the same name
as `/.roll/edge/<name>`, past its access gate (the site's let the visitor in), with the visitor's
path in `x-edge-path` and in `x-edge-next` where `context.next()` asks for the site's answer: the
path `/_roll/edge/{service}/{env}/{deployment}/{until}/{sig}` on the apps listener, which the
functions service reaches at its `ROLL_EDGE_BASE` (set at its start, as instances reach the
listener for traces; never an address a request gave it), signed for that deployment for a minute (an HMAC under a key every node shares),
which answers the same deployment's files and never runs an edge function again. Anything
unsigned or expired there is a 404. Without the functions service running, the files answer as
they would, and the environment's log says once which function isn't run.
`api.rs::a_sites_edge_function_runs_before_its_files`, `apps_build::edge::tests`.


## Base image updates

A built service learns when what it builds on gets fixes (Railway's automatic security patches,
Heroku's stack updates). Each build keeps the digests of its Dockerfile's base images (its
`FROM`s, not its own stages, `scratch` or a pinned one; `apps_build::base_images`) as its
registry says them right after the build (the plan's `bases`). Each hour the owner asks the
registries again for every built environment's live deployment (a manifest `HEAD` per image a
pass, no pull): a base that moved is `base_updated` on the environment (with a "Deploy without
the cache" on the service page) and a `deploy.base_updated` event once per new digest (off by
default in notifications). With `run.rebuild_on_base` (Settings' "When a base image is updated",
`roll apps set --rebuild-on-base true`) the same revision is built again without the cache, a
deploy like any other (health-checked, a gradual rollout if it has one). An upload has no
revision to build again, so it only says so. A plan also names a runtime past its end of life,
or within 90 days of it. `api.rs::a_moved_base_image_is_said_and_built_again`.

## Secret values in logs

A secret value never reaches a log (GitHub Actions' masking, Netlify's secret scanning): every
line written to an environment's log, a build's and a job's, and its access log, spans and
metrics (the app's output, a build step's, OTLP records, a request's path, roll's own lines) has the environment's sealed variables, every value roll
secrets gives it (a secret is one whatever its name or tag, but for the linked fields it says
are plain: a host, a port, a user, a database), its links' credentials (by name: `PASSWORD`, `SECRET`, `TOKEN`, `API_KEY` …)
and the password in any URL variable taken out as `***` before it's kept, followed, stored or
drained (`logs::Redact`, one Aho–Corasick pass a line, set as each deployment is made and kept for
the eight before it). A value under six characters is left, and so is each line of a multi-line
value under that; a value split across lines by the app isn't found. Unsealed variables are
shown in Variables anyway and aren't hidden. Not yet: a static site's exported files aren't
scanned for them (Netlify fails a build that would ship one). `logs.rs::secret_values_are_taken_out_of_a_logs_lines`
and `api.rs::a_sealed_value_never_reaches_the_log`.

## Variables as files

A variable can be kept as a file in its instances instead of in their environment (Render's
Secret Files, Fly's `[[files]]`, Cloud Run's secret volumes): `files: {NAME: "/etc/secrets/key.json"}`
on `PATCH …/variables` (the Variables tab's file button, `roll apps vars --as-file
NAME=/etc/secrets/key.json`). From the next deploy the Docker runtime writes it there (0444, so the
app reads it whatever user it runs as; its folders made) before the instance first starts, and it
isn't in the environment. It's how a credentials JSON, a client certificate or an `.npmrc` gets
in, and a value may hold newlines, which the environment's own file can't. A sealed variable stays
sealed as a file. Fargate refuses them for now. Builds get them as files too: each install and
build step of a generated Dockerfile mounts one at its path as a BuildKit secret
(`--mount=type=secret,id=NPMRC,target=/root/.npmrc`), so a private registry's `.npmrc` or a
`pip.conf` works and is never in a layer, the image's history or the cache's keys (a path with a
character a mount can't name, a comma say, is in the step's environment instead). A tree's own
Dockerfile mounts it itself: every variable is a BuildKit secret by its name
(`RUN --mount=type=secret,id=NPMRC,target=/root/.npmrc npm ci`). `api.rs::a_variable_kept_as_a_file_reaches_the_instance_there` (the paths checked,
out of the environment, back, dropped with the variable) and
`docker.rs::a_variable_kept_as_a_file_is_in_the_container` (the app serves the file it reads).

## Holding deploys

`held: true` on an environment (the environment panel's "Hold deploys", `roll apps env --hold`)
is a person's hold, as Netlify's locked deploys and Vercel's production builds that aren't
assigned to the domain are: pushes, CI waits, deploy hooks, the Git watch and tag moves still
build, but to `ready` (`no_traffic`: built and health-checked beside what's live, on their own
URL), and a person puts one live from Deployments ("Put live", a rollback to it, which keeps the
hold). A service that can't run beside the live one (a worker, a volume) isn't deployed while
held, the log saying why (a hook's answer is a 409). Deploying by hand works as ever. `held:
false` (or "Resume deploys") ends it, as it ends a rollback's hold; a rollback never replaces a
person's hold with its own. `api.rs::a_rollback_holds_the_next_push_until_deploys_resume` holds
production, pushes (ready, not live), puts it live (still held) and resumes.

## Paused by the clock

An environment's `pause_schedule: {windows, timezone?}` (Railway's app sleeping, a scheduled
scale to zero; for staging at night and over weekends) pauses it as one of its weekly windows
starts (`{from: "fri 19:00", to: "mon 07:00"}`, as a freeze's, in its time zone) and resumes it
as the window ends: paused exactly as `PUT {paused: true}` pauses ("Pausing": nothing runs,
wakes, deploys or is scheduled; roll's 503 "Paused" page meanwhile). Only what the schedule
paused is resumed by it: a person's pause or resume inside a window holds until that window
ends (`sched-paused` in the store's state, so another node's pass sees the same), and the next
window pauses again. Checked each cron pass (every ten seconds) on the owner.
`api.rs::a_pause_schedule_pauses_and_resumes_an_environment`.

## Deploy freezes

An environment's `freeze: {until?, windows?, timezone?, reason?}` (GitLab's deploy freeze
periods, a change window; the environment panel's "Freeze deploys", the service page's notice
with "Lift the freeze") stops what goes live without stopping work: while it's on, pushes, tag
moves, deploy hooks and the Git watch build to `ready` beside the live deployment, exactly as a
person's hold ("Holding deploys") does (a service that can't wait beside the live one isn't
deployed, the log saying why), and a person's deploy, redeploy or promotion is a 409 that says
the reason, unless it carries the header `X-Roll-Override: 1`, which goes through and is written
in the audit log (`audit.freeze.overridden`, with who). The override is `admin` in the permissions
catalogue (MCP's review: a freeze is the team saying nothing goes live now, and a token that may
only deploy can't go past it), so it's a header the gate reads, never a body field (a body's
`override` is a 400). The CLI says it as `--override`. Rolling back to what was live is
never frozen: it's the way out of a bad deploy. Putting live a build that never was (one held at
`ready`, freeze or not) is a deploy, so it needs the override too. A base image's rebuild
(`rebuild_on_base`) builds to `ready` as a push does. The secrets watch's redeploy waits for the
freeze to end: the change counts as unseen until then, so the first pass after it redeploys. It's on until
lifted (`freeze: null`) when it names no `until` and no `windows`; before `until` (unix ms); or in
any of its weekly `windows` (`{from: "fri 17:00", to: "mon 09:00"}`, wrapping past Sunday, at
most 14) in its `timezone` (IANA, UTC by default). `frozen` says whether it's on now. From the
CLI: `roll apps env --freeze "the launch"`, `--unfreeze`, `roll apps redeploy --override`,
`roll apps rollback <id> --override`, and `roll deploy`/`roll up --override`; any setting the
environment takes goes as JSON with `roll apps env --set FIELD=JSON` (a window's freeze,
`mirror`, `pause_schedule`, `heartbeats`, `checks`).
`api.rs::a_deploy_freeze_holds_deploys_but_never_a_rollback`, `freeze::tests`.

## Gradual rollouts

`run.rollout` (Vercel's rolling releases, Cloud Run's traffic splits): `steps` are increasing
percents (`[10, 50]`, at most five), each lasting `step_s` (default 120). After the new
deployment's instances pass their health check, they join the old ones instead of replacing
them, and the proxy sends each step's share of requests to them by turns (request *n* goes to
the new one when `(n × 37) mod 100 < share`, so each hundred requests sends exactly that many,
spread out). The proxy counts each deployment's answers and 5xx while a canary runs. The
rollout reads them every quarter of a step (5 s at most), and a step fails when the new
deployment answered `error_pct` (default 5) or more of at least 20 requests with a 5xx and more
often than the old one did, or, with `run.slo`, spent the objective's error budget
`rollout.slo_burn` (default 14.4, the fast burn alert's) times as fast as it lasts (Argo
Rollouts' analysis: a 99.9% objective fails a step at 1.44% of 5xx, well before the 5% rule;
`judge_tests`), or, with `rollout.p95_pct`, answered more slowly than the old one by more
than that many percent at the 95th percentile (Flagger's request duration check: each
deployment keeps a histogram of the proxy's answer times, to the answer's headers, in steps of
about 19% from 1 ms, its p95 read between a bucket's ends; each side needs 20 requests; a
mirrored copy is never judged). An outage both share, like a database that's down, isn't the new
code's fault. What no answered request shows counts too: a request routed to the new deployment
whose wake failed is one of its 5xx, and two or more crashes of its instances (a failed health
check, a process that exits) roll it back when the old one crashed less. The wake backoff is per
deployment, so a canary that won't start never holds back the old one's wakes, and each
deployment's instances are health-checked on its own path. A failure rolls it back: every request goes to the old deployment again, the new
one's instances drain, and the deploy fails with the numbers ("rolled back at 10% of requests:
14 of its 40 requests were 5xx…"). After the last step, the store switches the active
deployment (`switch_active`, the same compare-and-swap) and the old instances drain. Canceling
the deploy, or Roll back on the service page, does the same as a failed step. The environment
shows `canary` (deployment and share) while one runs, and each step is in the deployment's
steps (`canary_10`). A visitor stays on one deployment through a step (Vercel's rolling
releases): while a canary runs, the proxy gives a visitor without one a `roll_bucket` cookie
(0–99, a day) and sends buckets under the share to the new deployment; the cookie never reaches
the app. A client that keeps no cookies is routed by turns, as above.

With `rollout.manual` (Vercel's manual rolling releases, Cloud Run's held split), a step that
passes holds its split instead of moving on: the environment's `canary` says `waiting`, the
service page shows Next step, Put it all live and Roll back, and `roll apps rollout
advance|all|abort` does the same (`POST …/deploys/{dep}/rollout`, audited as
`rollout.decided`). It's still judged while it waits, so a deployment that starts failing at
10% rolls back by itself whether or not anyone is looking. Advancing past the last step puts it
live. A held rollout is a running deploy: the environment's next deploy queues behind it until
it's decided or canceled. The wait is in the owning node's memory; a takeover fails the deploy,
as it does any rollout, and every request stays on the old deployment.

A rollout splits requests, so it needs a web service whose old deployment is running, outside
a preview environment (a reviewer or two is nothing to judge a canary by). The first
deployment, an environment that's asleep, a worker and a cron service switch at once, and a
service with a volume refuses `steps` (one instance writes it). The state lives on the owner.
If the owner changes mid-rollout, the new owner runs the deploy again from the start, like any
other deploy it takes over.

## A project in one file

A root `roll.toml` can declare a whole project (Render's Blueprints), next to whatever else is in
it:

```toml
[apps.web]
root = "apps/web"          # uploaded by roll up; its own roll.toml [app] still applies
links = ["db", "cache", "uploads", "jobs"]  # databases below, and another app (its URLs)
[apps.proxy]
image = "nginx:1.27"
port = 80                  # and [apps.<name>.variables]: both set when the service is made
[apps.jobs]
repo = "acme/shop"         # a roll code repo: deploys on each landing on `branch`
root = "jobs"              # with repo or git: the directory in it that builds
branch = "main"            # (`git = "https://github.com/acme/shop"`: a public Git URL instead)
type = "worker"            # web, worker or cron (with `schedule`), `start` too
[apps.jobs.run]
min_instances = 2          # any run setting, checked as the API checks them
[apps.jobs.environments.staging]   # besides production: a branch, its own size, variables
branch = "develop"
run = { max_instances = 1 }
variables = { LOG_LEVEL = "debug" }        # set when it's made, as they are
[apps.jobs.variables]
NODE_ENV = "production"                    # set as it is
SESSION_SECRET = { generate = "secret" }   # 32 random bytes in hex, made once when it's made
STRIPE_KEY = { ask = "Your Stripe secret key", default = "sk_test_…" }  # asked of whoever makes it
[databases.db]
kind = "postgres"
[databases.cache]
kind = "redis"
[databases.uploads]
kind = "bucket"            # postgres, redis, bucket, stream, nosql or search
```

`[databases]` declares what a service links by name: Postgres and Redis give their URLs, a
bucket and a stream credentials of their own, a NoSQL database its login. A link may also name
another app of the file, for its private and public URLs (Render's `fromService`); those are
made after every app is, so the order doesn't matter, and a name can't be both an app and a
database. An app's other environments (`[apps.<name>.environments.<env>]`, never
`production`: that's the app's own settings) are made with it, or brought to the file's branch
and run settings when it's read again; their variables are set only when they're made, and
export writes them back without values. Import, export, the dashboard's "From a file" and `roll up --all` all read them
(`project.rs::a_file_links_buckets_streams_and_other_apps`).

**A "deploy to roll" link** (Heroku's button and `app.json`, Render's `generateValue`,
Railway's templates): `/dashboard/deploy?repo=owner/name` reads that repo's root `roll.toml`
from roll code (`POST /v1/apps/import {repo, revision?}`, its default branch's head), makes a
new project of yours and applies the file with New service's import: its databases, services and
links, a `generate = "secret"` variable made in the browser once, an `ask` one asked on the page
(its default offered; Apply waits for every answer). A repo's README can carry the link. `roll up
--all` does the same from a checkout: it makes the secrets and asks the questions on the
terminal (without one, an asked variable stops it, named). A service already there keeps its
variables. Reading a repo this way needs read on it, as planning from it does. `?git=https://github.com/owner/name`
does the same for any public Git repository with no app and no mirror (Railway's and Render's
buttons for a GitHub repo): rolld resolves its branch, fetches the head shallowly without any
blob over 64 KiB (`git_source::read_root_file`), reads its root `roll.toml`, and an app that
names only a folder becomes a Git URL service at that folder, following that branch
(`a_public_repos_roll_toml_imports_by_url`). The same holds for a roll code repo's link, whose
folders are now built from the repo instead of waiting for `roll up`. The secrets made
for a new service, and any variable whose name reads as a secret (`*_KEY`, `*TOKEN*`,
`*PASSWORD*`, `*SECRET*`), are sealed from the start (`sealed` in `POST /v1/apps/services`):
whoever deploys a file never sees them again, and nor does anyone who later reads its settings.

**Starter templates** (Railway's, Render's and Vercel's galleries) are project files in the
dashboard (`apps/dashboard/src/lib/templates/*.toml`), each on a public image: n8n and Uptime Kuma
on a volume, and Umami, Directus and Metabase on a roll Postgres database. Their variables point
at what roll gives them (`${{ DATABASE_URL }}`, `${{ ROLL_PUBLIC_URL }}`). They're offered on an
empty project's canvas and in New service's File tab, and listed at `/dashboard/deploy`.
`/dashboard/deploy?template=umami` makes a new project from one, and `&project=<id>` applies it to
a project you have. The page shows the file before anything is made, and applies it with the same
import as a repo's (`ImportPane`), so the generated secrets and asked values work the same way.
`every_starter_template_imports` reads each template through `/v1/apps/import` and checks its run
settings as creation does, so a template can't drift from what the API takes. Two of them stay
up (`min_instances = 1`): n8n's schedules and Uptime Kuma's checks run only while it's awake.

`roll up --all` (the project is `--project` or the directory's name, made if missing) prints the
plan first (`+` made, `=` there, `~` linked, `↑` deployed; `--dry-run` stops there). A database
counts as there only when it's on the project's canvas (another project's `db` is never this
one's), and a service the file names with another kind of source stops it (renaming one is the
fix, never a quiet conversion). Then it makes the missing databases and places them on the project's canvas, makes the missing services with
their links so the first deploy has the variables, links what an existing service lacks,
uploads each `root`, and redeploys an image service whose image changed. Names are matched, so
running it again only does what changed, including the run settings an entry names (`~
settings`, only those that differ; the rest stay as they are). On a push there's nothing more
to sync: a repo service's own `roll.toml` `[app]` (in its root) applies over its settings at
every deploy, and every landing deploys, which is what Render's Blueprint sync does for an
existing service. Making new services and databases needs a project and other products, so
that stays `roll up --all`'s. Nothing is ever deleted: a service or database removed
from the file stays until removed by hand. The file is checked before anything is made: one of
`root` or `image`, a root inside the directory, links to declared databases, kinds `postgres`
or `redis`. `roll apps import-compose [file] [--write]` turns a Compose file into these entries
(Railway's Compose import): image and build services become `[apps]` (the container's port,
its environment as variables), Postgres and Redis containers become roll databases linked from
what `depends_on` them, and what isn't carried over (volumes, networks, commands, health checks)
is said. The dashboard does the same from New service's "A file" (Railway's templates, Render's
Blueprints): rolld reads the file (`POST /v1/apps/import`, the parser `roll up --all` uses, in
`apps_build::project`; nothing is made), the dashboard plans it against the project with the
same lines (`+`, `=`, `~`, `↑`, and `!` for a service of another kind of source, which stops it),
and Apply makes the databases (placed on the canvas), then the services with their links,
settings, variables and branch (a repo service is made following it, so its first deploy is
that branch's head), the usual calls a person makes. Its databases are named by their products'
listings, never a cache that may have failed (which would read as none and make a second `db`);
an existing service's other repo or root is said (`·`), not changed; a new image is deployed
before the service is set to it, so a refused deploy is still in the plan when the file is read
again. A Compose file is read on the server, so its aliases are counted as they'd expand before
any value is built: past 100,000 nodes it's refused (a few KB of YAML aliases can be gigabytes). A `root` service is made as an
upload service: the browser can't send a directory, so `roll up --all` uploads it. A Compose
file's roll.toml can be saved from there. The plan is tested in `import.test.ts`, the route in
`a_compose_file_is_read_as_a_project_without_making_anything`. The other way (Fly's `fly config save`):
`POST …/projects/{project}/export` (`roll apps export [-o roll.toml]`, Project settings →
Services → Download roll.toml) writes the project as that file, its services' sources and
branches, the run settings that differ from a new service's, and the postgres and redis
databases they link (named by the caller, who lists those products; roll apps keeps their ids).
A value is never written: variables go by name in the file's header, with what a file doesn't
declare (other links, an upload service's directory). It's parsed back before it's answered, so
what's written is what `roll up --all` and the import read.
`a_project_is_written_as_roll_toml_that_reads_back`. The plan, the checks and the import are unit-tested in roll-cli and apps-build, and the whole run was
checked against a live rolld (apps and Redis on Docker, 2026-11-13): the plan, a Redis database
made and placed, the image service made with its link, port and variables and deployed, and a
second run finding everything there. The calls are the API's routes
as documented.

### From another platform

`POST /v1/apps/import` also reads another platform's file as roll.toml (Render's Heroku
migration, Railway's and Koyeb's imports; `apps-build/src/imports.rs`), and the dashboard's "From
a file" takes it by its name: Render's `render.yaml` (web, private, worker and cron services,
`keyvalue`/`redis` and `databases` as roll's own, `fromDatabase` as a link, `generateValue` and
`sync: false` as variables generated or asked for), `fly.toml` (each of its processes a
service, its build image, env, internal port, `min_machines_running`, release command, VM
memory), `railway.json` or `railway.toml` (start, pre-deploy, health path, replicas, cron,
restart policy) and Heroku's `app.json` with its Procfile (a service per process, `release` the
web one's pre-deploy, Heroku Postgres and Redis as roll's databases linked from every service,
`generator: secret` and required variables). What doesn't map is a note (build commands, roll
detects builds; disks and mounts, set in Settings; env groups, roll secrets hold shared values),
and the file is checked as `roll up --all` checks it before it's answered. The converters' unit
test reads one of each; `api.rs::a_compose_file_is_read_as_a_project_without_making_anything`
reads a Render Blueprint over the API.

## Comparing two environments

Settings → an environment → Compare (Railway's environment sync; `roll apps env-diff --from
staging`) shows what an environment doesn't share with another: its own variables missing, extra
or set to another value (by name, never a value, and a sealed one can't be compared), and each
setting that differs (`run.*` scale, the CI and tag switches, traces and vitals, egress, the allow
list, rate limit, firewall, headers, routes, and its own pages). Each is made the other's through
the usual routes: a setting with the `PUT …/environments/{env}` body the diff gives, variables
with `PATCH …/variables {copy_from, copy}` (never a sealed one: set it by hand); `--sync` does
all of them. A scoped token needs read on the other environment, and is told which values differ
only with reveal on both (a value set and compared would tell the other's); `copy_from` needs
reveal there, as `variables_from` does. Not compared: the branch, access, domains and what's
state (paused, maintenance, holds). The flow is `two_environments_are_compared_and_synced`.

## Image optimization

Every environment's host answers `/__roll/image?url=/hero.jpg&w=640&q=75` (Vercel's and
Netlify's image CDN, Next.js's `/_next/image`, which a Next.js app points at it with a custom
loader): the image at that path of the environment's own, resized to `w` (one of 16, 32, 48, 64,
96, 128, 256, 384, 640, 750, 828, 1080, 1200, 1920, 2048 and 2500; never larger than it is) at
quality `q` (20–100, 75 by default), as WebP when the browser takes it (`Vary: Accept`) and
otherwise in its own format, or as `f=avif`, `webp`, `jpeg` or `png`. It's roll storage's engine
(`s3_image`: JPEG, PNG, GIF and WebP in; EXIF orientation applied, colour converted to sRGB,
metadata dropped; inputs over 25 MiB or 50 megapixels refused before decoding). The source is
fetched in process as the visitor's own request for it would be (through the environment's
proxy with their headers), so its access, allow list, firewall and rate limit hold, and a static
site's files and an app's answers both work; only a path of its own, never a URL elsewhere, and
only an answer that says it's an image. What's made is kept on the node (64 MiB, the newest
kept; `x-roll-image: hit`), keyed by the environment, its live deployment, the path and the plan,
so a deploy never answers an old file's image; it carries the source's own `Cache-Control`, or a
minute. At most two transforms run at once on a node. `roll_up_deploys_an_uploaded_tree_and_cron_runs_on_demand`.

## A project's preview domain

A project can have a domain of its own for its previews (Vercel's preview deployment suffix), so
a preview answers at `web-pr12.preview.acme.dev` and cookies on the team's domain and OAuth
redirect allowlists work: `POST /v1/apps/projects/{project}/preview-domain {hostname:
"*.preview.acme.dev"}` (the project's Settings → Apps → Preview domain). It's a wildcard verified
as a custom domain is (the CNAME to its own target, `previews-<project>.<domain>`, or the
`_roll-challenge` TXT), one a project, and no other project's or service's domain. Each preview
environment answers to its `preview_host`, one DNS label unique in the project, which roll
previews sets on the `pv-` environment it makes; `host_target` maps `<label>.<base>` to it once
the domain is active, after any exact or wildcard custom domain. Any other label under it goes,
in process and with its Host as it came, to roll previews' router (`Apps::set_preview_router`,
which rolld fills: previews on roll's own runtime, from roll.toml's `[preview]`), so the wildcard
points at one place, roll apps' ingress; without the router it's a 404. Plain HTTP for now: a wildcard's
certificate needs DNS-01, which isn't issued here yet. The flow is
`a_preview_environment_answers_under_its_projects_preview_domain`.

## A custom domain on another port

A custom domain can go to another port of the app than the service's own (Railway's target
port, Fly's services): an admin UI on 9000 beside the app on 8080, on `admin.example.com`
(Settings → Domains → On the app's port, `PATCH …/domains/{host} {port}`, `roll apps domains
port admin.example.com 9000`). From the next deploy each instance publishes that port too, on
loopback as it does its own (`InstanceSpec::ports`; a sandbox's port bindings, the main one
labelled `sh.roll.apps.port` so it's never mistaken for another), and the proxy sends that host's
requests to the same instance there (`Inst::port_addr`). Until a deploy publishes it the host
answers 502 rather than the main port. The service's own names, health checks, wakes and
scaling stay on its port: a request to the other port wakes the instance as any does. A closed
environment's instances are reached through their sandbox's socket on their own port only (409).
The flow is `a_custom_domain_reaches_another_port_of_the_app`.

## Deleting and restoring a service

A delete can be undone for 48 hours (Railway's). `DELETE …/services/{id}` (Settings → Danger zone,
`roll apps delete`) cancels the service's pending deploys, removes its instances and hides it:
its hostnames, custom domains and hooks answer nothing, every route of it is a 404, and no sweep
(schedules, prunes, domain checks, resume on a new owner) touches it. What it needs to come back
stays: its environments, volumes, images, backups, domains, links and their credentials, and its
name (creating another of that name says to restore it or delete it for good). The project's
Settings → Apps lists it under Recently deleted (`GET /v1/apps/services?deleted=true`,
`roll apps restore --list`); Restore (`POST …/services/{id}/restore`, `roll apps restore -s
<name>`) clears the mark, places its card on the canvas again and loads its environments' live
deployments as a new owner would (a token needs `deploy` for it: production runs again). Its activity says who deleted and restored it. A deleted
service's delete (Delete now) deletes it for good at once; otherwise the prune sweep does 48
hours after the delete, on its owner (a node takes the lease of one nobody holds): everything
the old delete removed goes then, its volume after a last backup kept a week. The flow is
`a_deleted_service_is_restored_or_goes_for_good`.

## Duplicating a service

Settings → Duplicate (Railway's; `roll apps duplicate <name>`) makes a new service in the project, beside the original on the
canvas, with its source, build and run settings, links, and production's own variables (a
reference's template as written). It's made with the calls a person can make, so it copies only
what that person can read: sealed variables are never shown again, so they're named for the copy
to set, and other environments, domains and a registry login stay with the original. A repo or
image copy deploys at once, as any new one does. The settings round trip is
`a_service_is_made_again_from_its_own_settings`; what's copied, `duplicate.test.ts` and
`settings_through_the_cli`.

## A service and its database

Railway's and Render's templates make an app and its database together. roll does it without a
template: the plan reads what the code talks to (`Plan.uses`, from the root's manifests: `pg`,
`psycopg`, `pgx`, `sqlx` with `postgres`, `org.postgresql`, and `redis`, `ioredis`, `sidekiq`,
`go-redis`… in package.json, requirements.txt, pyproject.toml, Pipfile, Gemfile, go.mod,
Cargo.toml/Cargo.lock, composer.json and Maven/Gradle files; only `dependencies` in
package.json, since a dev dependency is usually a test's), with the reason in the notes. New
service ticks the project's one database of that kind, or a New Postgres/New Redis chip when the
project has none (with two or more, the user picks), and stops guessing once the user touches a
chip. On Deploy the sheet makes each new database in its product (`roll db`, `roll redis`),
places it beside the service on the canvas, and creates the service linked to it, so
`DATABASE_URL`/`REDIS_URL` are in the first deploy. A database made before a failed create stays
in the project, and a retry links it rather than making another. The first `roll up` in a
directory prints the same finding with the two commands that do it.

## Deploy hooks

An environment can have a deploy hook (Render's): a secret URL that deploys it with no API
token, for CI or a registry's webhook (`POST …/hook`, "Deploy hook" in the environment panel,
`roll apps hook`). Only the token's hash is kept, and a new one replaces the old. A POST to it
deploys the service's own source as a manual deploy would (actor `deploy hook`, and an
`audit.hook.deployed` entry with the caller's address, since the API's gate never sees it); for an image
service, `?image=` may name another tag or digest of the same repository
(`docker.io/library/nginx` stays nginx), never another image, so a leaked URL can only redeploy
what the service already runs. Any token that isn't the live hook's is the same 404. The token
is `rlk_`, 34 base32 characters and a CRC-32, as roll's other secrets, so secret scanning can
find a leaked URL. Making or removing one is admin in roll MCP's catalogue: it's a standing
credential. rolld's gate lets `/v1/apps/hooks` through to the hook's own check (its `RULES`);
access lines log the route pattern, never the token; it runs on the service's owner.

A Git URL service's hook is also its repository's push webhook (Render's and Railway's
"deploy on push" for a repo roll has no app on). Only a push counts: the platform's event header
(`X-GitHub-Event: push`, `X-Gitlab-Event: Push Hook`, Bitbucket's `X-Event-Key: repo:push`)
names it, a tag push, another event or a body that isn't a push is `{ ignored }`, an empty body is
a plain deploy, and GitHub's default form encoding (`payload=`) is read as its JSON is. An
environment that follows no branch deploys no push. GitHub's, GitLab's or Bitbucket's push posted to
the URL deploys the pushed commit at once to every environment that follows the pushed branch
(one repository webhook for all of them, as Render's and Railway's: a push to `develop` posted to
production's hook deploys staging when staging follows `develop`), once the repository's refs
(`ls-refs`) say the branch is at that commit now, with the commit's subject as the deploy's
message. The message is the commit's own, read from the repository (the body's only when that
read fails), so `[skip ci]` is the author's and a body can't drop it; the refs are read once and
roll.toml once per commit, whatever number of environments follow. Each environment's own pause,
rollback hold, skip tag and watch paths apply; the answer is the hook's own environment's deploy
when it made one, and a deploy that failed with none made is the answer's error (the platform
shows a failed delivery). The body alone never names what deploys: a leaked URL can't put an old commit, or a
fork's (which GitHub serves through the parent's URL), live, and a delivery a newer push overtook
is ignored. Each branch a Bitbucket push moved counts. It's keyed by the commit, as the five-minute watch's deploy is,
so a redelivery or the watch makes no second deploy. A branch no environment follows (without
roll previews on the node), a deleted branch, a `[skip ci]` commit and GitHub's ping are 200
`{ ignored }`. The URL's token is the secret
(GitHub's webhook signature isn't needed on top), and the environment panel says where to add it.

A push to a branch none of the service's environments follows is roll previews' when it's on the
node (`Apps::set_git_pushes`, `integrations::GitPush`): previews makes that branch's `pv-…`
environment from its project's rules (or moves it to the new commit, or tears it down for a
deleted branch), deploying it with `{ revision }` and branched links as it does a roll code
change's. Before anything is handed on, the repository's refs (`ls-refs`) must agree: the pushed
commit is where the branch is now, and a deleted branch is one it no longer has, so a body posted
to a leaked hook URL can't name another commit, and pushes come only from the repository's
writers (a fork can't push to it). The hook answers `{ ignored }` either way.

Pull requests reach roll previews the same way (`GitPush::pull`, a `GitPull`): GitHub's
`pull_request` events (opened, synchronize, reopened, closed) and GitLab's `Merge Request Hook`
(open, update, reopen, close, merge) posted to the hook carry the number, the title (untrusted
text, at most 200 characters), the page (made from the service's own URL, never the event's link)
whether it's a fork's (the head repository's id against the base's), and who opened it (GitHub's
`pull_request.user.login`; GitLab's `user.username` only when that actor is the opener,
`author_id`, since GitLab names whoever acted, so none on another person's push to it: untrusted
text, at most 100 characters, for roll previews' `skip_authors` like `*[bot]`, never for trust).
Every push handed on, a branch's or a pull request's, also says who pushed or acted (`pusher`:
GitHub's and Gitea's `sender.login`, GitLab's `user_username` or `user.username`, Bitbucket's
`actor.nickname`; the same untrusted text), so previews can skip a bot's branch before its pull
request comes. A same-repository one
keeps its branch, so it's the branch's preview with the number on it; a fork's is `pull/<N>`, and
roll previews runs it only once a writer approves that head. Its head is checked against the
repository first, as a push's is: a same-repository one's against its branch, a fork's against
`refs/pull/N/head` (GitLab: `refs/merge-requests/N/head`), so a body can't name another commit,
and calling a fork's commit a branch's fails that check. A close is handed on unchecked (the head
ref stays after it): at worst a forged one takes a preview down, as a forged branch deletion would.
Pull requests go only to a receiver that says it reads them (`GitPushes::takes_pulls`, false by
default), so one that dropped `pull` can never take a fork's head for a branch's and trust it; a
same-repository one from a branch an environment follows is that environment's, not a preview.

A preview (an ephemeral environment) never gets a resource link's production variables: roll
previews sets the branched ones itself, by name, and a link it doesn't branch gives nothing there
(a bucket's key, a search cluster's API key, an OpenSearch login). Another service's URLs stay.

## GitHub Actions

`sdk/apps-github-action` (no dependencies, like roll secrets' action) deploys a service from a
workflow after its tests pass (Fly's and Render's deploy actions): a repo or Git URL service
builds the workflow's commit, an upload service is sent what's committed under `path` (`git
archive`, so untracked, ignored or built files never go: a private repository without the GitHub
App), an image service deploys a tag the workflow pushed. It waits for the deployment, gives
`deployment-id`, `status` and `url`, and on a pull request can keep one comment saying where it's
deployed. It's keyed by the run and attempt (`Idempotency-Key`), so a retried step makes no second
deploy. A failed build's lines stay in roll unless `show-logs` is on, because a public repository's job
log is public. A fork's pull request gets no secrets from GitHub, so it has no token and is told why.
With `client-id` instead of `token`, the job signs in with its own OIDC token as a trusted machine
client (docs/permissions.md, "Machine clients"; `roll token client create … --github owner/repo`):
nothing stored in the repository, the token roll gives it short-lived and masked first, and a
refusal says which claim didn't match.
Tests: `node --test sdk/apps-github-action/test.js` (a fake rolld, GitHub API and OIDC endpoint, a
real `git archive`).

It's how a Git URL service waits for CI: its environment's branch unset (so neither a push nor
the five-minute watch deploys), and the action in a job after the tests (`needs: test`). Reading
GitHub's checks on the hook instead doesn't work without an app: a repository webhook gets one
`check_suite` per app or workflow, with nothing saying which is the last, and asking GitHub for
the rest of a public repository's checks is unauthenticated and rate-limited per address.

## Terraform

`sdk/terraform-provider-roll` (roll secrets' provider) has roll apps' resources too
(Render's, Heroku's and Vercel's providers): `roll_app_service` (a name, an image, a roll code
repo or a Git URL, its root and branch, its type, port, start command and size, and a private
repository's `git_username` and sensitive `git_token`, never read back; importable by id), `roll_app_variable` (sensitive, `sealed` replaces it, never read back) and `roll_app_domain`
(its `target` to point a CNAME at; importable as `svc_…/<env>/<host>`), `roll_app_environment`
(another environment: its branch, wait for CI, variables copied from another when it's made;
`svc_…/<name>`), `roll_app_link` (a resource's connection variables; the prefix changes in place
and keeps the credential; `svc_…/<kind>/<ref>`), `roll_app_notification` (a webhook, Slack,
Discord or email target and its filters; the URL is never read back, a webhook's secret is kept from
when it's made; `prj_…/<id>`), and the data source `roll_app_service` (by id, or by name in a
project: its URLs). `TestAppsAsCode` drives Terraform against a fake API: create, change in
place, import, destroy.

## An image built on your machine

`roll apps deploy --local-image web:dev` (Fly's `--local-only`, Heroku's `container:push`) runs
`docker save` here and uploads the archive (`--image-file` takes one already saved, gzipped or
not); the API takes it as `application/x-docker-image`, up to the upload limit. Loading an
archive applies the names inside it on the host, so before it's loaded `image_upload.rs` takes
them out: `manifest.json`'s `RepoTags` emptied, `repositories` left out, the OCI index's name
annotations removed, and entries that climb out or aren't files refused. The image is then
tagged `roll-apps-<service>:<deployment>` as a build's is, and the deploy goes on as any other
(health-checked, a rollout, rollbacks run it again). `docker.rs::an_uploaded_image_runs_and_names_nothing_on_the_host`
uploads an archive whose tag it deleted first and checks the tag never comes back. On the
fleet no node loads it: the archive goes to the bucket in a zip and a CodeBuild run of its own
(`codebuild::load_spec`: log in, `docker load`, tag, push; nothing else of the uploader's) pushes
it to the service's ECR repository, and the deployment runs it by digest
(`fargate.rs::an_uploaded_image_is_pushed_by_codebuild`).

## Static sites

A build whose plan serves files (busybox httpd on `/site`: a plain `index.html`, Vite, CRA,
Astro without an adapter…) runs no instance (Render's and Vercel's static sites). After the
build, the runtime exports the image's `/site` as a tar (`Runtime::export_files`: a `docker run
--network none --entrypoint tar`, its exit checked) once per image, kept on the node's disk
under `sites/` and in object storage for the node that takes the service over, keyed by the
image's digest (or a built image's tag hash), so a rollback or a promotion reuses it. The owner's
proxy answers from the tar directly: an index of its regular files (name → offset and length)
read once per node, byte ranges read per request. Nothing is extracted, so a tenant's archive
can't place a symlink or a `..` path anywhere: anything that isn't a regular file with plain
path components is left out of the index. GET and HEAD; a file, a directory's `index.html`, a page's `<path>.html` (`/about` answers
`about.html`, as Next's static export and Astro's `build.format: "file"` write pages, and counts
as there for `_redirects`; `/about/` doesn't, its relative links would point inside it), a
single-page app's index for a page path, or the site's `404.html` with a 404; ETags (304s),
`no-cache` on HTML, a year (`immutable`) on fingerprinted files and five minutes on the rest,
`nosniff`. A file is fingerprinted when its name changes with its bytes: in a build's asset
directory (Vite's `assets/`, Astro's `_astro/`, CRA's `static/js|css|media/`) with a hash of 8
to 64 letters and digits as a segment of its name (`index-BxR2a9Kd.js`, `main.8f3a2b1c.css`), or
anything under Next's `_next/static/` (`sites::fingerprinted`). A site's `_headers` can set its
own `Cache-Control` over it. Requests and bytes are metered and
access-logged as any (instance `site`), and nothing else is: no CPU or memory, no cold start.
A gradual rollout splits by bucket as usual, and a deployment's own URL serves its files too.
A start command set in the service, a site over 512 MiB or 100,000 files, or a runtime that
can't export (the fleet's CodeBuild builder, until its first run) means a container serves it,
as before. A service's sites are deleted with its images.


A site's own `_redirects` and `_headers` (Netlify's and Cloudflare Pages' files and syntax) are
read when its files load, never served themselves (`site_rules.rs`). `_redirects`: `/from /to
[status]`, `*` and `:splat`, `:name` segments, 301 by default; 301–308 redirect (the query kept
unless `to` has one; another site's URL allowed), 200 serves another of its files where none is
at the path (`/* /index.html 200` never hides a real asset), 404 answers one with a 404; the
first match wins, at most 2,000 rules. `_headers`: a path line, then indented `Name: value`
lines; every matching block adds its headers over roll's own (`Cache-Control` for one), at most
100 blocks, the response's framing (`Content-Length`, `Transfer-Encoding`, hop-by-hop) roll's.
Each file is at most 64 KiB; a line that doesn't parse is skipped, as theirs are. Conditions after
the status (Netlify's): `Country=us,ca` (the edge's `CF-IPCountry`), `Language=en` (the browser's
`Accept-Language`, `fr` taking `fr-CH`) and `Cookie=name` (one of them sent); each given must hold
for the rule to match, and a rule on something roll doesn't know here (`Role=`, Netlify Identity's)
never does. They're routing, not a gate: a firewall rule keeps people out.

A 200 to another site's URL proxies it (Netlify's 200 proxies, Vercel's external rewrites):
`/api/* https://api.example.com/v2/:splat 200` fetches `https://api.example.com/v2/users?x=1` for
`/api/users?x=1`, as roll fetches any URL a tenant chose (public addresses only, no redirects
followed: a 3xx goes back to the visitor; HTTPS, plain HTTP only where private targets are
allowed). The visitor's method, headers and body (at most 10 MiB) go on, without roll's own
(`x-roll-*`, its `roll_…` cookies, hop-by-hop headers) and with `X-Forwarded-Host`/`-Proto`; the
answer comes back as it came (at most 20 MiB, read whole: metered as egress, in the access log as
`external`). The rule's host is its own: a placeholder in it (`https://:host/`) isn't taken, so a
visitor can't make the site a proxy to anywhere. `a_static_site_proxies_to_another_site`,
`site_rules::tests`.

A site moved from Vercel or Netlify keeps its rules (`apps-build/src/site_configs.rs`): at plan
time the tree's `netlify.toml` (`[[redirects]]` from, to, status; `[[headers]]` for and values)
and `vercel.json` (`redirects` with `permanent` 308/307 or `statusCode`, `rewrites` as 200s,
`headers`; `:name` segments, and `:name*`, `:name+` or `(.*)` at the end as the rest, `$1` and
`:name*` in a destination as `:splat`) become `_redirects` and `_headers` lines appended in the
image, after the site's own files (whose rules come first, as Netlify reads them). The configs
themselves never go in, and a site built from its tree's root, which has them, never serves them
(`netlify.toml` can hold build settings). What those lines can't say (a regular expression in a
path, `has` other than a cookie, `missing`, Netlify's conditions other than Country, Language
and Cookie, its query or signing, `trailingSlash`, `routes`) is left out, each named in the build's
notes; `has: [{type: "cookie", key}]` and Netlify's `conditions` become the lines' conditions. `docker.rs::a_sites_vercel_json_and_netlify_toml_rules_answer`
checks a redirect, a rewrite and a header answering, and neither config served.

A rewrite may send a path to one of the project's own services (Netlify's and Vercel's proxy
rewrites, only ever to the project's own): `/api/* service:api/v1/:splat 200` sends `/api/users?x=1`
to the `api` web service in the same environment at `/v1/users?x=1`, its method, headers and body
as they came, on whichever node owns it, so a single-page app calls its API on its own origin with
no CORS. It goes through that service's own gates (its access, allow list, firewall, rate limit and
maintenance) and wakes it as any request does. A private target is reachable this way: its visitors passed
the site's own access, and the project's own rule sends them on (a visitor who claims to be a
rewrite never is: roll's rewrite headers are taken off where a request comes in). On a fleet the
request goes to the target's owner as any public request does, to its internal route, naming it,
with the rewritten path, routed as a fresh request for the target's lease (the visitor's address
kept); the decision is made before the site reads its gradual-rollout and affinity
cookies, so the target reads them. A path with a `.` or `..` segment is refused; a request a
rewrite sent is never rewritten again (no loops); a name the project has no web service for is a
502, said in the site's log.

Text-like files (HTML, CSS, scripts, JSON, XML, SVG, WebAssembly, TTF/OTF, icons) are sent
compressed to a client whose `Accept-Encoding` takes it, as Render's and Vercel's are: the
build's own `<file>.br` or `<file>.gz` next to the file when there is one (Vite's and Astro's
compression plugins), else gzipped here (level 6) for files from 1 KiB to 8 MiB, once per file
and node (a site never changes; up to 32 MiB of a site's gzipped copies are kept). Each
encoding has its own ETag, and `Vary: Accept-Encoding` goes on every such file.

A page loaded before a deploy keeps working after it (Vercel's skew protection, for static
sites): a fingerprinted file the live site doesn't have, which is what a lazy-loaded chunk of the
old build is, is served from the environment's last three static deployments before the live one
(`Apps::earlier_site_file`) for a day after it went live, where Vite's "failed to fetch
dynamically imported module" would otherwise follow every deploy. It's looked for where the live
site would answer a 404 or a rewrite (a single-page app's `/* /index.html 200` included), never
past a `_redirects` rule that hides the path (a 404 rule) or redirects it; never from a deployment
rolled back from; cached five minutes, not a year; each earlier site loaded once at a time (the
others wait for it), and one that doesn't load is tried again after a minute. Fingerprinted means
a bundler's kind of file (scripts, styles, maps, WebAssembly, JSON, fonts, images), so a document
or a download with digits in its name is never one.

### Skew protection

A web service can have the same (`run.skew_protection: true`; Settings' "A page loaded before a
deploy"). A request that names a deployment goes to that deployment's own instance, woken as its
own URL would wake it, when the deployment is one of the environment's last three live ones before
the live one and the live one went live less than a day ago. This is how a single-page app loaded
before a deploy keeps calling the API it was built against, and how a framework's client router
keeps getting its own build's chunks (Vercel's skew protection). A request names its deployment in
one of three ways:
- the `x-deployment-id` header;
- a `dpl` query parameter (`/app.js?dpl=dep_…`);
- the `__roll_dpl` cookie, which every page load gets (HttpOnly, the deployment that answered it).

The app knows its own id as `ROLL_DEPLOYMENT`, for its asset URLs and fetches. A page load
(`Sec-Fetch-Dest: document`, else `Accept: text/html`) always gets what's live, with a new cookie,
so a reload moves a visitor on. Every gate is read before any of this: the allow list, firewall,
password, maintenance and pause. A deployment rolled back from, a pruned one, and one that was
never this environment's live one are never reached this way. What's live answers instead.
`api.rs::a_request_naming_an_earlier_deployment_goes_to_it`, `skew::tests`.

A file sent as it is takes one byte range (`Range: bytes=…`, `Accept-Ranges: bytes`), so Safari
plays a site's video and a download resumes: a 206 with `Content-Range`, a 416 past the end,
and the whole file for several ranges or an `If-Range` that names another version.

## Forms

A static site's forms take submissions with no backend (Netlify Forms): a `<form>` marked
`data-roll-form`, or Netlify's `data-netlify="true"` or `netlify`, with a `name`, is found when
the site's files are read (the first 500 HTML files, each at most 256 KiB; at most 50 forms). A
POST to its `action` (a path on the site) or to the page it's on, urlencoded and at most 64 KiB,
is kept for the environment: a `form-name` field names the form when more than one could take
it. At most 50 fields (100-character names, 10,000-character values; a repeated field's values
joined), sealed with the deployment key and bound to their row, the newest 1,000 a form, and
nothing about the person who sent it (no address, no user agent). A form's honeypot
(`data-roll-honeypot`, or Netlify's `netlify-honeypot`) filled in is answered as a success and
dropped. More than 10 a minute from one sender, or 600 an environment, is a 429; a body with
files a 415, a bigger one a 413. A POST that's no form's (no form posts to that path and its
body names none) is answered as before, a 405; so is any POST to a deployment's own URL, so only
what's live fills the environment's submissions. The forms are found with a small tag reader that
takes quotes as a browser does (a word in another attribute's value isn't an attribute). The sender goes to the form's `action` when the
site has that page (303), else roll's own thanks page. Each kept one is `form.submitted` (its
form, its id and its fields' names, never their values; a notification target asks for it).
What was sent comes back only through `GET …/forms/{form}/submissions`, a reveal, each read an
`audit.forms.read`; the Events tab's Forms panel shows them and deletes one (`audit.forms.deleted`).
Deleting the environment or the service deletes its submissions. `a_static_sites_form_keeps_its_submissions`, `forms::tests`.

## Traces

An app with an OpenTelemetry SDK sends its traces to roll with no setup (Vercel's Observability,
Cloud Run's Cloud Trace, Heroku's OpenTelemetry): `traces: true` on an environment (`PUT
…/environments/{env}`, the environment panel's Traces switch) sets `OTEL_EXPORTER_OTLP_ENDPOINT`
(this environment's address, with a token of its own, kept sealed: `trace_ingest`),
`OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, `OTEL_TRACES_EXPORTER=otlp`,
`OTEL_METRICS_EXPORTER=otlp`, `OTEL_LOGS_EXPORTER=none` (logs only when the app sets `otlp`
itself: most apps also print them, and the environment's log would hold each twice) and
`OTEL_SERVICE_NAME` from the next deploy (the app's own variables win). The endpoint is a sealed
variable: its token is in no answer, the instances still get it. The address is the apps
listener's, public traffic's, as the runtime reaches it from an instance (`ROLL_APPS_OTLP_URL` on a
fleet: the public apps address), never the API's: a relay serves what it reaches to every
container on the node, and the API's loopback is open. `POST
/_roll/otlp/{id}/{env}/{token}/v1/traces` on that listener (any host) takes OTLP/HTTP, protobuf (read by a small bounded
reader of its own, `otlp.rs`: the fields roll keeps, every length checked against the bytes
present) or JSON, gzip or not, at most 8 MiB; the token is the credential, as a deploy hook's
(another is the same 404), and it runs on the service's owner. Spans are kept as the
environment's `trace:` log (a JSON line a span: ids, name, service, times, error, up to 32
attributes of 256 characters), with the log's retention. `GET …/traces?minutes=&min_ms=` lists
recent traces (root span, duration, span count, any error), `GET …/traces/{trace}` one trace's
spans; the Metrics tab shows them with a waterfall, and `roll apps traces [--min-ms N] [--trace
ID]` prints them.

Logs and metrics come in the same way (`…/v1/logs`, `…/v1/metrics`, the same token; Heroku's and
Cloud Run's). A log record becomes a line of the environment's own log, as JSON (`level` from its
severity, `msg`, `trace_id` and `span_id`, its attributes), so the log's level and field filters
and its drains read it, and a line opens its trace. A metric's data points (gauges, sums,
histograms; at most 8 attributes each, which name its series) are the environment's `metric:`
log. `GET …/app-metrics?minutes=`, the Metrics tab's App metrics panel and `roll apps app-metrics`
show each series (at most 50) as at most 60 points: a gauge's last value, a cumulative counter's
rate a second (a reset starts again), a histogram's mean. `api.rs::traces_sent_by_instances_read_back`
sends a log record (read back by its level, with its trace id and attribute) and two counter
points (a rate of 1 a second).

The proxy adds a span of its own to each request's trace (Cloud Run's), at once when traces go
on, with no SDK needed for it: it continues the client's W3C `traceparent` (or starts a trace),
sends the app a `traceparent` naming its span, so the app's spans are its children, and records
`<METHOD> <route>` (service `roll`: method, path, status, the instance, `roll.cold_start`) from
the request's arrival to its answer, with a `roll wake` child for a cold start's wait. A slow
first request then shows that it was the wake. `api.rs::traces_sent_by_instances_read_back`
follows a cold request through it.

## Web Vitals

How fast an environment's pages are for real visitors (Vercel's Speed Insights, Netlify's Real
User Metrics), with nothing to install: `vitals: true` (the environment panel's Web Vitals,
`roll apps env --vitals true`) has the proxy add `<script src="/__roll/vitals.js" defer>` to
its HTML pages, as it adds roll previews' feedback button (`vitals.rs`, `assets/vitals.js`). The
script is the browser's own `PerformanceObserver`: LCP, INP (the slowest interaction), CLS (the
largest session window), FCP and TTFB, sent once when the page is hidden to `/__roll/vitals` on
the same host. No cookie, no id, no third party.

- **Kept checked.** A beacon is at most 2 KiB; only its path (as a route, `/products/:id`) and
  numbers within bounds are kept, as a line of the environment's `vitals:` log with the edge's
  country; 600 a minute an environment, then a 429.
- **The page stays the app's.** The tag goes into a 200 HTML page as it streams (the app is
  asked for it uncompressed); its caching is the app's (a shared cache still keeps it, its ETag
  made weak since the bytes changed), and a cached page gets the tag too.
- **Read back** as each metric's p75 rated by web.dev's thresholds (good, needs improvement,
  poor), overall and for the 20 busiest routes: `GET …/vitals?minutes=`, the Metrics tab's Speed
  panel, `roll apps vitals`.

`api.rs::web_vitals_from_visitors_read_back_as_p75s`: off, nothing is tagged; on, pages carry the
tag, the script is served, beacons read back as p75s by route, a beacon with nothing measured is
refused; off again, the paths are the app's.

## Uptime checks

An environment's `checks` (Cloud Monitoring's uptime checks, Render's and Fly's HTTP checks;
`PUT …/environments/{env} {checks: [{name, path, every_s?, timeout_s?, status?, contains?}]}`,
the environment's Settings → Uptime checks): at most five, each a GET of its own `path` every
`every_s` (30–3600, default 60) through the environment's own proxy on its owner, as a visitor's
request goes but past its gates (allow list, firewall, rate limit, access), as a path schedule's
call is, with `roll-uptime/1.0` as its user agent. It passes on `status` (any 2xx or 3xx when it
says none) and, with `contains`, an answer whose first 64 KiB holds the text. Two failures in a row
are `check.down` (its name, path, status or why there was none), once until it passes again; the
next pass is `alert.resolved` for it. Both go to notification targets (`check.down` is in the
defaults), and on-call ones keep an incident per check. A check is a request: it wakes a service
that scales to zero and keeps it from sleeping as often as it runs, so a sleepy service's check
runs rarely or not at all. Not run in a preview, a paused environment or one in maintenance. How
each stands (`check_states`: up, failures in a row, the last status and time, why it failed, since
when it's down) is in memory on the owner, so a takeover starts it over.
`an_uptime_check_goes_down_and_comes_back`, `checks::tests`.

## Heartbeats

An environment's `heartbeats` (Healthchecks', Cronitor's dead man's switches; `PUT
…/environments/{env} {heartbeats: [{name, every_s, grace_s?}]}`, Settings → Heartbeats) watch
things outside roll: a backup on another machine, a job in someone else's scheduler, a device.
Each pings `https://<the environment's host>/__roll/heartbeat/<token>` (GET or POST, `curl -fsS`
at the end of the job) at least every `every_s` (a minute to a week); one that doesn't within
that and `grace_s` (default five minutes) of its last ping, or of being added, is
`heartbeat.missed` once, and the next ping is `alert.resolved` for it. Both go to notification
targets (`heartbeat.missed` is in the defaults), and on-call ones keep an incident per heartbeat.
At most ten. A token is the heartbeat's only credential: shown once, in the answer to the PUT that
adds it (`heartbeat_urls`, a credential in the permissions catalogue: a token that may only write
or deploy adds the heartbeat but doesn't see it), only its SHA-256 kept, and kept by name when the
list is put again.
So a ping gets in where visitors don't (an access token, signing in), but still past the
environment's allow list, firewall and rate limit, and wakes nothing: an idle service stays idle.
Pings and misses are kept in the store's state, so a takeover keeps them and two nodes' passes
say a miss once. A paused environment's heartbeats aren't judged. `heartbeats_ping_and_go_missed`.

## Vulnerability scanning

Each deployment's image is scanned for known vulnerabilities (Cloud Run's Artifact Analysis,
Render's and Railway's image scans) by the builder, once it's built, pulled or loaded: beside the
rollout, so a deploy is no slower, or before it with `run.block_on` (below). The live deployment
of every environment is scanned again each day on its owner, as advisories come out for the same
packages. The counts by severity are on the deployment (`vulnerabilities`), the findings at
`…/deploys/{dep}/vulnerabilities` (the most severe first, then those with a fix; at most 2,000;
`roll apps deploys --vulnerabilities DEP [--severity high]`), and both on the Deployments tab,
beside the build log. Advisories at high or worse that the
deployment's last scan, else the environment's deployment before, didn't have are
`deploy.vulnerable` (not in the default events: every base image has some on its first scan).

The scanner is Trivy, pinned by digest (`runtime/scan.rs`), on one node's Docker: the image is
saved to a 0700 directory and read as an archive by a container with no network, as rolld's own
user, read-only with every capability dropped (2 GiB, 2 CPUs, 256 processes, its report the one
file it writes, read up to 64 MiB). Nothing in the image runs: its package databases and lockfiles
are a parser's input. Its database is a volume (`roll-apps-trivy-db`) only the download writes,
at most twice a day; when a download fails, the one from before goes on (tried again in ten
minutes), and a scan with none fails (kept as its `error`). One scan at a time per node; a scan
takes a few seconds (4 s for `node:22-slim`, 330 MB). Fargate's builder (CodeBuild) doesn't scan
yet.

`run.block_on: "critical" | "high" | "medium" | "low"` (Settings' "Known vulnerabilities",
`roll apps set --block-on critical`) holds
a deploy whose image has one that severe or worse: the scan runs before the rollout and the deploy
fails with how many, the live deployment serving on; one that can't be scanned (a builder that
doesn't scan, a scan that didn't finish) fails too, since a hold that can't look holds. A
rollback is never held: it's the way out of a bad deploy. Changing `block_on` is a deploy in the
permissions catalogue, as any run key that decides what production runs.
The same run lists every package it found and converts that to an SBOM (CycloneDX JSON; Docker
Scout's, GitHub's dependency graph): `GET …/deploys/{dep}/sbom` (and the Deployments tab's
"SBOM" download), kept with the scan up to 8 MiB and dropped with the findings when the
deployment is pruned; `sbom: true` on the scan says it's there.
`api.rs::images_are_scanned_and_block_on_holds_a_deploy`, `scan::tests`.

## SLOs

A web service's `run.slo: {objective, latency_ms?, window_days?}` (Cloud Monitoring's SLOs;
Settings' "Objective", roll.toml's `[app.run] slo`) says what share of its requests must succeed,
not 5xx, over `window_days` (1–60, default 28), and with `latency_ms` what share of its minutes
with requests must keep p95 under it (a window-based indicator: the proxy's minutes keep
percentiles, not each request's time). What may fail is the error budget. `GET
…/environments/{env}/slo` and the Metrics tab's Error budget panel show each indicator's share
and the budget left over the window: the last week from minutes, before that from hours (an
hour's latency fails whole when its slowest minute did, the safe side).

Its alert is the SRE workbook's multiwindow, multi-burn-rate one: the budget burning 14.4 times
as fast as the window allows (2% of a 30-day budget in an hour) over the last hour and its last
five minutes, or 6 times over six hours and their last half hour, is `alert.slo` (the indicator,
the rate, which window), judged each minute with the other alerts, once until it slows, then
`alert.resolved`. The short window is what makes it stop soon after the burn does; an indicator
needs 20 requests (or 3 minutes with requests) in it to say anything. It's in the default events,
and an on-call incident is opened as for `alert.errors`. Changing `slo` is a write in the
permissions catalogue: it only changes how the service is watched.
`api.rs::an_slo_shows_its_budget_and_a_fast_burn_alerts`, `slo::tests`.

## Split tests

An environment's `split: {environment, percent}` (Netlify's split testing; Settings → Split test)
serves `percent` (1–99) of its visitors from another environment of the same service: production
against a `beta` environment that follows another branch, say. A visitor is kept on one side by
the `roll_bucket` cookie a gradual rollout already uses (0–99, a day; taken off before the app
sees cookies): the split takes the top buckets, a rollout's canary the bottom ones, so the two
only overlap past 100 together. The other environment answers past its own access gate (this
environment's let the visitor in; its own firewall and rate limit still apply), its requests in
its own access log and Metrics, so each side's errors, latency, visitors and custom events read
apart. roll's own scheduled calls and a deployment's own URL never split, nor does a request
another environment sent (no chains). A static site on the other side skips its own `service:`
rewrites, as any rewritten request does. `api.rs::a_split_test_serves_its_share_from_another_environment`.

## Traffic mirroring

An environment's `mirror: {environment, percent}` (Envoy's request mirroring, a split test's
quiet cousin) copies `percent` (1–100) of its GET and HEAD requests to another environment of
the same service, past that environment's gates, and drops the copy's answer (30 s at most), so a
new version sees production's real reads without a visitor seeing it: its errors, latency and
logs read apart in its own access log and Metrics. The visitor's request goes on untouched and
first. Nothing with a body is copied (a write must never happen twice), nor an upgrade, roll's
own scheduled call, a deployment's own URL or a request another environment sent. A copy is
marked as roll's own (`X-Roll-Mirror`, taken off any visitor's request), so the target counts
it as no page view and its rollouts never judge it. A node has at most 64 copies in flight; past
that, a request isn't copied, so a slow or sleeping target never piles them up.
`api.rs::a_mirror_copies_requests_to_another_environment`.

## Snippets

An environment's `snippets: [{at: "head" | "body_end", html}]` (Netlify's snippet injection;
the environment panel's "Snippets") puts HTML of its own in every page the proxy answers, so an
analytics, consent or chat tag needs no code change and no deploy. A `head` snippet goes
before `</head>`; a page without one gets none. A `body_end` snippet goes before `</body>`, or
at the end of a page without one. Pages are what Web Vitals' tag goes in: a browser's page load
answered 200 with uncompressed HTML, whether static or the app's. Nothing else is touched, and
the app's ETag turns weak since the bytes changed. At most five, each up to 16 KiB. They're the
owner's own text, written as given, and setting them on production is a deploy-level change, as
any production setting is. A change takes effect on the next request (the environment's gate
is read again).
`api.rs::an_environments_snippets_go_in_every_page`, `feedback::tests`.

## The app's Prometheus endpoint

An app that already exposes Prometheus metrics doesn't need an OpenTelemetry SDK: `run.metrics:
{port?, path?}` (Fly's `[metrics]`, imported from fly.toml; Settings' "Metrics endpoint") has the
owner GET `path` (default `/metrics`) on `port` (the app's own by default; a worker names one) of
each running instance every 15 seconds. A suspended or sleeping instance isn't scraped, so a
scrape never wakes anything or keeps it up. The text format is read with bounds (4 MiB an answer,
2,000 series a scrape, 16 labels a series, names and values cut): counters (and `_total` names
with no type) are rates, gauges values, histograms and summaries their sum and count (a mean;
buckets and quantiles aren't kept). Each series gets an `instance` label. They go where an SDK's
OTLP metrics go, so the Metrics tab's app metrics panel and `…/app-metrics` show both alike. The
metrics port is published as a custom domain's is, never in a closed environment. A scrape that
fails is said once in the environment's log, until one works. Changing `metrics` is a write: it
only changes how the service is watched. `api.rs::an_apps_prometheus_endpoint_is_scraped`,
`prom::tests`.

Counts of its own log lines are metrics too (Cloud Logging's log-based metrics): `run.log_metrics:
[{name, pattern}]` counts, each minute with the other alerts, the lines of its output (stdout and
stderr, never roll's) that hold `pattern` (any case) and keeps the count as a gauge named `name`
(labelled `source=log`) beside what it scrapes or its SDK sends, so the app metrics panel shows it
and a worker's `scale` can follow it. At most ten; Settings' "Metrics from its log" takes them as
`name: pattern` lines. `api.rs::matching_log_lines_are_a_metric`.

## Status pages

A project's status page (Better Stack's, Instatus'; the project's roll apps settings → Status
page, `PUT /v1/apps/projects/{project}/status-page {title?, items?}`) is a public page of its
uptime checks and heartbeats on a name of its own, `status-<slug>.<domain>`: each one up or down
now, its uptime and its last 90 days by day (a day with a failed check or a missed heartbeat
red, one with only passes or pings the accent, one with neither empty), and `/status.json` with
the same. Without `items` it shows every check and heartbeat of the project's environments (never
a preview's); with them, those (at most 50). Nothing private is on it: a service's and a check's
or heartbeat's names only, never a path, an address, a status code, an error or a log line. The
days are counted as each check runs and each heartbeat pings or is missed (`status_days`, kept 90
days, gone with the service), and whether each is up or down now is kept beside them, so every
node answers the same. A check reads down only once it has failed twice in a row, as its
`check.down` does, never after one failed try. A changed page keeps its name; taking it down
frees it and deletes its incidents. Answers are cached by browsers and the edge for 30 s. The node
keeps each page's rows and incidents for 10 s, forgotten at any change, so a crowd of visitors
reads the store once; a path that isn't the page, `/status.json` or `/feed.xml` reads nothing.
`api.rs::a_status_page_shows_the_projects_checks`.

Incidents are on it too. A check going down or a heartbeat missed opens one ("web · home is
down", or "web (staging) · home is down" outside production, investigating, one open per check or
heartbeat however often it's said), only for what the page shows (its items, never a preview's).
Coming back closes it, even on a node that took the service over: "up" is read against what the
store last said, not the node's memory. A minute's sweep closes the ones nothing will close any
more ("It's no longer checked."): the check or heartbeat removed or off the page, its environment
paused or in maintenance, its service deleted. A person posts their own (`POST …/status-page/incidents {title, state?, text}`) and
updates it (`…/incidents/{id}/updates {state, text}`: investigating, identified, monitoring,
resolved). The page lists the open ones and the last 90 days' with their updates, `/status.json`
has them, and `/feed.xml` is an Atom feed of every update, newest first, for readers and chat
apps that follow feeds. Text is text: never markup on the page or in the feed. An incident takes
100 updates; a resolve always goes through. Resolved incidents go after 90 days.
From the CLI, `roll apps status-page` shows the page and its incidents. `--title` and
`--item web/production/check:home` make or change it, and `--delete` takes it down.
`--incident "Slow checkouts" --text …` posts one, and `--update inc_… --state resolved --text …`
adds an update.
`api.rs::status_page_incidents_open_and_close`, `roll-cli/tests/apps.rs::freezes_settings_and_a_status_page_through_the_cli`.

## Visitors

Visitors and page views (Vercel's Web Analytics, Netlify's, Plausible's way: no script to add, no
cookie, nothing kept about a person) come from the access log the proxy already writes:
`GET …/environments/{env}/analytics?minutes=` (an hour to a week, default a day), `roll apps
analytics`, and the Metrics tab's Visitors panel. A page view is a browser's page load
(`Sec-Fetch-Dest: document`, else `Accept: text/html`) answered 2xx or 304, cached answers
included; an API's calls aren't. A visitor is an address and user agent on one day, counted in
memory for the answer and never stored. The access line gains `page`, `ref` (the referring site's
host when it's another site: never the referrer's path, which can carry tokens) and `country`
(the edge's `CF-IPCountry`, on the fleet). Top pages, referrers and countries come with a series
(5-minute, hourly or 6-hourly points). An answer reads at most the newest 200,000 requests (`cut`).

Campaigns, devices and events (Vercel Web Analytics', Plausible's): a page view that came with
`utm_source`, `utm_medium` or `utm_campaign` keeps those three on its access line (lowercased, at
most 100 characters each; nothing else of the query, which can carry tokens), counted as
`campaigns` with their views and visitors. `devices` reads the user agent already on the line as a
browser, an operating system and a class (`mobile`, `tablet`, `desktop`, or `bot` for crawlers,
`curl` and headless browsers), by the tokens browsers send, no version list kept. Custom events:
a page posts `{name, props?, path?}` to `/__roll/event` on its own host, or loads
`/__roll/track.js` and calls `roll.track("Signup", {plan: "pro"})` (a beacon, or a keepalive
fetch). An event is a name (1–100 characters) and at most ten properties (a-z, 0-9, `_` and `-`
names; strings, numbers or booleans as text of at most 200), kept as a line of the environment's
`events:` log with its route and the edge's country, nothing about the visitor; anything else is a
400, more than 1,200 a minute an environment a 429. It goes through the environment's gates (allow
list, firewall, rate limit, access) as a page does. `events` lists the 20 most counted names, each
with its properties' five most common values (500 names, 20 properties a name and 500 values
a property counted apart at most, so beacons that invent keys can't make a read hold millions). The Metrics tab's Visitors panel and `roll apps analytics` show them.
`analytics_count_page_views_and_visitors`, `track::tests`.

## Caching answers

A web service's answers can be kept and answered by roll (Vercel's CDN with `s-maxage` and
`stale-while-revalidate`, Netlify's and Render's edge caching) when its app says a shared cache may
keep them: `CDN-Cache-Control: max-age=N` (RFC 9213, which only shared caches read), else
`Cache-Control: s-maxage=N`. The owner's proxy (`resp_cache.rs`) keeps a 200's body (at most
1 MiB, its length known) per environment, path and query, and the request headers it `Vary`s
on, and answers the next GET or HEAD from it after the allow list, firewall, rate limit and
access checks but before anything wakes: a sleeping service answers its cached pages asleep, and
costs nothing for them. A stale answer within `stale-while-revalidate` is answered while one
background request fetches it again (marked by a request extension no client can send). Never
kept: `private`, `no-store`, `no-cache`, `Vary: *`, an answer that sets a cookie (roll's own
affinity and rollout cookies go on after it's kept). A request with `Authorization`, a
deployment's own URL, a gradual rollout's environment and a preview's feedback page never use it.
It's bounded (32 MiB and 2,000 answers an environment, the least recently used going first),
emptied whenever the live deployment changes, and lives on the owner (a takeover starts empty).
Answers say `X-Roll-Cache: HIT`, `STALE` or `MISS` with `Age`, and a cached answer's access line
is `by: cache`. An app revalidates on demand (Vercel's and Netlify's on-demand revalidation):
`POST …/environments/{env}/cache/purge {paths?, tags?}` or `roll apps purge [PATH…] [--tag T…]`
drops those paths' answers (any query; `/blog/*` is a prefix), the answers tagged with those tags,
or all of them, on the owner. Tags (Vercel's, Netlify's and Cloudflare's cache tags, Fastly's
surrogate keys) are what the app's answer named in `Cache-Tag`, `Netlify-Cache-Tag` or
`Vercel-Cache-Tag` (comma-separated) or `Surrogate-Key` (space-separated): at most 128 an answer,
matched in any case, so a CMS's webhook can drop every page showing product 42 with
`{tags: ["product-42"]}`. Those headers are the cache's: they never reach a visitor. Settings'
Networking has the same purge (paths starting with `/`, the rest tags, or nothing for
everything). Not yet: caching across nodes.

## Maintenance and draining

An environment in maintenance (Render's maintenance mode; Settings' "Maintenance", `roll apps
env --maintenance [NOTE]`, `--serve` to end it) answers its public names with roll's 503 page,
the note as text (never markup), `Retry-After: 30`, and wakes nothing, so a sleeping service
costs nothing meanwhile. A deployment's own URL still answers, to try a fix first; the private
network is untouched. The setting is kept with the environment's live state on its owner, read
from the store once, so a request pays nothing for it.

A web service's `run.timeout_s` (1–3600, Cloud Run's request timeout; `roll apps set
--timeout`) is how long a request has until the app answers: past it the visitor gets a 504 and
the access log a 504. It runs to the answer's head, so a WebSocket or a stream that has begun is
the app's to keep; unset, there's no limit, as before.

`run.drain_s` (1–300, Render's `maxShutdownDelaySeconds`; `roll apps set --drain`) is how long
an instance being replaced (a deploy, a promoted or aborted canary) or scaled in gets: its
requests in flight finish, then SIGTERM, and SIGKILL when the time is up (a second after SIGTERM
at least). Unset, 30 s. Each instance has its own window, so a slow one doesn't use up
another's.

An environment's own pages (Heroku's error and maintenance pages, stored rather than fetched, so
no request waits on another site): `PUT …/environments/{env} {pages: {error?, maintenance?}}`,
`roll apps env --error-page FILE --maintenance-page FILE` (`--roll-pages` to go back), or
Settings → Your pages. Each is HTML of at most 64 KiB, served as it is, with roll's status and
`Retry-After`, never cached: `maintenance` in maintenance, `error` for the 502, 503 and 504 roll
answers while the app can't answer, wake or start in time. Only roll's own pages are replaced
(they're marked inside the proxy); the app's own 5xx answers go out as the app sent them.

## Pausing

Sleep scales a web service to zero, and its next request wakes it; a worker or a cron service
has no request to wait for, so sleeping isn't stopping (a worker's floor starts it again). A
paused environment (Render's Suspend, `heroku ps:scale worker=0`; `PUT …/environments/{env}
{paused}`, `roll apps env --pause` / `--resume`, the service page's Pause and Resume) runs
nothing until it's resumed: its instances stop and the reaper keeps no floor, a job or a cron
run going is canceled, a request gets roll's 503 "Paused" page and wakes nothing (a private
caller neither; a static site's files and rewrites and a deployment's own URL neither), and that
page is metered as a request, not the app's error (no 5xx alert for an environment its owner
stopped; maintenance's page the same). Deploys, redeploys, promotions, rollbacks and jobs are
refused (409), a deploy queued before it doesn't build and one building doesn't go live, a push
to its branch is logged and not deployed (one waiting for its checks too, ended rather than
deployed late), the tag and secrets watches pass it over (acted on once it's resumed), and a
cron run that falls due is skipped, its schedule moving on, so resuming doesn't make the missed
ones up. `a_paused_cron_skips_its_runs_and_pausing_stops_one`. It's the spend limit's path for one environment: the flag lives with the
environment's live state on its owner, read from the store when that state is loaded, so a
takeover keeps it paused. `a_paused_worker_stays_stopped_until_its_resumed`.

## A password for an environment

An environment's `access: {password}` (Vercel's and Netlify's password protection; "Who gets in"
→ Use a password) lets in visitors who type it, for a client or a reviewer without a roll login
or the link: a page load without a session gets roll's page asking for it (a 401, nothing woken),
whose form posts to `/__roll/password` on the environment's host. The right password sets a
host-only `HttpOnly; SameSite=Lax` cookie, `roll_pass_<env>`, signed for seven days under a key
every node shares and over the password's hash, so a new password ends every session; the cookie
is taken out of what the app sees, as the access token's is. Only an argon2id hash is kept (the
same parameters as roll storage's IAM passwords), never returned: the environment says only
`password: true`. Each client gets ten tries every ten minutes per environment (then a 429 with
`Retry-After`); the right one clears its count. The token link, signing in and the bypass secret
still let in whoever they let in; a request that isn't a page load gets the plain 401.
`api.rs::a_password_lets_visitors_in`.

## Signing in to an environment

`access: {signed_in: true}` (the environment panel's "Require sign-in", `roll apps env
--signed-in`) lets in people signed in to roll who can open the project, as Vercel's
Authentication, Cloud Run's IAM invoker and Netlify's team access do, instead of a link everyone
shares (`visit.rs`):

- **Sent to sign in.** A page load with no session of the environment's own goes to
  `/dashboard/apps/visit` on roll's dashboard, before anything wakes; anything else is a 401. The
  app's host never sees roll's session cookie (another site), so the dashboard asks
  `POST …/visit` as the person for a one-use ticket (a minute) to the page they wanted. It's
  only for a URL at one of the environment's names (its hostname, an active custom domain), and
  only for a person's own roll login (`ses_…`), never a token. The ticket's URL is built from
  the environment's own scheme and port (roll's, as its URL is), with only the path and query
  taken from the page asked for: `to=http://<host>:<another port>/` can't send the ticket over
  plain HTTP or to whatever else listens on that address.
- **A session of its own.** The ticket goes back to the environment's host
  (`/__roll/visit`), which trades it for a host-only HttpOnly cookie, `roll_visit_<env>`, for 12
  hours, and sends them on. The app never sees the cookie. Tickets and sessions are kept by
  their SHA-256; the owner remembers a session it read until it ends.
- **Who.** rolld's gate lets only admins into roll apps' routes today, so today it's an
  admin; once roll apps serves people, it's whoever may open the project (rolld's
  `AccountsVisitors`, the one place that says). A ticket is only given to them, and a session
  asks again every 5 minutes (`visitor_recheck`): an account disabled, deleted or no longer an
  admin is turned away at its next look after that, and its session is gone. The ticket and the
  session keep the roll login that asked for them, and the recheck asks that it's still live
  (`Visitors::session_live`: signing out of roll, or revoking one's sessions after a lost laptop,
  ends what it opened within the same 5 minutes); a session from before logins were kept ends. Changing the
  environment's access ends every session. A token (`access.token`) still works alongside it,
  as does the bypass header for CI.

`api.rs::a_signed_in_person_gets_into_a_gated_environment` follows a page load to the
dashboard, a ticket only for a person and only to the environment's names, the ticket used
once, the session the app never sees, the person losing the project ending it, the roll login
ending it, the ticket's URL on the environment's own scheme and port, and a new access ending
it.

## Private services

`run.public: false` (Settings' "Reachable", `roll apps set --public false`) takes a web service
off the internet, as Render's private services: its `up.roll.site` names, its deployments' own
hostnames and its custom domains are 404s like an address nobody has, and `Environment.url` is
null. It's read from the service's settings on each request, so it takes effect at once, not
with the next deploy. The project's services still reach it at `<svc>.<env>.internal` on their
network, which wakes it ("A private caller wakes a sleeping service"). Its previews have no
public URL either.

## Scaling on CPU and memory

A web service scales on its requests (`concurrency` per instance); with `run.cpu_target` (10–100,
Render's target CPU, `roll apps set --cpu-target`) it scales on CPU too: the owner keeps a minute
of its awake instances' measured CPU against what they could use (`run.cpu` each), and while the
average is over the target another instance starts, one at a time, with a fresh minute to judge
it by (the need it had is kept meanwhile, so the new one isn't taken straight back in). Fewer
stay once the instances left would be under the target, through the same scale-in as requests
(`scale_down_after`). Only measured CPU counts: Docker's cgroup, not Fargate's tasks. On real
cgroups (`a_busy_instance_brings_another_on_its_cpu`, `--docker`): a quarter-CPU busybox kept busy
read "CPU at 100% of the 50% target" and got its second instance 11 s after it was made.

`run.memory_target` (10–100, Render's target memory, `roll apps set --memory-target`, Settings'
"Memory target") does the same on memory: the minute's memory in use against the instances'
`memory_mb`, so a memory-bound Node or JVM app scales out before it runs out of memory. With both
targets, whichever asks for more instances decides, and the log says which ("scaled out: memory
at 94% of the 70% target"). Docker only, as CPU. `api.rs::a_web_service_scales_on_its_cpu`
follows both out and back in.


Session affinity (`run.affinity`, Cloud Run's; Settings' "Session affinity", `roll apps set
--affinity true`) keeps each visitor on one instance while it's up and has room: the proxy sets a
`roll_instance` cookie (a digest of the instance's name, taken out of what the app sees), and a
request carrying it goes to that instance before any other. When it's gone or full another
answers and the cookie moves to it. For apps that keep sessions in memory or fan WebSockets out
by instance; the rollout bucket still decides the deployment first.
## Allowed networks

An environment's allow list (Render's `ipAllowList`; Settings' "Allowed networks", `roll apps env
--allow-ip CIDR …`, `--allow-any`) lets only its networks in through the public names, its
deployments' own URLs included. It's the first thing the proxy checks, before access, maintenance
or a wake, on the address the fleet vouches for (`roll_cluster::client_addr`: a forward's signed
client, the edge's, else the peer), an IPv4 client over IPv6 counted as IPv4; a request with no
address is refused. At most 100 networks, kept canonical (`10.1.2.3/8` is `10.0.0.0/8`). It's
read with maintenance into the environment's live state on its owner, so a request reads no row.
The project's private network doesn't go through the proxy and isn't affected.

Firewall rules (Vercel's; Settings' "Firewall", one a line, `roll apps env --firewall 'deny
/admin/*'`, `--no-firewall`) come after the allow list: at most 50, in order, and the first whose
given fields all match decides, `allow` letting the request in, `deny` answering a 403 that
names nothing of the rule, and `limit` (Vercel's rate-limit rules: `limit 10[:20] /login
method=POST`, `{action: "limit", per_minute, burst?}`) letting each client through at that rate
with its own bucket per rule, a 429 with `Retry-After` past it, so `/login` can be held to ten a
minute without throttling the site, and `challenge` (Vercel's Attack Challenge Mode: `challenge
/*` under attack, GET only) letting in a browser that passed a proof of work this hour and
answering anything else a small page that works it out (a SHA-256 with 17 leading zero bits over
a seed only roll makes, under a second) and posts it to `/__roll/challenge`, which sets a pass
(`roll_pass_<env>`, an hour, bound to the client's /24 or /64, never seen by the app;
`challenge.rs`); a request none matches is let in. A script that runs no JavaScript stays out,
and so would a search engine's crawler: roll doesn't vouch for one by its user agent alone, so
put challenges on the paths under attack, not on pages that must be indexed. A rule matches on a path (exact, or
a prefix ending in `*`, which takes the bare path too), methods (HEAD counting as GET), text in
the `User-Agent`, and (Vercel Firewall's custom rules, Cloud Armor's) client addresses (`ips`:
addresses and networks, IPv6 too, at most 100, by the address the fleet vouches for; none, and an
address rule doesn't match), a request header, a cookie or a query parameter being there or
holding some text (`header`/`cookie`/`query: {name, contains?}`; `deny ip=203.0.113.0/24`,
`deny header=X-Scanner`, `deny cookie=role:intruder`, `deny /api/* query=debug` one a line).
Every field a rule gives must match. The logs' request lines say "block" beside an address: a
`deny` of it put first. Not yet: a country, which waits on the edge vouching for `CF-IPCountry`
(a client sends its own to a lone node, so a rule on it would be one it can step around). The path is matched as an app is likely to read it: decoded, `//`, dot segments
and `;params` resolved, in any case and without a trailing slash, so `/%61dmin/x`,
`/a/../admin/x`, `/ADMIN;s=1/x` and `/admin` are all caught by a rule on `/admin/*`. HTTP only: a TCP service has none.

Response headers (Vercel's and Netlify's `headers`; Settings' "Response headers", one a line,
`roll apps env --header '/api/* Cache-Control: no-store'`, `--no-headers`): at most 20 rules,
each a path (matched as a firewall rule's; none: every path) and the headers to set, which an
app's own header of the same name wins over unless the rule `replace`s it (a line starting with
`!`). They're on every answer of the environment, static sites' included (after their
`_headers`), and roll's own pages. Names are checked (a token, lower-cased) and so are values
(printable, 1,024 at most, never a line break); nothing that frames the answer or is roll's
(`content-length`, `transfer-encoding`, `set-cookie`, `connection`, `x-roll-*`, `proxy-*`…)
(`api.rs::an_environments_headers_are_on_its_answers`).

Routes to other services (Vercel's rewrites to another project, Netlify's proxy rules, Cloud
Run's URL maps; Settings' "Routes to other services", `/api/* api` a line, `roll apps env
--route '/api/*=api'`, `--no-routes`): at most 20 rules on an environment of a web service, each
a path or a prefix ending in `*` and another service of the project by name, so `example.com/api/*`
is answered by the API service while the server-rendered frontend answers the rest. The first
that takes the path sends the request to that service's environment of the same name, the path
and query as they came, through the same rewrite as a static site's `service:` rules: after this
environment's allow list, firewall, rate limit and access gate, and past the target's (the
project's own rule let it in), never routed again from there. A route must name a service the
project has, and not its own (400). `api.rs::a_route_sends_a_path_to_another_service`.

A rate limit (Vercel's firewall rate limits; Settings' "Rate limit", `roll apps env --rate-limit
N[:BURST]`, `--no-rate-limit`) gives each client address `burst` requests at once (default: a
minute's worth) and then `per_minute`, a token bucket; more get a 429 with `Retry-After` (when
the next token comes), right after the allow list and before anything wakes. The buckets are the
owner's, in memory: at most 100,000 clients, the full ones forgotten first (they're the same as
none), and all of them when the limit changes.

## Metrics for a scraper

`GET …/environments/{env}/metrics?format=prometheus` (Fly's metrics endpoint, Render's metrics
streams) is an environment's last finished minute in Prometheus' text format: requests, 5xx
answers, latency by quantile, CPU seconds, memory, instances and cold starts, as gauges labelled
with the service, its id and the environment. "Finished" means stored: the meters are written
every 10 s, so until 15 s into a minute it's the one before. A scraper authenticates as anything
else does, with a scoped token that may `read` the service (`roll token create`).

## TCP services

A service whose port isn't HTTP (a game server, MQTT, Redis of its own) sets `run.tcp` (Railway's
TCP proxy, Fly's raw TCP; Settings' "HTTP or TCP", `roll apps set --tcp true`). Each of its
environments gets a port from the node's range (`ROLL_APPS_TCP_PORTS=20000-20999`, kept with the
environment, `Environment.tcp` the `host:port` clients use at `ROLL_PUBLIC_HOST`). A connection
there is let in by the environment's allow list (its peer) and its rate limit (connections per
client), refused in maintenance, wakes an
instance and holds it (a lease, as a request does, so it doesn't sleep while connected), and is
spliced to the app's port, half-closes included. Ports go round the range (one just freed
isn't handed straight to another environment, which old clients may still dial), closing a
port ends its connections, and a range inside the kernel's ephemeral ports (where instances'
host ports come from) is warned about at start. A deploy's health check is the port accepting
and the connection staying open (Docker's proxy accepts whatever the app does, then closes);
its HTTP names are 404s. One node for now: on a fleet the edge doesn't carry TCP (`## Next`). Checked live
against rolld on Docker: a busybox echo server, asleep, echoed the first connection in 170 ms
(the wake) and the next in 5 ms; `a_tcp_service_and_a_tunnel_reach_a_sleeping_port` (`pnpm
test:apps --docker`) keeps it measured: 156 ms over its port from asleep, 168 ms over a tunnel.

## Reaching a service from your machine

`roll apps proxy [-s service] [--local-port N]` (Fly's `fly proxy`) forwards `127.0.0.1:N` (default:
the service's port) to a web service's port through roll: each connection is a tunnel,
`GET …/environments/{env}/tunnel`, a WebSocket whose binary messages are the connection's bytes.
The owner wakes an instance and holds it (a lease, as a request does) while the tunnel is open,
connects only to the service's own port, and puts `tunnel.opened` in the activity. It's how a
private service's admin page or internal API is reached without making it public; it goes past
the allow list, access token and maintenance, so it's `admin`, as a shell is. At most 20 per
environment. Checked live against rolld on Docker: a private busybox `httpd`, asleep, answered
the first `curl` through the proxy (the wake), then in 6 ms.

`roll apps exec [-s service] [-e env] -- <command>` (Railway's `railway run`) runs a command on
this machine with an environment's variables, read through `…/variables/values` (a reveal, in
the audit trail). `PORT` and `ROLL_*` are the platform's, so they're left out, and so is a name
that steers this machine's own programs (`PATH`, `LD_PRELOAD`, `DYLD_*`, `BASH_ENV`,
`NODE_OPTIONS`, `PYTHONPATH` and the like: anyone who may write the environment's variables
could otherwise run code on the laptop running `exec`); each is named. A sealed variable is never
shown again, so it can't be set and is named instead. A linked service's private URL
(`http://<name>.<env>.internal:<port>`) is pointed at a local tunnel to it, as the proxy above
makes one, for as long as the command runs, so `roll apps exec -- npm run dev` reaches the API
its web service links (Railway's `railway run`, Netlify's `netlify dev`); another `*.internal`
value is said, as it only resolves inside roll. It exits with the command's exit code.

## Floors by the clock

`run.schedule_scale: [{windows, timezone?, min_instances}]` (AWS's scheduled actions, Azure's
autoscale profiles) raises a service's floor for some hours of the week: business hours that
shouldn't wait for a cold start, a batch window, a launch. In any of a rule's weekly windows
(`{from: "mon 08:00", to: "fri 18:00"}`, as a deploy freeze's, in its time zone) its
`min_instances` is the floor when it's higher than the service's own, never past
`max_instances`; out of them the service's own floor holds, and what scales on requests, CPU or
a backlog still scales above it. The reaper reads it each second, so a window starts and ends
within one. At most ten rules; changing them is a write in the permissions catalogue (it only
changes how much runs). `freeze::tests::a_schedule_raises_the_floor_in_its_window_only`.

## Workers that follow their work

A worker with `run.scale` (`{ stream, group, per_instance }`) runs one instance per
`per_instance` messages its roll stream consumer group is behind, between `min_instances` and
`max_instances`, as no general platform can: roll owns the stream (Render scales workers on CPU
and memory, KEDA on queue length outside the platform). Every ten seconds the service's owner
reads the group's lag (its committed offset on each partition against the partition's high
watermark, as the broker's `GET …/groups/{g}` adds it up). A stream is named by its id (`st_…`),
never a name, and a group by Kafka's characters. It's read only for a stream on the service's
own project, and only on a project the operator owns: anyone can put any reference on their own
canvas and streams don't know their owners yet, so a person's project doesn't follow a stream
(as roll secrets refuses links there), and roll MCP's gate wants read on the stream to set it.
Or a worker follows a gauge of its own (KEDA's Prometheus scaler, Cloud Run's custom metrics):
`run.scale: {metric, sum?, per_instance}` (Settings' "Or follow its own gauge") reads, every ten
seconds, the newest point of each series of that name from the last minute, from its Prometheus
endpoint ("The app's Prometheus endpoint") or its OpenTelemetry SDK, the largest of them (each
instance sees the same queue) or with `sum` their total (jobs in flight per instance), and runs
one instance per `per_instance` of it. A counter or histogram isn't a level: it's said in the
log and not followed. A stopped worker reports nothing, so this needs `min_instances` of at least
one. `api.rs::a_worker_scales_on_its_own_gauge`.
A deploy starts a scaled-out worker at the instances it wants, not back at its floor; a note
about scaling is logged when it changes, not every ten seconds; and a web service's `scale` is
dropped (it scales on its requests); the reaper starts instances at once and
stops one (the newest, given the drain to finish its work) only after fewer were wanted for
`scale_down_after`. rolld reads the lag from its own API (`GET /v1/streams/{s}/groups/{g}`, the
operator's token), which on a fleet forwards to the stream's owner, the only node whose numbers
are right (checked against a live rolld: 30 records, a group committed at 5, lag 25). Queues
follow once a queue is a workspace resource (its project is what keeps it a tenant's). Settings
picks the stream from the project's own, with the group and messages per instance (`roll apps
set --scale <stream>:<group>[:<per instance>]`), and the environment shows the backlog last read
and the instances it wants (`Environment.scaling`).


A deployment that crash-loops soon after it went live can undo itself (`run.rollback_on_crash`,
Settings' "A crash loop after a deploy", `roll apps set --rollback-on-crash true`): when the
reaper notices the loop within ten minutes of the switch, it rolls back to the deployment before
it (its image, no build, actor `crash loop`), the at-once deploy's version of a gradual
rollout's error rollback. A deployment that is itself a rollback is never rolled back this way,
so a bad rollback target stays and alerts rather than flapping.

A worker's restart policy (Railway's; Settings' "Restart", `roll apps set --restart
always|on_failure|never [--restart-tries N]`) says what happens when it stops: `always` (the
default) starts it again with the backoff, `on_failure` only after a nonzero exit or out of
memory, up to `restart_tries` in a row (10), and `never` leaves it. A clean exit under the last
two is the work done, not a crash (no crash loop). What the policy leaves stopped counts toward
the worker's instances, so nothing takes its place, until a deploy or a restart starts fresh
ones.

One instance is restarted on its own (`POST …/instances/{name}/restart`, `roll apps restart
--instance 1`, Restart on its row in Settings' Instances; Fly's `machine restart`): a web
service's fresh instance starts and answers first (outside the wake lock, as a deploy's do),
then the two swap under the lock and this one drains, so no request sees it go. One with a
volume, a worker's, an old deployment's or a canary's, or one at `max_instances` stops first
instead (never two on a volume, never past the most), and the reaper or the next request starts
what's wanted; a capped project's are refused. If the fresh one doesn't start, the old one keeps
serving and the answer says why
(`api.rs::one_instance_is_restarted_without_the_rest`).

## HTTP/2 and gRPC

`run.h2c` says the app speaks HTTP/2 without TLS, as gRPC servers do (Cloud Run's
`--use-http2`, Fly's `h2_backend`): the proxy forwards to it over an HTTP/2-only client (prior
knowledge), keeps `te: trailers` (gRPC refuses requests without it), and its health checks are
h2c too. Every response's frames pass as they came, so trailers (gRPC's `grpc-status`) reach the
client; this was true of no response before, since the proxy only copied data frames. A
WebSocket upgrade stays HTTP/1.1. Clients reach it over HTTP/2 on rolld's own apps listener (h2c
or HTTP/1.1); on the fleet the edge's HTTPS listener offers only `http/1.1` today, so gRPC from the
internet waits for it to offer `h2` (in `## Next`).

## A deployment's own URL

Every built deployment of a web service without a volume answers on its own hostname,
`<id>--<slug>` (the id without `dep_`), whatever is live (Vercel's deployment URLs, Cloud Run's
revision tags): the same environment's access rules, one instance at most (made from that
deployment's image, variables and settings), gone a minute after its last request, metered with
the environment. The active one's URL is just its instances. With `no_traffic` (`roll deploy
--no-traffic`), a deploy builds, pulls and health-checks, then stops at `ready` without
switching anything, so it can be tried at its URL; "Put live" (a rollback to it) runs its image
again, no build, through a gradual rollout when the service has one. A no-traffic deploy neither
supersedes a queued deploy nor is superseded by one, decides on the run it built (a roll.toml
that makes it a worker fails it), and doesn't run `pre_deploy` (a try-out never migrates the
shared database; putting it live does). No environment's slug contains `--` (slugs collapse
runs of `-`), so no hostname a tenant can get looks like another's deployment. A deployment's
URL answers with that deployment or not at all, never quietly with the live one; its instance is
metered even when nothing's live, and isn't counted in the environment's scaling or a canary's
judgement. A service with a volume has neither: one instance writes it.

## Following a tag

An image service's environment can follow its tag (`follow_tag`; "Follow the tag" in the
environment panel; `roll apps env --follow-tag true`), as Railway's image auto updates do:
every five minutes the owner asks the registry what the tag points at (a manifest `HEAD`
through `Runtime::remote_digest`, no pull, with the service's registry login when it's that
registry's, after the same public-address check a deploy makes) and compares it with the digest
its live deployment pinned. A new one deploys (actor `tag watch`, "nginx:1.27 moved to
…"), through everything a deploy does: pulled and pinned again, health-checked, a gradual
rollout when the service has one. Each environment remembers the digest it last acted on, and
only a move past it deploys: a rollback while following isn't undone, and a digest whose deploy
failed isn't tried again every five minutes. A deploy already on its way is left alone, and a registry
that doesn't answer is a line in the environment's log. Off by default: a deployment runs the
digest it pinned until someone deploys, so a push to a tag never changes production by
itself unless asked.

## Links on one node

A link's URL on one node names the host's loopback: rolld's Postgres proxy is
`127.0.0.1:55432`, and the other products listen on loopback too. A container can't reach that.
Its `host.docker.internal` is the bridge's gateway, where a loopback listener doesn't answer,
and a host firewall can drop every packet from a container to the host (ai-server's does).
`runtime/links.rs` gets around both without opening anything:

- **A unix socket.** When a variable names a loopback `host:port`, the Docker runtime rewrites
  it to `roll-link-<port>:<port>`. It serves that port on a unix socket,
  `<home>/apps/links/<port>/s.sock`, which forwards to the loopback port it stands for.
- **A relay.** Before an instance is made, the project network gets a relay for each such port
  (`roll-apps-link-<project>-<env>-<port>`). The relay is `socat`, pinned by digest, under the
  alias `roll-link-<port>`. It connects each caller to the socket through a bind mount, so no
  firewall is involved. It has a read-only root, no capabilities but binding a low port,
  32 MiB and 64 processes, and it restarts with the daemon.
- **Narrower than before.** A container reaches only the ports its project's links name, not
  the rest of the host. Removing the network removes its relays.
- **Where it's off.** With a remote Docker daemon, on macOS (Docker Desktop's
  `host.docker.internal` reaches the host's loopback itself), or with
  `ROLL_APPS_LINK_RELAY=0`, URLs are rewritten to `host.docker.internal` as before. Deployments
  made before this keep their old URLs until their next deploy.

`docker.rs::a_linked_database_on_loopback_is_reached_from_the_container` runs on ai-server,
whose firewall drops container-to-host traffic: the app fetches its `DATABASE_URL` at start and
serves the answer. With `ROLL_APPS_LINK_RELAY=0` the same test fails. The Umami template went
live on a real roll Postgres this way on the review rolld (healthy after 12.9 s), where it had
failed before.

## Egress

An environment with `egress: {mode: "roll", hosts}` (`PUT …/environments/{env}`; roll previews
sets it on a preview's environments from the project's `egress` and `egress_hosts`) can reach
roll and nothing else, so a fork's preview can't send its branched data anywhere:

- **A build on a network of its own, without its secret values.** BuildKit's own builds take
  no network but `host` and `none`, so a closed environment builds on its project's builder
  instead (`roll-apps-build-<project>`, `docker buildx` with the docker-container driver, its
  image loaded into the daemon after): a network made `--internal` with Docker 28's isolated
  gateway, so a step reaches neither the host's services nor the metadata endpoint
  (169.254.169.254) nor any private range. Its only way out is an egress proxy of its own that
  lets any host through on 443 and 80, connected to public addresses only; its steps get it as
  `HTTPS_PROXY`/`HTTP_PROXY` (by address: BuildKit's steps resolve with public DNS), which npm,
  pip, apk, apt, Go and cargo read, and the builder pulls base images through it. A tool that
  ignores the proxy variables gets no network. The builder is made on the first closed build and
  goes, with its cache, when the project's last service does. The build still gets no variable
  that is a secret (sealed, a roll secrets value that isn't plain, a link's credential), carries
  a URL password, or holds a secret value of eight characters or more (a reference that copied
  one), since public hosts are reachable (`ResolvedVars::without_secrets`). Its instances still
  get them all. The environment says `build_closed: true`, which roll previews checks before it
  builds a fork's head under `roll`, and the deploy itself says `closed: true` (refused unless
  it's built closed, and refused as unknown by a node that predates it: a rolling upgrade fails
  closed). A node that can't build a closed environment closed fails the build rather than
  build it on the default network. A build that needs one (a private registry's token) needs
  the environment open. Measured on ai-server (`pnpm test:apps --docker`,
  `a_closed_environments_build_has_no_way_out_but_the_proxy`): a first closed build of a small
  image, its builder made, takes 13 s; each build also loads its image into the daemon.
- **A network with no route out.** From its next deploy its instances run on
  `roll-apps-closed-<project>.<env>`, made `--internal` (the `.` is in no environment's name, so
  it's never another environment's open network, and a network already there with the wrong
  kind is refused, not joined), with Docker 28's isolated gateway: the network has no address of
  the host's, so its code reaches none of the host's services (nothing the node serves on every
  interface), and no `host.docker.internal`. Docker publishes no port from such a network, and
  the host can't reach its addresses either, so a web instance's network sandbox runs `socat`
  instead of sleeping (`runtime/ingress.rs`): it listens on a unix socket in a directory
  bind-mounted from `<home>/apps/ingress/<sandbox>/` (as rolld's user) and joins each caller to the
  app's port in its namespace, and rolld answers on a loopback port of its own for it, so the
  proxy reaches it at `127.0.0.1` as any other. Its waker is as an open one's: asleep, another
  service on the network reaches it by name and wakes it, and rolld's own connections (from the
  sandbox's loopback) are told apart (`docker.rs::a_closed_environments_private_caller_wakes_it`).
  A closed network made before this keeps its gateway until its environment's last service goes;
  with network sandboxes off (`ROLL_APPS_NETNS=0`) closed networks keep it too. Deleting an
  environment removes everything labelled for it first (`Runtime::list_env`: an earlier
  process's instances, sandboxes, wakers and runs too), so its network and its relays go with it
  (a network left behind holds one of the host's address pools).
  Its links' relays are on the same network, so roll's databases are reached as before; the
  project's other closed services there by name.
- **Listed hosts through a proxy** (`runtime/egress.rs`). `hosts` (`api.stripe.com`, or
  `*.sentry.io` for its subdomains; names only, at most 20) go through a CONNECT and plain-HTTP
  proxy in rolld on a unix socket, reached from the network through a relay
  (`roll-egress-<digest>:3128`, the links' `socat` image). It allows only a listed host on 443
  or 80, connects only to public addresses (a listed name that resolves privately is refused
  like an unlisted one), and keeps the hosts it refused, which the environment shows
  (`egress_refused`). A proxy is its network's own (named by the SHA-256 of the network and the
  list). A plain HTTP request goes alone: its `Host` is the checked URL's, it's closed after, and
  nothing else on the connection is sent; a chunked body is refused (411). An HTTPS tunnel is
  checked by the name it asks for (CONNECT), not its TLS SNI, which the proxy can't read. A
  project environment's closed services share its network, so their lists add up: what one may
  reach, another there can reach through its proxy. Instances get `HTTP_PROXY`/`HTTPS_PROXY` (and lower case), with
  `NO_PROXY` naming roll's own (`.internal`, the relays, the project's services); their own
  variables win.
- **Never open by accident.** A runtime that can't close a network refuses `roll` (409), and a
  deploy of a closed environment on one fails instead of running open. Fargate can't yet (a
  security group per task with no outbound rule is in "Next").

`docker.rs::a_closed_environment_reaches_nothing_but_its_listed_hosts`: on real Docker, the
instance is served on its closed network, a direct connection out fails, its proxy refuses an
unlisted host, and the refusal shows. `api.rs::closed_egress_runs_on_its_own_network_with_a_proxy`
checks the settings and what instances are made with; `egress.rs`'s unit tests the proxy
(tunnels, origin-form requests, refusals, private addresses).

## A Git URL

A service can build from any Git repository by its URL (a private one with its own login, below) (`{"kind": "git", "url":
"https://github.com/acme/site"}`; "Or a Git URL" under Repository in New service; `roll apps
create --git URL --branch main`; `git = "…"` in a project file), as Render's "public Git
repository" and Railway's and Cloud Run's repo URLs are: a GitHub or GitLab repository with no
app installed and no roll code mirror. `git_source.rs` is the whole of it:

- **Made**: the branch named (`branch`), or the repository's default one, must exist then: an
  `ls-refs` (protocol v2) answers in one round trip. Production follows it.
- **Deployed**: the deploy resolves the branch's head (or takes the `revision` given: a full
  commit id), then the build fetches that one commit shallowly (`depth 1`, `code_git::Client`),
  so a large history costs nothing. The tree is written with the hostile-tree rules: a name is
  one path segment (never `.`, `..` or `.git`), nothing is written over (a repeated name fails),
  symlinks and submodules are left out (a link out of the tree would build from the host), at
  most 100,000 files and 64 levels; the pack is bounded by `code_git::Limits`. Then it's a build
  like a repo's: its root's `roll.toml` `[app]`, a JS workspace's root, the generated Dockerfile.
- **Fetched safely**: the URL is `https://` (`http://` only where private addresses are allowed),
  carries no credentials, and goes through the same public-only client as webhooks and drains
  (`net::client`: every resolved address checked, no redirects, a timeout), plus the
  literal-address check, since an IP literal skips the resolver.
- **Private repositories** on GitLab, Bitbucket, Gitea or any smart-HTTP host (Render's,
  Vercel's, Netlify's): the service's own login (`PUT …/git-login`, `git_login` when it's made,
  `roll apps git-login`, Settings' "Private repository": a deploy token, an app password or an
  access token, or the name of the project's roll secret that holds it, read from its production
  secrets at each fetch so a rotated token, or one roll secrets issues per lease from the project's
  own GitHub App, just works and nothing is kept here; naming a secret is `deploy` in the
  permissions catalogue, as that production secret then goes to the source's host: whoever may
  change what production runs can already read its secrets, sensitive ones too). A password set here is sealed at rest (`git_auth`, bound to the service) and never returned, never
  in the URL, and sent as Basic auth only to the host it was set for: every fetch
  (`git_source::Remote`) checks the source URL's host against it, and moving the source to
  another repository (host or path) drops it in the same transaction, as a registry login is dropped with its
  registry. The branch check when it's made, the plan, deploys, the watch, the hook's ref check
  and watch paths all fetch with it. A GitHub repository can use it too (a fine-grained token
  with read access to contents), or roll's GitHub App (below).
- **roll's GitHub App** (Vercel's GitHub integration; roll code's App, `GitHubApp` port in
  `integrations.rs`, rolld's `CodeGitHub`): `PUT …/services/{id}/github`, `roll apps github`, or
  Settings' "GitHub App" connects a Git URL service on the App's host to the installation that
  sees its repository. Then every fetch uses a short-lived installation token (no personal token
  held), and its deploys post commit statuses (`roll/apps/<service>/<env>`, as a roll code repo's
  do) and GitHub Deployments of each environment (Vercel's: made when a deploy starts,
  `in_progress`, then `success` with the environment's URL or `failure`; production is GitHub's
  production environment, none is transient; the id is kept per deployment,
  `github_deployments`). And it needs no hook: the App's own pushes and pull requests, which roll
  code receives and verifies (`X-Hub-Signature-256`, once per delivery: `Code::on_github_event`),
  are queued for every service connected to that repository through that installation
  (`github_events`, only what deciding reads) and run on each service's owner within a second,
  as its hook's would be (`git_event`: the refs checked, every following environment deployed,
  pull requests and other branches to roll previews; actor `github app`). Installations span tenants (an installation's token reads any repository it sees), so the
  route is operator-only in the permissions catalogue (`OPERATOR_ONLY`), as connecting a repo is
  roll code's admin token's. That keeps grants out; sessions are kept out by rolld's gate, which
  gives signed-in people no `/v1/apps` route (an admin's session carries `Operator`). If roll apps
  ever serves signed-in people, this route must refuse a non-admin session itself. A private repository on the App's host can be made before it's
  connected: its branch check failing there doesn't fail the creation (the App isn't asked: whether
  it sees a repository is no tenant's to learn), and its first deploy waits for the connection or
  a login. Moving the source to another repository drops the connection with the login.
- **Followed**: every five minutes, with the tag watch, the owner asks each of its Git services'
  environments' branches where they are (`ls-refs`, a few hundred bytes). A new commit deploys
  (trigger `push`, actor `git watch`), keyed by the commit so two nodes make one deploy. As with
  a tag, each environment remembers the commit it last acted on: a rollback isn't undone, a
  commit whose deploy failed isn't retried every five minutes, and a paused or held environment
  waits. A push reaches production within five minutes, not at once: no webhook comes from a
  repository roll has no app on. A repo in roll code still deploys on each landing at once.

## Volume backups

A volume is backed up to object storage as a tar (`apps/volumes/<service>/<env>/<unix ms>.tar`),
on demand (`POST …/volume/backups`, "Back up now" in the environment panel, `roll apps backups
--now`) and on a schedule by its owner: once a day with the newest seven kept, or the volume's own
`backups {every_hours, keep}` (Settings → Back up every / Backups kept; Fly's snapshot retention,
Railway's schedules: every 1–168 hours, 0 for by hand only, and 1–90 kept, scheduled and by hand
alike), as Railway's volume backups and Render's and Fly's snapshots. A restore can take another
environment's backup of the service (`{from: "production"}`, the environment panel's "production's
backups", `roll apps backups -e staging --from production --restore <id>`): staging seeded from
production's data, what staging had backed up first under its own name. Copying another
environment's files needs reveal there for a scoped token, as copying its variables does. A throwaway busybox container with no
network mounts the volume read-only and streams `tar c` into `Objects::upload`, so nothing holds
the volume in memory; it's the disk at that moment, so an app mid-write can leave a file half
written, as any snapshot of a running disk. The container's exit is checked at the end of the
stream, so a backup that stopped short (a daemon error, an image it couldn't pull) fails instead
of being stored, and never rotates a good one out. A restore (`…/backups/{id}/restore`)
downloads the backup whole and checks it against its listed size before touching anything,
then holds the environment asleep under its wake lock (requests get a 503 for that moment;
rollouts and cron runs on the volume wait or refuse), extracts the tar into a staging directory
on the volume, and only once it extracted whole swaps the old files for it; a broken tar leaves
the volume as it was. It runs on its own task, so a client that goes away can't leave it half
done or the environment held. The next request wakes it. The hourly backups run on their own
task too, never on the reaper's loop. Both are in the audit
trail. Without object storage on the node, backups answer 409. (Fargate has no volumes yet.)

What a click can't take back is backed up first. A restore backs up what's there before it
writes, so it can be undone. An environment's deletion (and a service's, for each of its
environments) stops its instances, takes a last backup of its volume and only then removes the
volume, so a service's deletion no longer leaves its volume behind. A deleted environment's
backups stay a week under its name (an environment made again with the name lists them), then
the sweep (one node on the fleet, hourly) removes them. A daily backup that fails is said in the
environment's log and is a `volume.backup_failed` event, notified by default; the last one that
worked stays. `a_volume_is_backed_up_and_restored`.

## Log drains

A project can send its services' runtime output to up to five HTTPS endpoints
(`POST /v1/apps/projects/{project}/drains`, Settings → Log drains), as Render's log streams and
Fly's and Heroku's drains do: NDJSON batches of at most 500 lines or a second, each line its
time, service, environment, instance, deployment, stream and text, signed as webhooks are. Each
node sends what its own instances write (a bounded tap on its log hub), so nothing crosses
nodes and no line waits on another node. It's best effort, as drains are: a batch is tried three
times and then dropped, and a drain that can't keep up loses its oldest lines (counted in the
node's log) rather than slowing a writer or holding memory. Endpoints are tenant-chosen URLs:
the public-only client, no redirects, private addresses refused unless
`ROLL_APPS_ALLOW_PRIVATE`.

A drain chooses what it's sent (Vercel's drains): `sources` (`runtime`, the default; `http`,
the access log, a request a line; `build`, each deploy's build; `audit`, each audit entry as it's written, below), and for whom (`environments`,
`services`, `previews`, as notification targets do; a preview's output only to a drain that asks
for previews'). Each line says its `source`. A vendor's intake wants its key in a header
(Datadog's `DD-API-KEY`, Better Stack's and Axiom's `Authorization`): a drain's `headers` (at
most ten, names checked, never `content-type` or roll's own `X-Roll-*`) are sealed, sent with
every batch and never returned but by name. A drain changes in place (`PATCH`, `roll apps drains
--edit`, the dashboard's Edit: its sources and filters, and new headers only when typed, since
they replace them all), keeping its URL and secret; at once on the node that changed it (only that project's
drains are read again, every other project's queued lines untouched), within 30 s on the
others. A batch's `X-Roll-Delivery` is the same on its retries, so a receiver dedupes by it.
Request and build lines reach the log tap only while some drain asks for them (the proxy's hot
path pays nothing otherwise), the gate read from the drains' sources at every change and every
30 s. Listing drains opens nothing; a drain whose sealed settings won't open is skipped, said in
the node's log, rather than every drain failing. Test (`…/test`, `roll apps drains --test`, the dashboard's Test) sends one line now,
signed and with its headers, and says how it was answered. `roll apps drains --add URL --source
runtime,http --only-env production --header 'DD-API-KEY: …'`.
`a_log_drain_gets_the_runtime_output_signed`.

The audit trail streams out too (Vercel's audit log streams), so a SIEM keeps who changed what
past the 90 days roll keeps it. A drain with the `audit` source gets each entry as it's written,
on the node that wrote it (the service's owner): a `system` line whose text is the entry as JSON
(`{"event": "audit.variables", "data": {"actor": …, "set": ["API_TOKEN"], …}}`, names and ids,
never a value), its environment `""` when the entry is the service's own (`audit.deleted`,
`audit.git_login.set`). It's a drain, so it's best effort. A notification target that asks for the
`audit` event gets the same entries through the event log, durably (retries, the delivery log),
as a webhook's JSON or a chat message ("alice@… did variables {…}"). PagerDuty and Opsgenie
never get them: nobody gets paged for a change. `audit_entries_stream_to_drains_and_webhooks`.

A preview's services that roll's own runtime runs (roll previews' `roll.toml` services, not
roll apps instances) reach the same drains: previews follows each one on the preview's holder,
masks its lines and hands them to the drainer (`Apps::drain_preview_lines`, never waiting; past
256 batches waiting one is dropped, counted in the node's log). They go to drains with
`previews` and the `runtime` source, in the preview's `pv-` environment (an `environments`
filter applies), with `service_name` the service's and `service`, `instance` and `deployment`
null, so a drain narrowed to `services` never gets them. A preview's jobs' output (`release`,
`seed`, `teardown`) is the `build` source, the job's name its `service_name`.
docs/previews.md, "Log drains".
`a_previews_runtime_lines_reach_only_drains_that_take_previews`.

A drain's `format` (Heroku's and Render's syslog drains, every vendor's OpenTelemetry intake)
picks its wire, fixed when it's made: `ndjson` (the above), `otlp` (each batch an OTLP/HTTP JSON
`ExportLogsServiceRequest` to the URL, a collector's or a vendor's `/v1/logs`: a resource per
service and environment, `service.name`, `deployment.environment.name` and `roll.service.id`; a
record a line, its time in nanoseconds, its text as the body, `log.iostream`, `roll.source`,
`roll.instance` and `roll.deployment` as attributes; the drain's headers carry the vendor's key)
or `syslog` (`syslog+tls://host:port`, as Papertrail gives one: RFC 5424 messages, facility
user, `err` for stderr and `info` otherwise, the service as HOSTNAME, the environment as
APP-NAME, the instance as PROCID and the source as MSGID, each on one line and framed by its
length (RFC 5425) over TLS with the system's roots, a connection a batch; the host resolved and
refused when private, as the HTTP client's resolver does; no headers: a syslog server's token is
its host and port). Retries, buffering and filters are the same for every format. `roll apps
drains --add syslog+tls://logs.papertrailapp.com:12345 --format syslog`, the dashboard's Format.
`a_log_drain_speaks_syslog_or_otlp`.

## Spend limits

A project can have a monthly alert and a hard limit (`PUT /v1/apps/projects/{project}/limits`,
Settings → Spend limits, `roll apps limits --alert 20 --hard 50`), in US dollars at the prices
in "Observability and cost", over its services' metered use this calendar month (UTC), as Railway's usage limits
work. One node (`apps:spend`) checks every 30 s: past the alert it writes one
`spend.alert` event for the month; past the hard limit one `spend.limit` and marks the project
capped. Each mark is a compare-and-set in the event's own transaction, so a handover never sends
two. Every node reads the capped services every 30 s and caps their environments here: running
instances stop, no floor is kept, a wake answers 402 ("Paused"), and deploys, jobs and cron runs
are refused. Changing the limits clears the month's marks; the next pass decides them again, so
raising the limit lifts the cap within 30 s. Notification targets get both events by default.
Spend counts the services a project has now (a deleted service's month drops out).

## Measured

- **The proxy's own cost on warm traffic** (`docker.rs::the_proxys_own_cost_is_measured`, a
  release build, real Docker, the same busybox instance through roll's public route and at its
  own published port, 500 requests one at a time after 50 to warm up, three runs each, ai-server
  at load 22 to 30): it added 250, 265 and 261 µs at the median (mean 259). Two store queries a
  request (the host's environment, then whether its service is still public) were most of it:
  with a host's answer kept a second (forgotten at once on any change on that node: its
  settings, environments, domains; only a name of 253 bytes at most that names something is
  kept, since anyone can send any `Host`), it adds 133, 90 and 102 µs (mean 108), 58% less. Requests a
  second at 50 at once are the app's (busybox httpd, about 550 either way). On another node a
  change (made private, a domain gone) shows within that second; the node that made it sees it
  at once.
  What's left was timed by segment (temporary checkpoints, release, 500 warm requests): roll's
  own work is about 40 µs a request (the host 3, the dispatch 7, the gate, access, live state and
  lease 12, the upstream request built 10, the answer 5, the headers and pages 4), none worth a
  change; the rest of the difference is the hop to the instance (a fresh connection each time,
  since busybox httpd closes it; an app that keeps it alive gets the pooled one).
- A web instance's network lives in a sandbox made with it (`Docker` with `netns`, the default;
  `ROLL_APPS_NETNS=0` turns it off): busybox asleep, holding the network namespace, its aliases
  on the project network and its published port, with no capabilities, 16 MiB, half a vCPU, 8
  pids and a read-only root (at a twentieth of a vCPU, its own start was throttled to 800 ms
  instead of 170, on the path of every new instance). The app joins that namespace (`NetworkMode: container:<sandbox>`), so
  a wake starts only the app, with no network set up on its path (the same idea as Kubernetes'
  pause container). Snapshot restore (CRIU) would go further, but this host has no CRIU and
  Docker's checkpointing is experimental. Measured on real Docker
  (`a_cold_start_skips_network_setup_with_a_sandbox`: the same image, five alternating pairs):
  median 151 ms against 238 ms, and in a second run 171 against 375, faster in five of five each
  time; the standard cold start went from 258 to 175 ms median and Node's from 313 to 199 ms.
  After the private-caller watch moved off the socket tables (2026-11-13, evening), the same
  suite measured the standard cold start at 155 ms median, the pairs 146 against 257 ms, and
  Node's at 217 ms: the start path no longer reads them.
  The sandbox (and its waker) is removed with its instance, and a node taking a service over
  keeps a known instance's sandbox and waker.

On ai-server (36 cores, Docker 29, overlayfs, load around 25–30 from other work), 2026-11-12.
`crates/apps-control/tests/docker.rs` (`pnpm test:apps --docker`) prints the first three; the Node
row is `roll up` of a two-file Node app against a real rolld, timed with curl over Tailscale.

| path | what | median |
| --- | --- | --- |
| warm | a request to a running instance through the proxy | 1 ms |
| resume from suspended | unpause → first byte | 13 ms |
| cold start from stopped (busybox static site, 0.25 CPU) | start → first byte | 260 ms |
| cold start from stopped (Node 22, `node server.js` run directly) | the same, Node booting | 360 ms (static in the same run: 364 ms) |
| first deploy, static site | upload → build (base image pulled) → health check → live | 12.5 s |
| first deploy, Node | `roll up` → build (`node:22-slim` pulled) → health check → live | 11.1 s |

- A wake talks to dockerd's socket (`runtime/engine.rs`), not the `docker` CLI. Four alternating
  pairs of the Docker test at load 35–38, CLI against socket, medians per run: cold start
  296/293/299/325 ms against 270/263/270/285 ms (all four pairs won, about 9%); resume
  48/41/45/45 ms against 14/13/16/13 ms (3.3×). What's left of a cold start is dockerd starting
  the container and the app's own boot.
- **Startup CPU boost** (Cloud Run's): an instance starts with 4× its CPU (at least 1, at most
  4 cores) and is held to its own limit once it answers (`Runtime::settle`; the limit is the
  container's `sh.roll.apps.cpu` label, so a new process of rolld still applies it); a stop
  gives the next start the boost again, off the wake's path. Measured on the static site at
  0.25 CPU, 5 alternating pairs: medians 291/326/294/274/354 ms without it against
  243/260/267/268/249 ms with it (all five won, 294 → 260 ms overall, and a far tighter spread);
  the tiny Node server at 0.5 CPU: 329/324/328 against 315/320/319 ms (three of three, about
  3%). The Docker test checks that a running instance is back at its 0.25 CPU. What it uses is
  metered from the cgroup, so the boost is billed as used.
- A start script that is just `node …` runs without npm in front of it (apps-build,
  `Pm::start`). Before that and the socket, a Node cold start was 665 ms against 443 ms for the
  static site in the same conditions; now the two are the same within noise (360 and 364 ms,
  `generated_node_python_and_go_images_build_and_answer` and the static test, one run). That
  test also builds the generated Node, Python and Go Dockerfiles for real.
- A static site from its files (`a_static_sites_files_answer_without_a_container`, real Docker):
  the first request 2 ms (it reads the site's index), warm 0 ms, against a 258 ms median cold
  start for the same site in a container; no container exists for it at all.
- Idle cost: a service asleep (stopped) meters nothing: no CPU-seconds, no GB-seconds, $0.00.
  The only idle cost is the image on disk (roll's, not the user's). Suspended instances keep their
  memory on the host (roll's cost, not billed) for `sleep_after` (10 minutes by default).
- The service page follows `GET …/changes` (server-sent ticks from the owner: a deploy's steps
  and end, every audited change, and its instances' states, compared in memory each second) and
  reads the service again 250 ms after a burst of them; while that stream is up it polls once a
  minute as a safety net instead of every 3 s during a deploy and every 15 s otherwise, and
  nothing at all while the tab is hidden.
- An environment's read (the dashboard's every few seconds) worked out `needs_deploy` each
  time: every variable decrypted and every link resolved, then the fingerprint. It's cached per
  environment, keyed by the active deployment and a generation every settings, variables, link
  or environment change bumps (after its write, so a read racing a change never caches the old
  answer), and good for a minute so a linked database's own change still shows. With 40
  variables and one link (debug build, the fake link resolver, which answers at once), 300 reads
  took 6.06 ms each cold and 2.10 ms cached, median of five alternating pairs, 61–71% faster in
  all five. Against a real pg-control, resolving a link is a network call, so the saving is
  larger.

## Builds in detail (apps-build)

**A monorepo's apps** (Railway's monorepo import, Vercel's root picker): `apps_build::candidates`
walks a tree's folders up to three levels down (dependencies, build output, tests and docs
skipped) and keeps those that plan to something that runs or serves (a start command, a
framework, its own Dockerfile), at most 20. New service asks for them once per repo or Git URL
(`POST /v1/apps/plan {roots: true}`) and offers each as a root directory, and a Git URL's plan
shows before its first build (`a_monorepos_apps_are_its_candidate_roots`).

**Drop a folder to deploy it** (Netlify Drop): an upload service's Deployments tab takes a folder
dropped on it (or chosen), bundles it in the browser exactly as `roll up` does (`lib/bundle.ts`:
the `roll-bundle-1` tar, gzipped with `CompressionStream`, `.git`, `node_modules`, build output
and `.env` files left out, at most 256 MiB) and posts it to `…/deploys` as an upload: a static
site is live seconds later, anything else builds as `roll up`'s would. Checked against a live
rolld: a dropped two-file site served at once, its `.env` not uploaded. New service's "roll up"
tab and an empty project's canvas take a folder too: the upload service is made from the folder's
name, then deployed, and its page opens on the first build (checked live: a folder dropped on an
empty project was a service serving it 3.6 s later).

`roll completions bash|zsh|fish|powershell|elvish` (or `roll completion`) prints the shell's completions for the whole
CLI (every command and flag, `clap_complete` from the same definitions), as `fly completion` and
`railway completion` do.

`roll apps plan [DIR]` reads a tree on this machine with the same detection (`apps_build::plan`
over a `DirTree`, as a new web service's build would read it, a Procfile's `web:` included), and
`roll up` does so before it makes a service: a tree nothing builds is said before any project or
service exists (with a roll.toml, whose `[app]` can name the builder, it's only said; its `[app]`
is applied to the plan, and one the build would refuse stops `roll up` there). A repo
service's plan (`--repo`, or a linked repo service) is roll's, from its tree in roll code.

Deno (a `deno.json`, before Node: its config decides) runs its `start` task, or `main.ts`,
`server.ts`… with network, environment and file access, on the Deno image, its dependencies in
the image. .NET (one `.csproj` or `.fsproj` at the root; two are the person's to choose) is
`dotnet restore` and `dotnet publish` on the SDK image of its `TargetFramework`, run on the
ASP.NET image as its `app` user with `--urls http://0.0.0.0:$PORT`. Both are plan-tested
(`deno_runs_its_start_task_or_its_entry_module`, `dotnet_publishes_its_project_and_runs_it_on_port`)
and built and run for real in Docker. Elixir (a `mix.exs`) is a `mix release` on the Elixir image
(`MIX_ENV=prod`; the dependencies' layer before the code; a Phoenix app's `assets.deploy` alias
run first when it has `assets/`), run on the same image (its ERTS needs the same system
libraries) as `nobody` with `PHX_SERVER=true`, `RELEASE_TMP=/tmp` and the CA certificates its TLS
calls need; a Phoenix app's notes say `SECRET_KEY_BASE` must be set. The first Docker run found
the slim image has no CA certificates, so `mix local.hex` couldn't fetch Hex: the build stage
installs them (and git, for a git dependency). `deno_dotnet_and_elixir_trees_build_and_answer`
builds a `Deno.serve` app, a minimal ASP.NET one and a dependency-free Elixir release, none with
a Dockerfile, each answering through roll (55 s for the three from a warm cache, 2026-11-15).

Scala (a `build.sbt`) builds on the sbt image with the build definition (`build.sbt`, `project/`)
copied and `sbt update`d first, Coursier's, Ivy's and sbt's caches mounted: with
sbt-native-packager in `project/plugins.sbt` (or Play) it's `sbt stage`, run as
`target/universal/stage/bin/<name>` (sbt's `normalizedName` of `name :=`, `"""…"""` too: lowercase,
`_` kept, other runs as `-`; Play gets
`-Dhttp.port=$PORT`, and a note on `PLAY_HTTP_SECRET_KEY`); with sbt-assembly it's `sbt assembly`,
the jar run with `java -jar`; neither is the person's to say (a 400 unless start is set; with one,
`sbt compile` and the tree run on the sbt image). Clojure:
a `project.clj` is `lein uberjar` on the Leiningen image, a `deps.edn` with a `build.clj` and a
`:build` alias is tools.build's `clojure -T:build uber` on the tools.deps image (a deps.edn
without them needs build and start set); the standalone jar, wherever the build put it, runs on
the JRE. Both run on `eclipse-temurin:21-jre` as an `app` user with `-XX:MaxRAMPercentage=75`.
Gleam (a `gleam.toml`, the Erlang target) is `gleam export erlang-shipment` on the Gleam image,
run as `entrypoint.sh run` on the same image; a JavaScript target is a 400 saying so. A root
`start.sh` and nothing else roll builds (after every language and a static site, so one beside a
package.json is Node's and one beside an index.html, a dev script, leaves the site served as files)
is run with bash on
`debian:bookworm-slim`, what it needs from the service's `packages` (apt). Plan-tested
(`scala_clojure_gleam_and_a_start_script_build_and_run`); `clojure_gleam_and_a_start_script_build_and_answer`
builds a Leiningen app on the JDK's HTTP server, a Gleam app with an Erlang FFI server and a
start.sh with python3 from `packages`, each answering through roll. The first Docker run found
Leiningen writes the uberjar to `target/`, not `target/uberjar/`: the jar is found wherever it
is. sbt was built and run for real once (sbt-assembly and sbt-native-packager apps answering,
2026-11-18) but isn't in the suite: its first build fetches a Scala compiler, minutes on a cold
cache.

A tree none of them builds says what it did find (`pubspec.yaml`: Dart, a `.sln`, `Package.swift`…),
so the next step (a Dockerfile, the root directory) is plain.

- Node yields to another language: a tree with a Gemfile, composer.json, requirements.txt,
  pyproject.toml, manage.py, go.mod, Cargo.toml, pom.xml or build.gradle isn't a Node app even
  with a package.json; a package.json with a `build` script there adds a Node `assets` stage.
- Manifests are copied before the source, so dependency installs are a cached layer, except when
  the install may read other files (workspaces, install scripts, `patches/`, `-r`/`-e` lines,
  gemspecs, Composer path repositories) or `install` is overridden.
- Framework entries run directly (Nuxt `node .output/server/index.mjs`, Nest `node dist/main`,
  SvelteKit adapter-node `node build`, Astro's node adapter); Next and Remix use the start script.
  Static builds (Vite, CRA, Gatsby, Astro without an adapter, adapter-static) are provider
  `static` on busybox httpd; one with a client-side router is a single-page app ("Single-page
  apps").
- Go runs on distroless static (no shell); Rust builds on `rust:<v>-slim-bookworm` so the binary
  runs on `debian:bookworm-slim`; Java runs the built jar on a JRE image; Python installs into
  `/opt/venv`; PHP serves `php -S` with `PHP_CLI_SERVER_WORKERS=4`.
- Every generated start command takes `$PORT` (default 8080); only a tree's own Dockerfile sets
  another port, through its last `EXPOSE`.
- A **Procfile** (Heroku's; Nixpacks and Railpack read it too) moves a Heroku app over unchanged:
  `web:` is a web service's start command and `worker:` a worker's when the service names
  none, and `release:` is a web service's pre-deploy command (Heroku's release phase, once per
  app: never a worker's or a cron's) when the service sets none; the plan's notes say which. A
  tree's own Dockerfile keeps its `CMD` and reads no Procfile, a static site stays one, and a
  package of a JS workspace built from the repo's root doesn't take the root's Procfile.
- Builds on one node's Docker share one BuildKit, and two things in it were shared by every
  service: cache mounts (`RUN --mount=type=cache`: npm's, pip's, Go's and Cargo's caches,
  `.next/cache`), keyed by their id (the target path by default), and the layer cache, whose key
  for a `RUN` step holds its command, parents and environment but not what its cache mounts
  held. So one service's build could leave files another's read, or fill its own mount with bad
  files and hand a step's output to another service whose step matched (a public repo's
  generated Dockerfile is predictable). Now the build reads a copy of its Dockerfile (generated
  or the tree's own, outside the tree so `COPY . .` never takes it, next to the Dockerfile's
  own `.dockerignore`) in which every cache mount of a `RUN` ends `,id=<namespace>-<hash>`
  (the last id wins) and every stage declares `ARG ROLL_LAYERS`, which the builder sets to a
  value made from the same secret, so every `RUN` step's key is the service's own
  (`apps_build::isolate_caches`: quotes, escapes, `# escape=`, continuations and heredocs as
  BuildKit's parser reads them; checked against BuildKit). The namespace is made at the
  service's first build, never shown (the build log has `…` where BuildKit would print it), and
  the value only reaches the docker CLI's environment. A custom frontend (`# syntax=` other than
  docker's) makes its own mounts and can name any id but a namespaced one. CodeBuild builds
  each run in a fresh VM and share nothing but the service's registry cache, so they're left as
  they are. What a node cached before this change is shared still: an operator upgrading a
  node that built untrusted code prunes BuildKit's cache once (`docker builder prune`).
- **Deploy without the cache** (Render's "Clear build cache & deploy"): `POST …/deploys
  {no_cache: true}`, `roll apps deploy --no-cache`, `roll up --no-cache` (`X-Roll-No-Cache: 1`),
  or Settings → Build. The build runs with `--no-cache --pull` (CodeBuild: without
  `--cache-from`, and writes the registry cache afresh) and the service gets a new cache-mount
  namespace, so the dependency caches start empty for it and every build after it. A deploy of an
  image has no build: 400.

## Not done yet

- The Fargate runtime and the CodeBuild builder are built and tested against fake AWS APIs
  (`crates/apps-control/tests/fargate.rs`: a web service built, health-checked as a task, woken
  and put to sleep; a failed build; a cron run's exit code; no variable in any `RunTask` or
  `StartBuild` request), not against AWS. The stack (`deploy/cloud/aws/roll-cloud.yaml`) doesn't
  have them yet: the CodeBuild project (privileged; its role may push to `apps/*`, read
  `<prefix>builds/*` and the `roll-apps/builds/*` secrets), the execution role (pull `apps/*`,
  read `<prefix>env/*`, write the log group), the fleet role's rights (`RunTask` of `roll-apps-*` families, register task
  definitions, pass the execution role, `StartBuild`/`StopBuild`/`BatchGetBuilds` on the project,
  ECR `CreateRepository`/`BatchDeleteImage` under `apps/`, Secrets Manager under
  `roll-apps/builds/`), a bucket lifecycle rule expiring `<prefix>env/` and `<prefix>builds/`
  after a day, current and noncurrent versions alike (the data bucket is versioned: a deleted
  environment file, its secrets in it, is otherwise a noncurrent version for 35 days; the
  `code/runs/` rule is the model), and the `ROLL_APPS_*` node settings. A private image pulls with the service's
  login as a Secrets Manager secret `roll-apps/registry/<service>` named in the task definition
  (repository credentials; tested against the fake AWS): the fleet role needs
  `CreateSecret`/`PutSecretValue`/`DeleteSecret` there and the execution role `GetSecretValue`;
  the secret is deleted (without recovery) when the login is removed, its registry changed, or
  the service deleted (`Runtime::registry_login_changed`); instances made after that, and stopped
  ones starting again, go without it. A new login reaches the next start, not only the next
  deploy. A value over several lines (a PEM key) can't go in an
  environment file (one line each), so a deployment's such values are one Secrets Manager secret
  (`roll-apps/vars/<stack>/<service>-<deployment>`, JSON, under the fleet's ECS cluster name so
  another fleet in the account never sweeps it) the task definition names per variable
  (`valueFrom <arn>:<NAME>::`). It's made once and kept while the deployment may run (live,
  pending, ready, or ended in the last day), so its ARN and the task definition stay the same
  across cron runs and scale-outs; the ten-minute sweep deletes the rest after an hour, and the
  service's deletion all of them. A secret is written only when its value changed (every write
  is a new version), and asked for at every start of a task definition naming one (never
  remembered: another node may have deleted it, and it's made again under a new ARN). The fleet role needs `CreateSecret`/`PutSecretValue`/`GetSecretValue`/
  `DeleteSecret`/`ListSecrets` under `roll-apps/vars/` and `roll-apps/registry/`, the
  execution role `GetSecretValue`; `fargate.rs::a_multi_line_value_reaches_the_task_through_its_
  deployments_secret`). Still missing on the fleet: private networking (Cloud Map), volumes
  (EFS; a service with a volume is refused) and shells (ECS Exec).
- The edge routes `*.up.<site domain>` and unknown hosts (custom domains) to roll apps.
- Custom domains' certificates are built and tested against Pebble (HTTP-01) and a fake
  Cloudflare API (Cloudflare for SaaS), not against Let's Encrypt or Cloudflare. For
  `ROLL_APPS_TLS=cloudflare` the site zone still needs Cloudflare for SaaS enabled, `up.roll.site`
  proxied and set as the fallback origin (`PUT /zones/{zone}/custom_hostnames/fallback_origin`),
  the token's SSL and Certificates edit on the zone, and an SSL mode for custom hostnames that
  accepts the fallback origin's certificate for the custom hostname's server name (Full, or Full
  (strict) with an Origin Rule overriding SNI to `up.roll.site`). Custom hostnames aren't held to
  authenticated origin pulls, and their `CF-Connecting-IP` isn't believed yet: the edge classes
  them as unknown hosts. Domains' state comes from the minute loop (while `pending`) and Verify;
  Cloudflare's webhooks for custom hostnames aren't used, and a custom hostname whose validation
  timed out isn't re-validated (removing and adding the domain again starts over).
- An instance's writable layer and volume have no disk quota (Docker's `--storage-opt size`
  needs overlay2 on XFS with project quotas, which a node may not have), and from a container
  `host.docker.internal` reaches every listener on the host that binds more than loopback
  (links don't use it: they go through their relays); both are the host's set-up (the fleet
  runs tasks, not containers on a node). roll-fetch
  writes a bundle's env file 0644 (what reads it in previews' containers decides whether it can
  be 0600). Builds' secrets a stopped node left in Secrets Manager are swept at start (two days
  old) and tasks' environment files every ten minutes (`Runtime::sweep`); the bucket's `builds/`
  zips, and every object's noncurrent versions, wait for the stack's lifecycle rule.
- Links: Postgres, Redis, buckets, streams, queues, NoSQL and search are wired. A queue's user
  comes from roll secrets' `queue` provider (docs/secrets.md, "Links"), and its AMQP port on one
  node goes through the same link relay as any loopback port.
- Private networking is a Docker network per project and environment; it isn't reachable from
  the host's own roll databases on hosts whose firewall drops container-to-host traffic.

## With the other platform branches

Reviewed on 2026-11-13 against main 13d4a1b7b (#18 accounts, #19 MCP), feature/roll-mcp-next
b691d1029 (#24), feature/roll-secrets 55a618718, feature/roll-previews 8c7f6ecdf and
feature/roll-search 7f92c2f28, the held apps work being wip/roll-apps-next (pairwise
`git merge-tree` too). Ours are done or in `## Next`; theirs have been told.

- **roll-mcp, the prefix check per method** (#24): a route only a prefix row decides is listed
  in `roll-access/src/prefix-reviewed.txt`. apps-next adds one since a7065490d: `GET
  /v1/apps/services/{id}/environments/{env}/deploys/{dep}/commits` (a read; the line goes in
  with whichever of #24 and #26 merges second).
- **roll-mcp** (#19 merged into main at 11:15; #24, feature/roll-mcp-next, follows): every route
  in `api::router` resolves to a catalogue row, and http-api.md's
  classes follow the catalogue (restore is `delete`, a domain's PATCH `deploy*`, a job's logs
  `reveal`, hooks `admin`, wake `write`, all of a shell session `admin`). Ours on merge:
  - Done in the stacking merge (1f0caf874, with accounts #18): `/v1/apps/hooks` in rolld's
    `UNSCOPED`; `rlk_` (not `rlh_`, main's automations' prefix) in `SECRET_PREFIXES` and the
    product mapping, and `Access::on_leaked("rlk_")` revoking through
    `Apps::revoke_leaked_hook`; the `"apps"` environment resolver (ephemeral is "preview");
    `api::actor` naming the token's grant, then a signed-in admin, then the operator. The GitHub
    partner pattern is `rlk_[a-z2-7]{40}`.
  - Still open: `put_link` (and create's `links`) checking `Grant::allows(<resource>, reveal)`
    itself (the catalogue's `.lends()` does it at the gate today).
  - `net.rs` and `model::private_ip` move onto mcp's `roll-net`.
  - `apps_set_environment` (mcp-user-tokens) to take `maintenance`, `allow_ips`, `rate_limit` and
    `firewall` (told); new run fields pass through `apps_update_service`'s `run`.
  - #24: `apps_deploy` takes `no_traffic`; `apps_set_environment` covers `PUT
    …/environments/{env}` but `access` and `feedback`; `run.scale` needs read on its stream at
    the gate (a row's lends name each resource's own action), and `apps_update_service` says
    so.
- **roll-previews** (`RollApps`): new Environment fields are ignored safely, `ready` falls into
  its success branch (it never sends `no_traffic`), and apps no longer runs canaries in
  ephemeral environments. Theirs: `unset` the variables it set before and no longer wants (an
  override removed today stays in the `pv-` environment); once secrets is wired, PATCH only the
  link redirects, not `t.vars` (a second copy of project secrets in apps); `catalog()`'s
  memory ignores `[app.previews.run]` and per-environment `run`; `--env K=V` on argv in
  `code-preview/src/docker.rs` (#20's change). Previews sends `feedback: true` (its own rule:
  the setting, a change, an access token) once apps' follow-up is in its tree: the held #21
  answers 400 to an unknown field. One copy of `feedback.js` and its tagger after the merge.
  Screenshots of a roll apps service (previews' `RollApps::screenshot`): `Apps::reachable(service,
  env)` wakes an instance and holds it while the returned `Reachable { container, port }` lives;
  previews' browser joins `--network container:<container>` (capabilities dropped, read-only)
  and loads `http://127.0.0.1:<port>/`, as on its own runtime. None on a runtime with no
  container to join (Fargate). Checked on Docker in `a_private_caller_wakes_a_sleeping_service`.
  Once #26 and #28 are both on main, rolld calls `apps.set_feedback_notes(…)` with an adapter
  over previews' `Engine::notes_for(env, path)` (`p.engine` in `rolld/src/previews/mod.rs`),
  mapping its `Note` onto apps' (`path, selector, note, by, at`, ms).
- **roll-secrets** (see "Secrets and environments"). The wiring is #31
  (`feature/roll-secrets-apps`, after #20 and #26): rolld's `secrets` becomes secrets' `ForApps`
  (resolve with `linked` from `Values.linked`, nothing where secrets has no environment, the
  project's trust of the service; `may_reveal` by `Secrets::caller_of(operator, user, grant)`
  and `allows(Reveal, Scope::at(project, env, "/"))`). Ours on wiring:
  - apps never ensures a secrets environment: an apps environment roll secrets doesn't have
    reads as no secrets (previews ensures its `pv-` ones with their base and trust; ensuring
    again with other values would be a 409 `mismatch`), and a person adds one in roll secrets
    when they want secrets there.
  - Done on apps' side: `SecretSource::digest(project, env)` (secrets' digest of an environment's
    values: versions and keyed fingerprints, no value; `None` by default). Kept with each
    deployment at its switch (`deploy_secrets`); `…/pending` answers `secrets: true` when it moved
    since, the dashboard says "the project's secrets changed" under Redeploy, and `needs_deploy`
    is worked out again the moment it moves instead of after its minute's cache. Secrets' `ForApps`
    answers it with `Secrets::digest(project, env, "/")`.
  - Redeploy when they change (`redeploy_on_secrets`, off by default; `roll apps env
    --redeploy-on-secrets true`, Settings): on the service's owner, each such environment holds
    a long poll of roll secrets' digest of its own (`SecretSource::wait_digest`, 25 s at a time,
    secrets' #79), so a change redeploys within a second or two. The task starts when the
    service is resumed there, when the setting is turned on or the environment resumed, and from
    the minute's pass, a safety net that also checks once; it ends itself when the service moves
    away, the setting goes off, the environment is paused or deleted, or the node stops. A source
    that answers at once (no long poll) is asked every 30 s, an error after 10 s, a deploy on its
    way after 2 s. The check is the digest
    against the live deployment's; when it moved and nothing is on its way, a redeploy of the same
    image with the new values (`settings`, actor `secrets watch`, keyed by the digest and the
    deployment it replaces, so the long poll and the minute's pass, or two nodes' watches across
    a takeover, make one redeploy, and secrets
    that come back to an earlier set get a new one, never the old superseded one). The environment remembers the
    digest it acted on (as a tag watch does), so a redeploy that failed isn't tried again until they change;
    a live deployment with no digest kept starts the count.
  - Done on apps' side: `SecretVar.sensitive` (secrets' #46: a key tagged `sensitive` reaches only
    runtimes, as Vercel's sensitive variables): apps treats it as a sealed variable, in builds and
    instances, never in an answer (`sealed` in the listing, out of `…/variables/values` for
    everyone, references to it sealed too). Secrets' `ForApps` sets it from the key's tags.
  - Done on apps' side: `GET …/variables/values` asks `SecretSource::may_reveal(who, project,
    env)` (false by default) when any value comes from roll secrets, and answers 403 naming them
    if not; so do reading a job's output (`…/jobs/{job}/logs`: a job can print them) and opening
    a shell (it reads them all: roll secrets' choice, so an apps admin without reveal on a
    protected environment doesn't get its values that way); secrets' `ForApps` answers it by its own rules (`Caller::allows(Reveal, …)`, protected
    environments, a token's scope).
  - Seal with secrets' `Keyring` (`ROLL_SECRET_KEY_PREVIOUS`, `ROLL_SECRET_KEY_KMS`), not
    `ROLL_SECRET_KEY` alone, so a key rotation by secrets' guide keeps apps' sealed variables,
    registry logins and target URLs readable, and KMS works on a fleet.
  - `SecretSource` gains `digest`, on `Secrets::digest(project, env, path, caller)` (what `roll
    run --watch` polls: keys, versions, fingerprints and links; it decrypts nothing and isn't
    audited), for `needs_deploy`'s minute-long cache. Since secrets' #74 a live link's digest
    follows its value too (a rotated database password moves it), so with
    `redeploy_on_secrets` on a rotation redeploys.
  - A value whose source is a secret needs `Caller::allows(Reveal, Scope::at(project, env,
    path))` on top of apps' `reveal` (secrets confirmed: otherwise apps' reveal gets around
    protected environments). `Keyring::new(current, previous)`, the same key material.
  - `rolld/src/secrets/syncs.rs`'s `PublicOnly`: #19 is on main, so #20 takes `roll_net`
    (wip/secrets-on-mcp's resolution, which still needs main's #18).
  - Once people manage secrets on their own projects, a canvas stops proving ownership: a
    person-owned project counts only for services that person owns (apps' `SecretSource`, and
    already scaling on a stream).
- **roll-search** (#22 on main; #29 follows): its conflicts with apps are additions side by side
  (`Product`/`Kind` arrays grow). A link to a search cluster gets its own API key through
  apps' `Resources::link`/`unlink` (#26). Done on feature/search-app-links (after #29):
  `LinkKind::Search` (`ALL`, `as_str`, `parse`), handled in rolld's `RollResources` by roll
  secrets' search provider like buckets and streams (`secrets::link_provider("search")`, which
  calls search's keys route with `access: "data"`); roll-access's `link_target` maps `search` to
  search's `cluster` (a link needs `reveal` on it). The key is data-only (no security privileges),
  its variables `ELASTICSEARCH_URL` and `ELASTICSEARCH_API_KEY`; an OpenSearch cluster's is an
  internal user with a data-only role, `OPENSEARCH_URL`, `OPENSEARCH_USERNAME` and
  `OPENSEARCH_PASSWORD`; revoking removes it.
- **Conflicts with apps**: previews merges cleanly. With main (and #24), six files from apps
  merging feature/accounts rather than main's squash of #18 (`AGENTS.md`, `rolld/src/{auth,
  cluster,main}.rs`, `tests/auth.rs`, `docs/workspace.md`): take apps' side, keeping one early
  `workspace::open`, and one `/v1/apps/hooks` row in `tests/auth.rs`; merging main into
  feature/roll-apps clears them. With search: additions side by side (`[Product; 10]`,
  `[Kind; 8]`, the dashboard's kind maps). With secrets: eleven files, all trivial but
  roll-workspace's `delete` (keep main's `delete(scope, id)` with `on_delete`);
  `routeTree.gen.ts` is regenerated, not merged.
- **Every product sharing the system database** reserves its own share of
  `ROLL_SYSTEM_DB_CONNECTIONS`: apps has `DbBudget::apps`, previews its own share; mcp still
  borrows the workspace's. `DbBudget`'s test (`split(Some(12))`, at most 12) can't pass once
  apps, search, secrets and previews are combined (seven weighted pools, four fixed and two
  reserved make 13 at least): move it to 16 and count the fixed pools as reserved.
- `scripts/test-apps.sh` runs its own `cargo test -p` calls, not `scripts/lib/gate.sh`'s shared
  package set (adding apps there would slow roll code's and agents' gates).
- Merge order: mcp, secrets, search, apps (then the secrets wiring), previews.

## Next

Ranked; the top item not marked *waiting* is what's being worked on. Reviews on
2026-11-13 and 2026-11-14 (a second product review of the experience, a product review of what one engineer can close here now, bugs and security in the newest work, the product against Railway, Render, Fly,
Vercel, Cloud Run and Heroku, and integration with the other branches' heads) set this order;
on 2026-11-15 a product review of what's left that one engineer can build here (against the
same six, each gap checked in the code) put items 1–9 first, and a bug review of the day's
commits was fixed (a run-setting typo in one environment's block failed every environment's
build; a drain naming a deleted service couldn't be edited; route groups filled from the oldest
lines; the pre-deploy container had no `ROLL_INSTANCE`; another environment's routes stayed on
the charts). A second product review the same day (with the review pack's screenshots of the real UI)
put items 1–11 first. A second review that day (after #78, against the same six and Netlify) put the six items above first and found five bugs, fixed: a tag push or any other event on a Git URL service's hook redeployed production; an environment whose first deploy failed was never followed; the watch ignored `[skip ci]`; an environment with no branch took any branch's push; `wait_for_ci` was accepted where nothing reads it. A review on 2026-11-16 (the product against the same six, each gap checked in the code) put items 1–6 first and found three bugs, fixed: links froze what they read (a rotated NoSQL login), `PUT …/environments/{env}` wrote a hold and a name before its last checks, and unlinking stopped at the first credential it couldn't revoke. Another that evening (after watch paths, the previews hand-off, the GitHub Action and more Terraform, against the same six and Netlify) found three gaps, all built since (private repositories on any Git host, one webhook for every environment, and linked services reached from `roll apps exec`, which it took for missing), and five bugs, fixed: a Git URL hook deployed whatever commit the body named; roll.toml's watch paths were ignored for Git URL services; the Action deployed a pull request's merge commit and read only its first 100 comments; a Bitbucket push was read for its last branch only; a Git host without fetch filters lost `[skip ci]`. A review after #99 merged (the same seven, and the other threads' open branches) put response caching and analytics in, and found: a fork's pull request would have been a trusted preview through a receiver that dropped `pull` (now `takes_pulls`), previews got production link credentials (now none in an ephemeral environment), a same-repository pull request from a followed branch made a second preview, and the digest note above was stale. A review of the stack after #115 (caching, analytics, traces, the GitHub App, against the same seven and the other threads' branches) found nine bugs, fixed: trace intake on rolld's API would have opened the API to every container through the link relay (it's on the apps listener now); the trace token showed in variable listings; the cache's key ignored the host, it survived an owner's release, had no node-wide bound, wasn't metered, refreshed private environments in vain and counted refreshes as page views; the App's events could stall behind a sleeping service and were queued after GitHub's answer; previews' GitHub Deployments weren't transient. It put the six items above after branch previews. Egress rules for previews were built next ("Egress"), leaving the fleet's half and closed services' private wakes as items; then sign-in-gated environments ("Signing in to an environment"), whose sessions end within minutes of the person losing the project; then the proxy's own span in traces, and the deploy action's OIDC sign-in (its server side is MCP's #114). Cached answers across nodes then went behind the fleet's first run: they only pay on a fleet serving traffic. Real-user Web Vitals came next ("Web Vitals"), then OTLP logs and metrics ("Traces"), and closed environments' private wakes ("Egress"). A review on 2026-11-17 (the product against the same seven, every gap grepped in the code) put items 2–11 in, and a bug and security review of that night's work found seven problems, fixed: a closed environment could join an open network another environment's name made; a plain HTTP request through the egress proxy could name another host or ride a reused connection; egress proxies were shared across environments by a 48-bit digest (refusals seen elsewhere); `/__roll/visit` could redirect off the site with a tab; a signed-in visitor's request carried the environment's shared token to the app; an unreadable egress read as open; and closed services' lists add up (now said). Those ten gaps were built that day and the next (#161–#186: holds, secret files, vercel.json and netlify.toml rules, memory scaling, grouped errors and `error.new`, CI on any Git host, imports, manual rollouts, tag purges, health check settings, files in builds, traces as one log entry). A review after them (the same seven, each gap grepped in the code, nothing that waits on AWS or another thread) put items 2–10 in. Those were built on 2026-11-17 (#191–#224), and a bug and security review of the day found nine problems, fixed in #223; another product review that evening put items 2–9 in and skipped builds (built at once), and undoable deletes and comparing environments were built next; a review of previews' #234 found that a closed environment's build had an open network (now a network of its own, out only to public hosts) and that closed instances reach the host's services (now an isolated gateway too, reached through their sandboxes' sockets). CPU and memory per instance came next, then `report: false` (previews' one voice on a pull request). A custom domain on another port needs each instance to publish more than one port (Docker's loopback publish, the closed sandbox's socket, the live state's address), so the build timeout went first. It was built right after. Backups' schedules, retention and restore elsewhere came next; roll previews' review then found that closed instances with long names were never reached (a unix socket path past 107 bytes; fixed on main in #271), and asked for a project's preview domain and the opener on a pull request (both built next, with a steadier alert test). Image optimization came after them, on roll storage's image engine. A product review that evening (against Railway, Render, Fly, Vercel, Netlify, Heroku and Cloud Run, each gap grepped in the code) put items 1–8 in; branch and pull-request previews were built. The console
("A console") was built next, then staged changes across a project ("What the next deploy changes"), then syslog and OTLP drains ("Log drains"), then secrets changes redeploying within seconds (secrets' #79 landed; "With the other platform branches"), then sidecars ("Sidecars"), then campaigns, devices and custom events ("Visitors"), then Scala, Clojure, Gleam and start.sh builds ("Builds in detail"), then static sites' forms ("Forms"). A product review that evening (against the same seven, each
gap grepped in the code) put items 1–9 in, and found five bugs in the forms work before it
landed, fixed: a word in another attribute's value marked a form; one sender could make every
visitor's form refuse; a POST that was no form's got a form's errors; paging skipped a
submission sharing the boundary millisecond; a deployment's own URL filled the live inbox. A review of
the day's sidecars, analytics and builds found six more, fixed: a digest-pinned sidecar image
skipped the registry check; runs and consoles ran their sidecars unmetered; sidecars' load read as
the app's for autoscaling; invented event properties grew a read without bound; sbt's script name
dropped `_`, missed `"""…"""` and had no tree of its own without a plugin; a dev start.sh turned a
static site into a container. Firewall rules on more than the path were built next, then PagerDuty and Opsgenie targets
("Notifications"), then external proxy rewrites and conditional rules ("Static sites"), then uptime
checks ("Uptime checks"), then heartbeats ("Heartbeats"), then image scans ("Vulnerability
scanning"), then objectives ("SLOs"), then Prometheus scraping ("The app's Prometheus endpoint"), then split tests ("Split tests"), then edge functions ("Edge functions"). A review that morning (the product against Railway, Render, Fly, Vercel, Netlify, Heroku and Cloud Run, each gap grepped in the code) put items 2–6 in; autoscaling on the app's own gauges was built first ("Workers that follow their work"), then a password ("A password for an environment"), then deploy freezes ("Deploy freezes"), then status pages ("Status pages"), then Teams and Google Chat targets ("Notifications"). A review after them (the same seven, and what they offer past the basics, each gap grepped in the code) put items 2–6 in; SBOMs came first ("Vulnerability scanning"), then rollouts that watch the objective ("Gradual rollouts"), then floors by the clock ("Floors by the clock"), then traffic mirroring ("Traffic mirroring"), then log-based metrics ("The app's Prometheus endpoint"). A review after the 17-PR turn landed (2026-11-19 10:00: the product against the same seven, and the other threads' branches: secrets' maker references for `roll://` and Git logins, MCP's tools for the new routes, previews' environments) put pause schedules first (built: "Paused by the clock") and items 2–4 in; status page incidents came next ("Status pages"), then rollouts that watch latency ("Gradual rollouts"), then audit log streaming ("Log drains"). A review after them (2026-11-19 afternoon: the product against the same seven, each gap grepped in the code, and a bug and security review of the day's stack, #408–#423) put items 1–8 in and found twelve bugs, fixed: an incident a check opened stayed open after a restart, a removed check or a paused environment; putting a never-live build live, a base image's rebuild and a secrets change went past a freeze (the last one never retried); incidents named what the page doesn't show (previews' heartbeats among them); every visit to a status page read every tenant's environments; mirrored copies were unbounded and judged by the target's rollout; a busy incident couldn't be resolved, and a page's incidents outlived it; a Teams card rendered links from tenant text; a p95 step was a bucket wide (41%); an upload couldn't override a freeze; one failed try read as down on the page. Skew protection for web services was built first ("Skew protection"), then a hard concurrency limit ("Saturation"), then the CLI for freezes, any environment setting and status pages ("Deploy freezes", "Status pages"), then the dashboard's Files panel (under Shell), cache purge and log metrics editor, then snippets ("Snippets"), then `roll://` variables ("Secrets and environments"), then the freeze override as admin (MCP's review), then queue links, then a deploy under traffic on real Docker and the deployments and variables component tests.

1. **Runtime config** (Vercel's Edge Config and flags): values changed without a deploy, read at
   a roll-internal address, as a file through variables as files, and inside edge functions;
   changes audited.
2. **An ignore command** (Vercel's `ignoreCommand`, Netlify's `[build] ignore`), ranked down:
   watch paths already skip a deploy that touches nothing relevant, and the usual command
   (`git diff --quiet HEAD^ HEAD ./`) needs git history, which a deploy's fetched tree doesn't
   have. It needs a sandboxed run of the command on a checkout with the previous revision.
3. **Variables that belong in roll secrets** (roll secrets' review): `roll://` references to the
   service's own environment are built ("Secrets and environments"). Left: a secret-looking
   variable pasted into the panel offers "Move to roll secrets" (into the project environment,
   leaving a reference behind). *Waiting on roll secrets' #407 (maker references, head
   4ea68773a, batch 5):* a reference into another environment, and a Git login's `secret` as
   one. Both are bound to the person who set it, through `bind_ref` for another environment, or
   `bind_ref_runtime(resource, reference, caller, runtime_env)` once apps has checked that the
   caller holds `deploy` there (a sensitive key, in exactly that environment; the maker must also
   be able to reveal the folder). The `Maker` string (`usr_…@runtime:production`) is kept
   beside it and is never a tenant's to write. Each fetch reads it with `resolve_as`
   (`digest_as` for "needs deploy"), which re-checks the maker every time.
4. *Waiting on roll secrets' #357 (its batch 4).* **A workload
   identity token in each instance** (Vercel's OIDC federation, Fly's, Cloud Run's): roll
   secrets' #68 issuer mints `sub = roll-apps:<project>:<service>:<env>` (`SecretSource::issue`,
   at most an hour, rotated keys); apps' half is delivery: a refreshed token per instance read
   from its link endpoint, a per-request header for web services, and `aud` per environment.
5. *Waiting on the user.* **Signed-in people, not just admins** (every competitor; roll search and
   secrets do): `roll_auth::SignedIn` scoped through `Workspace::trusts`, others' services 404.
   Letting non-admins run code on a node's Docker is the user's call first.
6. *Waiting on the fleet's first run.* **Cached answers across nodes and at the edge** (Vercel's, Netlify's): today the owner's
   proxy alone keeps them; the storage CDN's purge outbox could carry the purges.
   Across nodes means a node that doesn't own the service answering from its own copy, which
   needs the owner's say that the environment has no gate (access, allow list, firewall, rate
   limit) on each answer, a generation per environment for purges, and hits metered and logged
   where the owner reads them: only worth it once the fleet serves traffic.
7. **Egress on the fleet** (Fargate): a security group per closed environment's tasks with no
   outbound rule but the egress proxy's, and the proxy as a task or on the node; until then
   Fargate refuses `egress: roll` (409). *Waiting on the fleet's first run.*
8. *Waiting on queues as a workspace resource.* **Workers that follow a queue's depth**, as they
   follow a stream group's lag now (a queue's project is what keeps another tenant's depth
   unread).
9. **Region**: the region shown on the overview and each environment, defaulting to the project's
   databases', then a choice per environment once the fleet has a second region (Railway's,
   Fly's). *Waiting on the fleet.*
10. **Static outbound IPs**: a NAT gateway with Elastic IPs on the fleet's subnets, listed in
   Settings, without stopping services from sleeping (Railway's and Render's). A roll-hosted app
   linked to a search cluster with an IP allowlist (search #243) needs them on the list; until
   then such a cluster stays open (on one node, the link relay's loopback is what it sees). *Waiting on the
   user's first AWS run.*
11. *Waiting on the edge's owner (roll cloud).* **gRPC from the internet**: roll-edge's HTTPS
   listener offering `h2` (ALPN `h2, http/1.1`; hyper's `http2::Builder` when `h2` is chosen,
   with the same graceful drain) and setting `Host` from `:authority` (HTTP/2 has no `Host`,
   and every product behind the edge routes on it), so Cloudflare's gRPC reaches h2c apps.
   Done here: `run.h2c` to the app, trailers through the proxy. Then TCP services.
12. **roll secrets as the env store**: the wiring is done (deploys resolve the project's secrets,
   digests mark "needs deploy", roll secrets' reveal rules, linked and sensitive keys, dynamic
   credentials refused, new environments ensured); what's left is service variables kept in roll
   secrets at `/services/<name>` (one store for both), and links as per-environment secrets
   links.
13. *Waiting on the user.* The fleet runtime in the stack: the roles, project, security group and
   settings listed in "Not done yet", a first run against AWS with its cold starts measured (a
   warm window or `min_instances` 1 by default there until a wake is fast), and
   allocation-based metering for Fargate.
14. *Waiting on the edge's owner (roll cloud).* **Wildcard certificates**: DNS-01 on a delegated
   `_acme-challenge.<base>` CNAME into a zone roll writes (`roll_edge::acme`'s DNS solver at
   another name), then `DomainTls::wildcards` true there; the routing and verification are done.
15. *Waiting on the edge's owner (roll cloud).* **Bring your own certificate** (Vercel's,
   Heroku's SNI, Fly's import): `PUT …/domains/{host}/certificate {cert_pem, key_pem}`, sealed,
   served by SNI at the edge (and Cloudflare's custom certificates), expiry through
   `domain.expiring`.
16. *Waiting on a static build (no musl target on ai-server, and adding one is the shared toolchain's).* **A waker that takes every caller while the app starts**: busybox `nc -l` takes one, so a burst
   of private calls to a stopped service loses all but the first (refused). A waker of roll's own
   (roll-fetch's `wake` mode, a static binary mounted read-only) that accepts each, holds them,
   and hands them over once the app listens; the cheap accept count already tells rolld.
17. Custom domains' TLS on the fleet for real (the zone settings in "Not done yet", authenticated
    origin pulls for custom hostnames, then a first domain through Let's Encrypt and Cloudflare).
18. **The rest of a cold start**: done: the network in a sandbox (258 → 175 ms); next, snapshot
    restore (CRIU, which needs the host's Docker daemon with checkpoints on: the user's call), or
    Firecracker microVM hosts on the fleet.
19. EFS volumes and Cloud Map private DNS on the fleet; TCP services on the fleet (the edge carrying TCP to a service's owner).
