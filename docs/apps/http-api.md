# roll apps HTTP API

The contract between `crates/apps-control` (`/v1/apps`) and its clients:
`apps/dashboard/src/lib/apps-api.ts`, `roll` (`crates/roll-cli/src/apps.rs`) and the MCP tools on
feature/roll-mcp. Change them together.

- Bodies are JSON unless a route says otherwise. Every error is `{"error": string}` with the
  status below. Times are unix milliseconds. Ids: `svc_…`, `dep_…`, `run_…` (12 lowercase base32
  characters after the prefix).
- The routes sit behind rolld's gate (the operator token today; scoped tokens from
  feature/roll-mcp). roll apps checks nothing itself.
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
| `GET /v1/apps/services/{id}/links` | | `Link[]` | read |
| `PUT /v1/apps/services/{id}/links/{kind}/{ref}` | `{prefix?}` | `Link` | write |
| `DELETE /v1/apps/services/{id}/links/{kind}/{ref}` | | 204 | write |
| `GET /v1/apps/projects/{project}/notifications` | | `NotifyTarget[]` (the URL never returned, only its host) | read |
| `POST /v1/apps/projects/{project}/notifications` | `{ kind?: "webhook" \| "slack" \| "discord", url, events? }` | 201 `NotifyTarget` (+ `secret` once, for a webhook) | reveal for a webhook (its secret comes back), write for Slack and Discord |
| `DELETE /v1/apps/projects/{project}/notifications/{nid}` | | 204 | write |
| `POST /v1/apps/projects/{project}/notifications/{nid}/test` | | `{ ok, code, error }` (sent now, not retried) | write |
| `GET /v1/apps/projects/{project}/notifications/{nid}/deliveries` | | `Delivery[]`, newest first (50) | read |
| `GET /v1/apps/projects/{project}/drains` | | `LogDrain[]` (the URL never returned, only its host) | read |
| `POST /v1/apps/projects/{project}/drains` | `{ url }` (https, a public host) | 201 `LogDrain` + `secret` once | reveal (its secret comes back) |
| `DELETE /v1/apps/projects/{project}/drains/{did}` | | 204 | write |
| `GET /v1/apps/projects/{project}/limits` | | `SpendLimits` | read |
| `PUT /v1/apps/projects/{project}/limits` | `{ alert_usd: number \| null, hard_usd: number \| null }` | `SpendLimits` (this month's alert and cap are decided again within 30 s) | admin (the hard limit pauses the whole project) |
| `GET /v1/apps/events?after=&limit=` | | `{ events: Event[], next }`: what happened to services, in order, after a cursor | read |
| `PUT /v1/apps/services/{id}/registry` | `{ username, password, server? }` | `Service` (a private registry's login for an image source; sealed, never returned) | write |
| `DELETE /v1/apps/services/{id}/registry` | | `Service` | write |
| `GET /v1/apps/services/{id}/environments` | | `Environment[]` | read |
| `PUT /v1/apps/services/{id}/environments/{env}` | `EnvironmentPut` | `Environment` | deploy*; with `variables_from` also reveal in that environment (it copies its values) |
| `GET /v1/apps/services/{id}/environments/{env}` | | `Environment` | read |
| `DELETE /v1/apps/services/{id}/environments/{env}` | | 204 | delete |
| `POST /v1/apps/services/{id}/environments/{env}/deploys` | `NewDeploy`, or a gzipped tar (`application/gzip`) | 202 `Deployment` | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/deploys` | | `Deployment[]`, newest first | read |
| `GET /v1/apps/services/{id}/environments/{env}/deploys/{dep}` | | `Deployment` | read |
| `GET /v1/apps/services/{id}/environments/{env}/deploys/{dep}/logs?follow=&tail=` | | `LogLine[]` or SSE | read |
| `POST /v1/apps/services/{id}/environments/{env}/deploys/{dep}/rollback` | | 202 `Deployment` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/redeploy` | `{ message? }` | 202 `Deployment` (trigger `settings`) | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/promote` | `{ from, message? }` | 202 `Deployment` (trigger `promote`: `from`'s live image, no build, with `env`'s variables; `rollback_of` names the promoted deployment) | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/deploys/{dep}/cancel` | | `Deployment` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/restart` | | 202 `Environment` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/sleep` | | `Environment` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/wake` | | `Environment` | read |
| `GET /v1/apps/services/{id}/environments/{env}/variables` | | `Variables` (values masked) | read |
| `GET /v1/apps/services/{id}/environments/{env}/variables/values` | | `{[name]: value}` | reveal |
| `PATCH /v1/apps/services/{id}/environments/{env}/variables` | `{set?: {[name]: value}, unset?: string[]}` | `Variables` | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/logs?follow=&tail=&since=&q=&http=&status=&min_ms=` | | `LogLine[]` or SSE; `http=1`: the proxy's access log (`stream: "http"`, text `AccessLine`), `status` (`500`, `5xx`, `400-499`) and `min_ms` filtering it | read |
| `GET /v1/apps/services/{id}/environments/{env}/metrics?minutes=` | | `Metrics` | read |
| `GET /v1/apps/services/{id}/environments/{env}/usage?days=` | | `Usage` | read |
| `GET /v1/apps/services/{id}/environments/{env}/runs` | | `CronRun[]` | read |
| `POST /v1/apps/services/{id}/environments/{env}/runs` | | 202 `CronRun` (run now) | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/shell?instance=&cols=&rows=` | WebSocket | a terminal: binary both ways, text `{"resize":[cols,rows]}` | admin (a shell runs anything with the environment's variables, as agents' shells) |
| `POST /v1/apps/services/{id}/environments/{env}/shell/sessions` | `{ instance?, cols?, rows? }` | 201 `{ id }` (a shell over HTTP: the dashboard's terminal) | admin (a shell runs anything with the environment's variables, as agents' shells) |
| `GET /v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}/screen` | | SSE: base64 chunks (the scrollback first), then `event: closed` | admin (a shell runs anything with the environment's variables, as agents' shells) |
| `POST /v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}/keys` | `{ data }` (base64, at most 64 KiB) | 204 | admin (a shell runs anything with the environment's variables, as agents' shells) |
| `POST /v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}/resize` | `{ cols, rows }` | 204 | deploy* |
| `DELETE /v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}` | | 204 | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/jobs` | | `Job[]`, newest first | read |
| `POST /v1/apps/services/{id}/environments/{env}/jobs` | `{ command, timeout_s? }` | 202 `Job` | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/jobs/{job}` | | `Job` | read |
| `GET /v1/apps/services/{id}/environments/{env}/jobs/{job}/logs?follow=&tail=` | | `LogLine[]` or SSE (ends with the job) | read |
| `POST /v1/apps/services/{id}/environments/{env}/jobs/{job}/cancel` | | `Job` | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/domains` | | `Domain[]` | read |
| `POST /v1/apps/services/{id}/environments/{env}/domains` | `{hostname}` | 201 `Domain` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/domains/{host}/verify` | | `Domain` | read |
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
};

type Run = {
  type: "web" | "worker" | "cron";
  port?: number;            // web: the port the app listens on (default from the plan, else 8080; PORT is set to it)
  start?: string;           // overrides the image's command
  pre_deploy?: string;      // once per deploy, in a container of the new image with its variables and
                            // the project network (no volume), before the switch: a migration. A
                            // non-zero exit fails the deploy; the old one keeps serving
  schedule?: string;        // cron: five fields, UTC
  timeout_s?: number;       // cron: a run's limit (default 3600)
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
};

type Service = {
  id: string;
  project: string;          // workspace project id
  name: string;             // 1–32 of [a-z0-9-], unique in the project
  source: Source;
  build: Build;
  run: Run;
  registry: { server: string; username: string } | null; // a private registry's login (never the password)
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
type NewService = { project: string; name: string; source: Source; build?: Patch<Build>; run?: Patch<Run>; variables?: { [name: string]: string } /* production, before its first deploy */; registry?: { username: string; password: string; server?: string } /* an image's login, before its first pull (as PUT …/registry) */; x?: number; y?: number };
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
  /** Creating it: start with this environment's variables (ignored when it exists). */
  variables_from?: string;
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
  created_at: number;
};

type Instance = { id: string; deployment: string; state: InstanceState; started_at: number | null; in_flight: number };

type DeployStatus = "queued" | "building" | "deploying" | "active" | "inactive" | "failed" | "canceled" | "superseded" | "removed";

type NewDeploy = { revision?: string; image?: string; message?: string };

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
};

/** Wired on a node today: postgres (DATABASE_URL) and redis (REDIS_URL, REDIS_REST_URL/TOKEN). */
type Link = { kind: "postgres" | "redis" | "bucket" | "stream" | "queue" | "nosql"; ref: string; prefix: string; vars: string[] };

type Variables = { vars: { name: string; masked: string; source: "service" | "secret" | "link" | "roll" }[] };

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

"/** In the event log, each written in the same transaction as the change it records. Sequence
 *  numbers commit in order with no gaps, so a reader moving past `next` never misses one. Kept 7 days. */
type Event = { seq: number; at: number; kind: "deploy.active" | "deploy.failed" | "crash_loop" | "spend.alert" | "spend.limit"; service: string; environment: string; deployment: string | null; data: object };
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
type AccessLine = { method: string; path: string; status: number; ms: number; bytes: number; instance: string; request_id: string; cold: boolean; waited_ms: number };
type NotifyTarget = { id: string; project: string; kind: "webhook" | "slack" | "discord"; host: string; events: ("deploy.failed" | "crash_loop" | "deploy.active" | "spend.alert" | "spend.limit")[]; created_at: number; secret?: string };
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
  /** What the user puts in DNS: a CNAME to `target`, or (apex) a TXT `_roll-challenge.<host>` with `token`. */
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
