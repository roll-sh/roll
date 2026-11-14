# roll apps HTTP API

The contract between `crates/apps-control` (`/v1/apps`) and its clients:
`apps/dashboard/src/lib/apps-api.ts`, `roll` (`crates/roll-cli/src/apps.rs`) and the MCP tools on
feature/roll-mcp. Change them together.

- Bodies are JSON unless a route says otherwise. Every error is `{"error": string}` with the
  status below. Times are unix milliseconds. Ids: `svc_…`, `dep_…`, `run_…` (12 lowercase base32
  characters after the prefix).
- The routes sit behind rolld's gate (the operator token, or roll MCP's scoped tokens). roll
  apps checks no token itself, except a deploy hook's own (`/v1/apps/hooks/…`, the secret in its
  URL). A project at its spend limit answers 402 on what would run (deploys, jobs, wakes).
- `{env}` is an environment name: 1–32 characters of `[a-z0-9-]`, not starting or ending with
  `-`. `production` is the one environment that `deploy` (not `write`) guards.
- Live streams are server-sent events (`text/event-stream`): each event is `data: <json>`; the
  stream starts with what is already there (`?tail=N` lines) and follows until the client goes.

## Routes and actions

`action` is what the permissions catalogue (feature/roll-mcp, product `apps`) classifies the
route as. "deploy*" means `deploy` when `{env}` is `production`, else `write`.

| Route | Body | Answer | action |
| --- | --- | --- | --- |
| `GET /v1/apps` | | `Overview` | read |
| `POST /v1/apps/plan` | `PlanRequest` | `Plan` | read |
| `GET /v1/apps/services?project=` | | `Service[]` | read |
| `POST /v1/apps/services` | `NewService` | 201 `Service` | deploy* (a repo or image source deploys production at once; write for an upload source) |
| `GET /v1/apps/projects/{project}/services` | | `Service[]` | read |
| `POST /v1/apps/projects/{project}/services` | `NewService` (`project` may be left out) | 201 `Service` | deploy* (as above) |
| `GET /v1/apps/services/{id}` | | `Service` | read |
| `PATCH /v1/apps/services/{id}` | `ServicePatch` | `Service` | write |
| `DELETE /v1/apps/services/{id}` | | 204 | delete |
| `GET /v1/apps/services/{id}/activity` | | `Event[]` (`audit.*`, newest first, 200): who changed what, never a value | read |
| `GET /v1/apps/services/{id}/links` | | `Link[]` | read |
| `PUT /v1/apps/services/{id}/links/{kind}/{ref}` | `{prefix?}` | `Link` (a kind that makes a credential per link makes it here, once; setting the link again keeps it) | write |
| `DELETE /v1/apps/services/{id}/links/{kind}/{ref}` | | 204 (revokes a credential the link made) | write |
| `GET /v1/apps/projects/{project}/notifications` | | `NotifyTarget[]` (the URL never returned, only its host) | read |
| `POST /v1/apps/projects/{project}/notifications` | `{ kind?: "webhook" \| "slack" \| "discord", url, events?, environments?, services?, previews? }` (environments: names; services: names or ids, kept as ids; previews: default false) | 201 `NotifyTarget` (+ `secret` once, for a webhook) | reveal for a webhook (its secret comes back), write for Slack and Discord |
| `DELETE /v1/apps/projects/{project}/notifications/{nid}` | | 204 | write |
| `POST /v1/apps/projects/{project}/notifications/{nid}/test` | | `{ ok, code, error }` (sent now, not retried) | write |
| `GET /v1/apps/projects/{project}/notifications/{nid}/deliveries` | | `Delivery[]`, newest first (50) | read |
| `GET /v1/apps/projects/{project}/drains` | | `LogDrain[]` (the URL never returned, only its host) | read |
| `POST /v1/apps/projects/{project}/drains` | `{ url }` (https, a public host) | 201 `LogDrain` + `secret` once | reveal (its secret comes back) |
| `DELETE /v1/apps/projects/{project}/drains/{did}` | | 204 | write |
| `GET /v1/apps/projects/{project}/limits` | | `SpendLimits` | read |
| `PUT /v1/apps/projects/{project}/limits` | `{ alert_usd: number \| null, hard_usd: number \| null }` | `SpendLimits` (this month's alert and cap are decided again within 30 s) | admin (the hard limit pauses the whole project) |
| `GET /v1/apps/events?after=&limit=` | | `{ events: Event[], next }`: what happened to services, in order, after a cursor | read |
| `GET /v1/apps/services/{id}/changes` | | server-sent events: `ready`, then a `changed` (data `{}`) whenever a deploy moves, an instance wakes, sleeps or crashes, or someone changes the service; the page reads it again (on the owner, in memory; it carries nothing itself) | read |
| `PUT /v1/apps/services/{id}/registry` | `{ username, password, server? }` | `Service` (a private registry's login for an image source; sealed, never returned) | write |
| `DELETE /v1/apps/services/{id}/registry` | | `Service` | write |
| `GET /v1/apps/services/{id}/environments` | | `Environment[]` | read |
| `PUT /v1/apps/services/{id}/environments/{env}` | `EnvironmentPut` | `Environment` | deploy*; with `variables_from` also reveal in that environment (it copies its values) |
| `GET /v1/apps/services/{id}/environments/{env}` | | `Environment` | read |
| `DELETE /v1/apps/services/{id}/environments/{env}` | | 204 | delete |
| `POST /v1/apps/services/{id}/environments/{env}/deploys` | `NewDeploy`, or a gzipped tar (`application/gzip`) | 202 `Deployment` | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/deploys?before=` | | `Deployment[]`, newest first, 100 at most; `before` (unix ms, the last one's `created_at`): the next page | read |
| `GET /v1/apps/services/{id}/environments/{env}/deploys/{dep}` | | `Deployment` | read |
| `GET /v1/apps/services/{id}/environments/{env}/deploys/{dep}/logs?follow=&tail=` | | `LogLine[]` or SSE | read |
| `GET /v1/apps/services/{id}/environments/{env}/deploys/{dep}/commits` | | `DeployCommits`: what a repo deployment brings over the live one (for the live one: the one live before it), along its first parents, newest first, at most 50 | read |
| `POST /v1/apps/services/{id}/environments/{env}/deploys/{dep}/rollback` | `{ keep_variables? }` (default false: the variables come back as they were when it was live) | 202 `Deployment` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/rollback` | `{ keep_variables? }` | 202 `Deployment`: back to the deployment live before what runs, walking back through rollbacks (after B → A the next goes before A, never forward to B); 409 when there's none | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/redeploy` | `{ message? }`; `Idempotency-Key` header optional (a retry with it answers the first call's redeploy; a key a deploy of another kind used is a 409) | 202 `Deployment` (trigger `settings`) | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/promote` | `{ from, message? }` | 202 `Deployment` (trigger `promote`: `from`'s live image, no build, with `env`'s variables; `rollback_of` names the promoted deployment) | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/hook` | | 201 `{ path }`: a new deploy hook, its path shown once (prefix the API's base URL; the token is `rlk_…` with a checksum, so secret scanning finds a leaked one); the old one stops working | admin (a standing credential that deploys) |
| `DELETE /v1/apps/services/{id}/environments/{env}/hook` | | 204 | admin |
| `POST /v1/apps/hooks/{id}/{env}/{token}?image=` | | 202 `Deployment` (actor `deploy hook`); `image`: another tag or digest of an image service's own repository, else 400; any token that isn't the live hook's is a 404. No API token: the URL is the secret (rolld's `RULES` let it through to the hook's own check; access lines log the route pattern, never the token). Each deploy is `audit.hook.deployed` in the activity, with the caller's address | none (the URL) |
| `GET /v1/apps/services/{id}/environments/{env}/volume/backups` | | `Backup[]`, newest first (7 kept) | read |
| `POST /v1/apps/services/{id}/environments/{env}/volume/backups` | | 201 `Backup` (now) | write |
| `POST /v1/apps/services/{id}/environments/{env}/volume/backups/{bid}/restore` | | `Backup` (the volume as it was; the environment asleep meanwhile) | delete (it replaces the volume's files) |
| `POST /v1/apps/services/{id}/environments/{env}/deploys/{dep}/cancel` | | `Deployment` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/restart` | | 202 `Environment` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/sleep` | | `Environment` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/wake` | | `Environment` | write (it starts compute someone pays for; a visitor's request wakes a public environment through the proxy, not this route) |
| `GET /v1/apps/services/{id}/environments/{env}/variables` | | `Variables` (values masked) | read |
| `GET /v1/apps/services/{id}/environments/{env}/pending` | | `{ variables: { added, removed, changed } \| null, run: string[], secrets: boolean \| null }` (secrets: the project's secrets changed since, by roll secrets' digest; null without one): what the next deploy changes (names, never values; variables against the snapshot kept at the live deployment's switch, null without one; run: fields the service sets that the live deployment doesn't run with). Links and project secrets move `needs_deploy` too, unnamed | read |
| `GET /v1/apps/services/{id}/environments/{env}/variables/values` | | `{[name]: value}`, never a sealed variable (nor a reference that reads one); 403 naming them when some come from roll secrets and roll secrets wouldn't show them to the caller (`SecretSource::may_reveal`) | reveal |
| `PATCH /v1/apps/services/{id}/environments/{env}/variables` | `{set?: {[name]: value}, unset?: string[], seal?: string[]}` (set, then unset, then seal: names set here or already set; 400 for another; nothing unseals, a new value stays sealed) | `Variables` | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/logs?follow=&tail=&since=&before=&instance=&deployment=&stream=&q=&http=&status=&min_ms=&by=` | | `LogLine[]` or SSE; `before` (unix ms) reads back to a time (not followed), `instance`/`deployment`/`stream` narrow it (followed too), `X-Roll-Search-Truncated: 1` when a read back stopped short; `http=1`: the proxy's access log (`stream: "http"`, text `AccessLine`), `status` (`500`, `5xx`, `400-499`), `min_ms` and `by` (one reason, or `any`: every answer roll gave itself) filtering it | read |
| `GET /v1/apps/services/{id}/environments/{env}/metrics?minutes=&format=` | | `Metrics`; `format=prometheus`: the last finished minute as Prometheus text (`roll_apps_requests_per_minute`, `…_errors_per_minute`, `…_latency_ms{quantile}`, `…_cpu_seconds_per_minute`, `…_memory_bytes`, `…_instances`, `…_cold_starts_per_minute`, `…_cold_start_ms`, labelled `service`, `service_id`, `environment`) | read |
| `GET /v1/apps/services/{id}/environments/{env}/usage?days=` | | `Usage` | read |
| `GET /v1/apps/services/{id}/environments/{env}/runs` | | `CronRun[]` | read |
| `POST /v1/apps/services/{id}/environments/{env}/runs` | | 202 `CronRun` (run now) | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/runs/{run}` | | `CronRun` | read |
| `GET /v1/apps/services/{id}/environments/{env}/runs/{run}/logs?follow=&tail=` | | `LogLine[]` or SSE (ends with the run): its container's lines | reveal (a run's output can be the variables) |
| `POST /v1/apps/services/{id}/environments/{env}/runs/{run}/cancel` | | `CronRun` (409 unless it's running) | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/shell?instance=&cols=&rows=` | WebSocket | a terminal: binary both ways, text `{"resize":[cols,rows]}` | admin (a shell runs anything with the environment's variables, as agents' shells) |
| `GET /v1/apps/services/{id}/environments/{env}/tunnel` | WebSocket | one TCP connection to the web service's own port, binary both ways (`roll apps proxy`); wakes an instance and holds it while open; at most 20 per environment (409); 409 for a worker, a cron service or a static site | admin (it reaches the port past the allow list, access and maintenance) |
| `POST /v1/apps/services/{id}/environments/{env}/shell/sessions` | `{ instance?, cols?, rows? }` | 201 `{ id }` (a shell over HTTP: the dashboard's terminal) | admin (a shell runs anything with the environment's variables, as agents' shells) |
| `GET /v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}/screen` | | SSE: base64 chunks (the scrollback first), then `event: closed` | admin (a shell runs anything with the environment's variables, as agents' shells) |
| `POST /v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}/keys` | `{ data }` (base64, at most 64 KiB) | 204 | admin (a shell runs anything with the environment's variables, as agents' shells) |
| `POST /v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}/resize` | `{ cols, rows }` | 204 | admin (all of a shell session is) |
| `DELETE /v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}` | | 204 | admin (what closes an admin's shell is admin) |
| `GET /v1/apps/services/{id}/environments/{env}/jobs` | | `Job[]`, newest first | read |
| `POST /v1/apps/services/{id}/environments/{env}/jobs` | `{ command, timeout_s?, tasks? }` (tasks 1–100: that many containers, ten at a time, each with `ROLL_TASK_INDEX` and `ROLL_TASK_COUNT`; the job fails on the first task that does) | 202 `Job` (with `tasks`) | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/jobs/{job}` | | `Job` | read |
| `GET /v1/apps/services/{id}/environments/{env}/jobs/{job}/logs?follow=&tail=` | | `LogLine[]` or SSE (ends with the job) | reveal (a job's output can be the variables) |
| `POST /v1/apps/services/{id}/environments/{env}/jobs/{job}/cancel` | | `Job` | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/domains` | | `Domain[]` | read |
| `POST /v1/apps/services/{id}/environments/{env}/domains` | `{hostname}` | 201 `Domain` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/domains/{host}/verify` | | `Domain` | read |
| `PATCH /v1/apps/services/{id}/environments/{env}/domains/{host}` | `{ redirect_to: string \| null }` | `Domain` (another hostname of the environment that serves it: no loops or chains; null serves the app again) | deploy* (it changes what a hostname serves) |
| `DELETE /v1/apps/services/{id}/environments/{env}/domains/{host}` | | 204 | delete |

- Creating a service makes its `production` environment, places its card on the project's canvas
  (at `x`, `y`, or the first free spot when both are left out or 0), and, for a repo or an image,
  queues the first deploy at once. An upload service waits for `roll up`.
- In `PATCH` bodies (`build`, `run`) a field set to `null` goes back to its default; fields left
  out stay.
- `…/deploys/{dep}/logs?follow=1` ends when the deployment is done (at once for one that is).
- `…/usage` counts whole UTC days (`day` is days since 1970-01-01), today included, days without
  use as zeros.

A `roll up` upload (`application/gzip`) may carry `X-Roll-Message` (percent-encoded UTF-8: the
checkout's last commit, by default) and `X-Roll-Revision` (its sha), shown on the deployment.

A log's `q` (1–200 characters) returns the lines that contain it, any case, from memory and then
the stored chunks newest first (back to `since`), at most `tail` (2000); it reads at most 64 MiB
of stored logs and says so with `X-Roll-Search-Truncated: 1`.

`POST …/deploys` takes an `Idempotency-Key` header (1–200 printable ASCII): a retry with the same
key in the same environment, within a day, answers the deployment the first call queued instead of
queuing another. The same key with another `revision` or `message` is a 409.

Statuses: 400 a bad body, name, env or query; 404 no such service, environment, deployment or
domain; 409 a name or hostname that's taken, a rollback to a deployment with no image, a redeploy of an environment with nothing deployed, a cancel
of a finished deployment, a second volume instance; 413 an upload over 512 MiB; 503 with
`Retry-After` while the service's owner moves.

## Shapes

```ts
type Overview = {
  services: Service[];
  runtime: "docker" | "fargate";
  /** Where generated URLs live: `up.roll.site`, `apps.localhost`. */
  domain: string;
  prices: Prices;
};

type Prices = { cpu_second: number; gb_second: number; egress_gb: number; build_minute: number };

type Source =
  | { kind: "repo"; repo: string /* owner/name in roll code (GitHub repos are mirrored there) */; root?: string }
  | { kind: "image"; image: string /* docker.io/library/nginx:1.27 */ }
  | { kind: "upload" } /* `roll up`: each deploy carries its tree, kept only until its build ends */;

type Build = {
  /** auto: detect (a Dockerfile wins); dockerfile: use `dockerfile`. */
  builder: "auto" | "dockerfile";
  dockerfile?: string; // path in the tree, default "Dockerfile"
  install?: string;    // overrides the detected commands
  build?: string;
  start?: string;
  /** A repo source deploys a push only when it changes a path these match (gitignore-style, from
   *  the repo's root, as CODEOWNERS; at most 50). roll.toml `[app] watch` wins over it. */
  watch?: string[];
  spa?: boolean;            // a single-page app (a page path with no file answers index.html); unset: detected
  target?: string;          // the tree's own Dockerfile's stage to build (--target): letters, digits, _ - . (64 at most)
};

type Run = {
  type: "web" | "worker" | "cron";
  port?: number;            // web: the port the app listens on (default from the plan, else 8080; PORT is set to it)
  start?: string;           // overrides the image's command
  pre_deploy?: string;      // once per deploy, in a container of the new image with its variables and
                            // the project network (no volume), before the switch: a migration. A
                            // non-zero exit fails the deploy; the old one keeps serving
  schedule?: string;        // cron: five fields, in `timezone` (UTC when unset)
  timezone?: string;        // cron: an IANA zone (Europe/Budapest), its daylight saving kept; 400 for an unknown one
  timeout_s?: number;       // cron: a run's limit (default 3600); web: a request's time to its answer's head, 1–3600, then a 504 (unset: no limit; upgrades and streams that began are the app's)
  min_instances: number;    // web: 0 scales to zero (default 0); worker: at least 1
  max_instances: number;    // default 3
  concurrency: number;      // requests in flight per instance before scaling out (default 50)
  cpu: number;              // vCPU per instance, 0.25–8 (default 0.5)
  memory_mb: number;        // 256–16384 (default 512)
  health_path?: string;     // web: GET it for a 2xx/3xx; none: the port accepting a connection.
                            // Also checked every 10 s while running: 3 failures in a row replace the instance
  health_timeout_s: number; // default 60
  suspend_after_s: number;  // default 30; 0 never suspends
  sleep_after_s: number;    // default 600; min_instances > 0 never sleeps below min
  volume?: { mount: string; size_gb: number };
  alerts: { memory_pct: number; error_pct: number; p95_ms: number; log_match?: string; log_count?: number }; // default 90, 5, 0 (0 off): `alert.*` events over five minutes; log_match: text its stdout/stderr holds log_count times (default 10) in five minutes, `alert.log`
  /** A gradual rollout (README "Gradual rollouts"): increasing percents 1–99 (at most five; []
   *  switches at once, the default), each for step_s (1–3600, default 120); a step whose 5xx
   *  share reaches error_pct (1–100, default 5) of 20+ requests, and beats the old one's, rolls
   *  back. Web services only; refused with a volume. */
  rollout: { steps: number[]; step_s: number; error_pct: number };
  /** The app speaks HTTP/2 without TLS (a gRPC server): the proxy and health checks use h2c,
   *  `te: trailers` passes, trailers come back. A WebSocket upgrade stays HTTP/1.1. Default false. */
  h2c?: boolean;
  /** A worker only: its instances follow `group`'s lag on `stream` (a roll stream on the
   *  service's project; another's is never read), one per `per_instance` (default 1000) behind,
   *  within min_instances–max_instances; read every 10 s on the owner, scaled in one at a time. */
  scale?: { stream: string; group: string; per_instance?: number };
  /** false: no public URL (its up.roll.site names, its deployments' and its custom domains are
   *  404s like an unknown address, at once); the private `<svc>.<env>.internal` still answers.
   *  Default true. Environment.url is null while it's false. */
  public?: boolean;
  /** A replaced or scaled-in instance's time to finish: its requests in flight, then SIGTERM,
   *  SIGKILL when it's up (Render's maxShutdownDelaySeconds). 1–300; unset (or null) 30. */
  drain_s?: number;
  /** A web service also scales on CPU (Render's target CPU): while its awake instances
   *  average over this percent of their CPU for a minute another starts (one at a time, up
   *  to max_instances), and fewer stay once they'd be under it. 10–100; unset, requests only.
   *  Only what the runtime measures counts (Docker's cgroup; not Fargate, which bills by size). */
  cpu_target?: number;
  /** A worker that stops: started again `always` (default), on `on_failure` only (a nonzero
   *  exit or out of memory, up to restart_tries in a row, default 10, 1–100), or `never`; a
   *  clean exit under the last two isn't a crash. What it leaves stopped isn't replaced until a
   *  deploy or `…/restart`. 400 on another type. */
  restart?: "always" | "on_failure" | "never";
  restart_tries?: number;
  /** A web service keeps each visitor on one instance while it's up with room (Cloud Run's
   *  session affinity): a `roll_instance` cookie (a digest of the instance, never shown to the
   *  app), moved when another instance answers. 400 on another type. */
  affinity?: boolean;
  /** Its port speaks TCP, not HTTP (Railway's TCP proxy): each environment answers on a port
   *  of its own from the node's ROLL_APPS_TCP_PORTS (Environment.tcp), woken and spliced per
   *  connection, its allow list and maintenance applied; its HTTP names are 404s. A web
   *  service; no health_path, h2c, affinity or gradual rollout (its health is its port). */
  tcp?: boolean;
  /** A deployment that enters a crash loop within ten minutes of going live is replaced by the
   *  one before it (a rollback: its image, no build, actor "crash loop"); a rollback's own
   *  deployment never is, so there are no two in a row. */
  rollback_on_crash?: boolean;
};

type Service = {
  id: string;
  project: string;          // workspace project id
  name: string;             // 1–32 of [a-z0-9-], unique in the project
  source: Source;
  build: Build;
  run: Run;
  registry: { server: string; username: string } | null; // a private registry's login (never the password)
  links: { kind: LinkKind; ref: string }[]; // what it links (no variables): the canvas draws them
  created_at: number;
  environments: EnvironmentSummary[];
};

type EnvironmentSummary = {
  name: string;
  state: InstanceState;
  url: string | null;
  active: { id: string; revision: string | null; created_at: number } | null;
  latest: { id: string; status: DeployStatus } | null;
};

type InstanceState = "none" | "stopped" | "suspended" | "waking" | "running" | "crashed" | "deploying";

type Patch<T> = { [K in keyof T]?: T[K] | null }; // null: back to the default
type NewService = { project: string; name: string; source: Source; build?: Patch<Build>; run?: Patch<Run>; variables?: { [name: string]: string } /* production, before its first deploy */; registry?: { username: string; password: string; server?: string } /* an image's login, before its first pull (as PUT …/registry) */; links?: { kind: LinkKind; ref: string; prefix?: string }[] /* at most 20, made before the first deploy (as PUT …/links); one that doesn't resolve makes no service */; x?: number; y?: number };
type ServicePatch = { name?: string; source?: Source; build?: Patch<Build>; run?: Patch<Run> };

type EnvironmentPut = {
  branch?: string | null;
  ephemeral?: boolean;
  /** Private (previews): visitors need `?roll_access=<token>` once (a host-only HttpOnly cookie
   *  `roll_access_<env>`, then a 302 to the URL without it), else a 401, before anything wakes.
   *  The token is 20–200 of [A-Za-z0-9_-]; only its SHA-256 is stored, and the app never sees
   *  the cookie. null: public again. */
  /** `bypass_sha256`: the SHA-256 (hex) of an automation's secret (CI's end-to-end tests): a
   *  request with `X-Roll-Preview-Bypass: <secret>` goes in with no cookie or redirect. */
  access?: { token: string; bypass_sha256?: string } | null;
  /** Creating it: start with this environment's variables, except its sealed ones (ignored when it exists). */
  variables_from?: string;
  /** A push deploys only once its commit's checks pass (statuses other than roll apps' own):
   *  pending waits (up to an hour; `Environment.waiting_for_ci` names it), a failure skips it. */
  wait_for_ci?: boolean;
  /** An image service whose image has a tag: deploy when the tag points at a new digest
   *  (checked every 5 minutes on the owner; actor `tag watch`). 400 otherwise. */
  /** Redeploy (the same image, the new values) when the project's secrets change, checked each minute
   *  on the owner against roll secrets' digest; off by default. */
  redeploy_on_secrets?: boolean;
  follow_tag?: boolean;
  /** A preview environment (ephemeral) only, set by roll previews: a visitor who came in through
   *  its access link gets roll previews' feedback button on its pages (`/__roll/feedback.js`
   *  before `</body>`, served from the preview's host), sending notes to the preview in roll's
   *  dashboard. 400 on another environment. */
  feedback?: boolean;
  /** Maintenance: `true` or `{message?}` (text, at most 500 characters) and its public names
   *  answer roll's 503 page (Retry-After 30) with the message, waking nothing; a deployment's own
   *  URL still answers. `false` or null serves again. */
  maintenance?: boolean | { message?: string | null } | null;
  /** Its own HTML instead of roll's pages, each at most 64 KiB (400 past it or empty); null goes back
   *  to roll's, a field left out stays. */
  pages?: { error?: string | null; maintenance?: string | null };
  /** Only these networks get in through its public names (deployment URLs too): at most 100,
   *  each `"a.b.c.d/n"`, an IPv6 `/n`, an address, or `{cidr, description?}`; kept canonical.
   *  Checked first, on the address the fleet vouches for (no address: 403). null or [] lets
   *  anyone in. The private network is unaffected. */
  allow_ips?: (string | { cidr: string; description?: string | null })[] | null;
  /** Each client's requests a minute through its public names (deployment URLs too), a token
   *  bucket per address the fleet vouches for: `burst` at once (default per_minute), then the
   *  rate; more is a 429 with Retry-After, before anything wakes. 1–100000 each; null: none. */
  rate_limit?: { per_minute: number; burst?: number } | null;
  /** Firewall rules (Vercel's), at most 50, in order: the first whose given fields all match
   *  decides (`allow` lets it in, `deny` is a 403 naming nothing); none matching lets it in.
   *  `path` exact or a prefix ending in `*`, matched on the path as an app reads it (decoded,
   *  `//` and dot segments resolved); `methods` any of; `user_agent` contained, case aside.
   *  Checked after allow_ips, before the rate limit and any wake; HTTP only. null: none. */
  firewall?: { action: "allow" | "deny"; path?: string; methods?: string[]; user_agent?: string }[] | null;
  /** Its own size and scale over the service's (min_instances, max_instances, cpu, memory_mb,
   *  concurrency, suspend_after_s, sleep_after_s; any other field is a 400). Each replaces the
   *  service's, null goes back to it; checked with the service's settings as one run. */
  run?: { [field: string]: number | null };
};

type Environment = EnvironmentSummary & {
  service: string;
  branch: string | null;     // deploys on every landing on this branch (repo sources)
  ephemeral: boolean;        // a preview environment
  instances: Instance[];
  domains: Domain[];
  port: number | null;       // locally, the service's own port (ROLL_APPS_PORTS)
  needs_deploy: boolean;     // variables or settings changed since the active deployment
  private: boolean;          // its hostnames answer only visitors with its access token
  next_run_at: number | null; // a cron service's next scheduled run, once deployed
  crash_loop: { crashes: number; last_at: number; last_error: string } | null; // 3+ crashes in 10 minutes
  canary: { deployment: string; share: number } | null; // a gradual rollout under way: the percent of requests the new deployment takes
  run: { [field: string]: number }; // its own run settings over the service's ({}: the service's)
  hook: boolean;              // it has a deploy hook
  follow_tag: boolean;        // an image service's tag is watched: a new digest behind it deploys
  redeploy_on_secrets: boolean; // the project's secrets changing redeploys it (same image, new values)
  feedback: boolean;          // a preview's pages carry roll previews' feedback button
  maintenance: { message: string | null } | null; // in maintenance (null message: roll's words); null: serving
  pages?: { error?: string; maintenance?: string }; // its own pages (absent: roll's): error for the 502/503/504 while it can't answer or wake, maintenance while in maintenance
  allow_ips: { cidr: string; description?: string }[]; // only these networks get in through its public names ([]: anyone)
  tcp: string | null; // a TCP service's address, host:port (null until it has its port)
  rate_limit: { per_minute: number; burst: number | null } | null; // each client's requests a minute (null: no limit)
  firewall: { action: "allow" | "deny"; path?: string; methods?: string[]; user_agent?: string }[]; // its rules, in order
  scaling: { backlog: number | null; wanted: number } | null; // a worker following its work: the backlog last read (every 10 s) and the instances it wants
  wait_for_ci: boolean;
  waiting_for_ci: string | null; // the push (sha) waiting for its checks
  created_at: number;
};

type Instance = { id: string; deployment: string; state: InstanceState; started_at: number | null; in_flight: number };

type DeployStatus = "queued" | "building" | "deploying" | "active" | "inactive" | "failed" | "canceled" | "superseded" | "removed" | "ready"; // ready: a no_traffic deploy, built and health-checked, never live

/** no_traffic: build, pull and health-check it, then stop at `ready` without switching (a web
 *  service without a volume; 400 otherwise). It answers on its own URL; a rollback to it puts it
 *  live. Every built deployment of a web service without a volume has its own URL:
 *  `<id without dep_>--<environment's host>` (`abc123def456--web-prod.up.roll.site`), behind the
 *  environment's access token when it's private; one instance at most, gone a minute after its
 *  last request, metered with the environment. */
type DeployCommits = {
  since: { id: string; revision: string } | null; // the deployment it's measured against; null: nothing to compare
  commits: { sha: string; summary: string; author: string }[] | null;
  complete: boolean;         // `since` was reached (false: more than 50, or not in its history: a rollback, another branch)
};

type NewDeploy = { revision?: string; image?: string; message?: string; no_traffic?: boolean; no_cache?: boolean /* build from scratch (400 with an image): no cached layer, base images pulled again, the service's cache mounts empty from then on; an upload says it with `X-Roll-No-Cache: 1` */ };

type Deployment = {
  id: string;
  service: string;
  environment: string;
  status: DeployStatus;
  trigger: "push" | "manual" | "rollback" | "upload" | "api" | "settings" | "promote"; // settings: a redeploy
  actor: string | null;     // who asked: "git:<name>" for a push (its commit's author, as the commit claims it), the calling token or connection ("operator" for the operator's token); null when roll did it on its own
  revision: string | null;   // git commit of a repo source
  message: string;
  image: string | null;      // the built or pinned image, by digest once known
  plan: Plan | null;         // what the build detected
  rollback_of: string | null;
  pruned_at?: number;        // pruned (older than the environment keeps): history only, its image gone; a rollback to it is a 409
  steps?: { step: "build" | "pre_deploy" | "rollout" | "live" | "failed"; at: number }[]; // when each began: on GET …/deploys/{dep} only
  error: string | null;
  created_at: number;
  started_at: number | null;
  finished_at: number | null;
  /** Milliseconds from the deploy's start to its first healthy answer. */
  ready_ms: number | null;
};

type PlanRequest = { repo: string; revision?: string; root?: string } | { files: Record<string, string> };

type Plan = {
  provider: "dockerfile" | "node" | "python" | "go" | "rust" | "ruby" | "php" | "java" | "static";
  framework: string | null;  // next, django, rails, spring…
  language_version: string | null;
  install: string | null;
  build: string | null;
  start: string | null;
  port: number;
  dockerfile: string;        // what builds the image (the tree's own, or generated)
  notes: string[];           // why: "found pnpm-lock.yaml", "next build → .next"
  spa?: boolean;             // a static site with a client-side router
  /** Datastores the root's manifests have client libraries for ("pg in package.json"): New
   *  service offers to make and link them. Absent when none. */
  uses?: ("postgres" | "redis")[];
  /** The Procfile's `release:` (Heroku's release phase): the deployment's pre-deploy command when
   *  the service sets none. Absent without one, and for a tree's own Dockerfile. A Procfile's `web:`
   *  (`worker:` for a worker) is `start` when the service names none. */
  release?: string;
};

/** Wired on a node today: postgres (DATABASE_URL) and redis (REDIS_URL, REDIS_REST_URL/TOKEN). */
type Link = { kind: "postgres" | "redis" | "bucket" | "stream" | "queue" | "nosql" | "service"; ref: string; prefix: string; vars: string[] };
// `service`: `ref` is another web service of the project (its id); `URL` is
// `http://<name>.<env>.internal:<port>` in the linking environment, `PUBLIC_URL` its public URL
// when it has one there. The prefix defaults to its name (`API_`). A service's links go with it.

type Variables = { vars: { name: string; masked: string; source: "service" | "secret" | "link" | "roll" | "reference"; reference?: string /* a reference variable's template, `${{ api.KEY }}` */; sealed?: true /* never shown again: masked "••••••••" whatever its length */ }[] };
// A value set with `${{ VAR }}` / `${{ service.VAR }}` is a reference, filled in at each deploy;
// setting one that names another service needs `reveal` on it (403), and one naming no service of
// the project is a 400.

type LogLine = { t: number; stream: "stdout" | "stderr" | "build" | "system"; instance: string | null; deployment: string | null; text: string };

type Metrics = {
  minutes: number[];         // unix minute of each point, oldest first
  requests: number[]; errors: number[]; // 5xx
  p50_ms: number[]; p95_ms: number[]; p99_ms: number[];
  cpu: number[];             // cores used, average over the minute
  memory_bytes: number[];    // resident, max over the minute
  instances: number[];
  cold_starts: number[];
  cold_start_ms: number[];   // average of the minute's cold starts (0 when none)
};

type Usage = {
  days: { day: number; cpu_seconds: number; gb_seconds: number; requests: number; egress_bytes: number; build_seconds: number; usd: number }[];
  total_usd: number;
};

/** In the event log, each written in the same transaction as the change it records. Sequence
 *  numbers commit in order with no gaps, so a reader moving past `next` never misses one. Kept 7 days. */
type Event = { seq: number; at: number; kind: "deploy.active" | "deploy.failed" | "crash_loop" | "spend.alert" | "spend.limit" | "alert.memory" | "alert.errors" | "alert.latency" | "alert.log" | "cron.failed" | "job.failed" | "domain.active" | "domain.expiring" | "domain.expired" | "domain.tls_failed" | `audit.${string}` /* changes people made: what /activity lists */; service: string; environment: string; deployment: string | null; data: object };
/** A webhook's body: { type, at, seq, service, environment, deployment, data, title, text, url },
 *  signed as roll code's webhooks are: X-Roll-Signature-256: sha256=hex(HMAC-SHA256(secret,
 *  X-Roll-Timestamp + "." + body)), the timestamp the send time in Unix seconds (a retry is signed
 *  anew), with X-Roll-Event and X-Roll-Delivery (the delivery's id; a retry keeps it).
 *  Slack gets { text }, Discord { content } (mentions off). */
/** A project's monthly limits, US dollars at roll's prices over the calendar month (UTC). Past
 *  `alert_usd`: one `spend.alert` event (service ""; data { project, project_name, usd, limit_usd }).
 *  Past `hard_usd`: one `spend.limit` event and the project is capped (`capped`): its instances
 *  stop, visitors get a 402 page, deploys, jobs and cron runs are refused, until the month ends or
 *  the limit is raised. */
type SpendLimits = { alert_usd: number | null; hard_usd: number | null; month_usd: number; capped: boolean };
/** A project's log drain: its services' runtime output (stdout, stderr, roll's lines), POSTed as
 *  `application/x-ndjson` batches (at most 500 lines or 1 s) of
 *  { t, service, service_name, environment, instance, deployment, stream, text }, signed as
 *  webhooks are (X-Roll-Signature-256, X-Roll-Timestamp, X-Roll-Delivery the batch's id). Best
 *  effort: a batch is tried three times, then dropped; a drain that can't keep up loses its
 *  oldest lines. At most 5 a project. */
type LogDrain = { id: string; project: string; host: string; created_at: number; secret?: string };
/** A request through the proxy (`stream: "http"` lines' text, JSON). The path has no query string. */
type AccessLine = {
  method: string; path: string; status: number; ms: number; bytes: number;
  ip: string | null; ua: string; host: string;      // who asked: the address the fleet vouches for, the user agent, the host
  // An instance's answer:
  instance?: string; request_id?: string; cold?: boolean; waited_ms?: number;
  // Or one roll gave itself, before any instance, and why (metered: a failed wake counts as an error):
  by?: "allow_list" | "firewall" | "rate_limit" | "private" | "maintenance" | "wake" | "capped" | "not_deployed" | "moving" | "unavailable";
};
/** A volume's backup: a tar in object storage (`apps/volumes/<service>/<env>/<id>.tar`). */
type Backup = { id: string; at: number; size: number };
type NotifyTarget = { id: string; project: string; kind: "webhook" | "slack" | "discord"; host: string; events: ("deploy.failed" | "crash_loop" | "deploy.active" | "spend.alert" | "spend.limit" | "alert.memory" | "alert.errors" | "alert.latency" | "alert.log" | "cron.failed" | "job.failed" | "domain.active" | "domain.expiring" | "domain.expired" | "domain.tls_failed")[];
  environments: string[];  // only these environments' events; empty: every one's
  services: string[];      // only these services' (ids); empty: every one's
  previews: boolean;       // preview (ephemeral) environments' events too (an environment named in `environments` always)
  created_at: number; secret?: string };
type Delivery = { id: string; event: number; kind: string; status: "pending" | "delivered" | "failed"; attempts: number; code: number | null; error: string | null; at: number };
type RunStatus = "running" | "succeeded" | "failed" | "skipped" | "timed_out" | "canceled";
type CronRun = { id: string; deployment: string; status: RunStatus; exit_code: number | null; started_at: number; finished_at: number | null };
/** A one-off command in a fresh container of the active deployment: its variables and project
 *  network, no port, no volume. timeout_s 1–86400 (default 3600). */
type Job = { id: string; deployment: string; command: string; status: RunStatus; exit_code: number | null; started_at: number; finished_at: number | null };

type Domain = {
  hostname: string;
  kind: "roll" | "custom";
  /** What the DNS needs: a CNAME to `target` (a subdomain), or the TXT challenge (an apex). */
  record: "cname" | "txt" | "both" | "none"; // both: a shared CNAME target routes, the TXT proves ownership
  status: "pending" | "issuing" | "active" | "failed";
  /** What the user puts in DNS: a CNAME to `target`, or (apex) a TXT `_roll-challenge.<host>` with `token`.
   * A wildcard (`*.example.com`: every name one label under it, after exact domains) has its
   * CNAME on `*.example.com` and its TXT on `_roll-challenge.example.com`. It's refused (400)
   * where custom domains' certificates can't be wildcards (no DNS-01). */
  target: string;
  token: string;
  error: string | null;
  /** The certificate (`ROLL_APPS_TLS`): `none` where nothing issues them (a node without the
   *  edge); for a custom domain `pending` until it's `issued` (Cloudflare for SaaS, or the
   *  edge's own over ACME HTTP-01 once DNS is verified), or `failed`. A roll domain is `issued`
   *  behind the edge. */
  tls: "none" | "pending" | "issued" | "failed";
  /** Why it failed, or what it's waiting for (the provider's words, at most ~300 characters). */
  tls_error: string | null;
  redirect_to?: string | null; // its requests go 308 there (www → apex), path and query kept
  created_at: number;
};
```

A custom domain that is still unverified 7 days after it was added is removed, so the hostname
is free for whoever really owns it. Removing a domain, its environment or its service removes it
from the certificate's provider too.

## Public traffic

Requests to a service's hostname (`<slug>.<domain>`, a custom domain, or locally the service's
own port) are the service's, not the API's: they reach the waking proxy, never the gate. The
proxy adds `X-Forwarded-For`, `X-Forwarded-Proto`, `X-Forwarded-Host` and `X-Request-Id`, and
strips `X-Roll-*` request headers. A service that can't wake in `wake_timeout` answers 503 with
`Retry-After: 5` and a small page; a service with no deployment answers 404 with a page that
says so.
