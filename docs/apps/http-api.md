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

`docs/apps/openapi.json` is this table as OpenAPI 3.1: one operation per row, with its path
parameters, the words below, and the permission it needs as `x-roll-action`. The shapes stay
here. `api.rs::the_openapi_spec_is_the_contract_and_the_router` fails when the router serves a
route this table doesn't list (or the reverse), and when the spec is stale:
`ROLL_OPENAPI=write cargo test -p apps-control --test api openapi` writes it again.

`action` is what the permissions catalogue (feature/roll-mcp, product `apps`) classifies the
route as. "deploy*" means `deploy` when `{env}` is `production`, else `write`.

| Route | Body | Answer | action |
| --- | --- | --- | --- |
| `GET /v1/apps` | | `Overview` | read |
| `POST /v1/apps/plan` | `PlanRequest` | `Plan`; with `roots: true` (a repo or a Git URL, no `root`): `{ roots: { root, provider, framework? }[] }`, the folders up to three levels down that build on their own (a start command, a framework or a Dockerfile: no library package, docs or dependencies), at most 20 | read |
| `POST /v1/apps/projects/{project}/export` | `{ databases? }` (`{"postgres:<ref>": "db"}`: the names of the databases its services link, as the caller lists them; one not named is named after its ref) | `{ roll_toml, notes }`: its services (source, branch, the run settings that differ from a new service's), the postgres and redis databases they link, variables by name only in its header; read back with the parser `roll up --all` uses before it's answered; 409 for a project with no service | read |
| `GET /v1/apps/projects/{project}/status-page` | | `{ title, url, items }`: the project's public status page; 404 without one | read |
| `PUT /v1/apps/projects/{project}/status-page` | `{ title?: string /* 1–80, default "Status" */, items?: { service, environment?, check? \| heartbeat? }[] /* at most 50; none: every check and heartbeat of its environments */ }` | `{ title, url, items }`: made (a new public name, `status-<slug>.<domain>`) or changed (its name kept) | admin (it makes the project's service names public: a person's call, not a token's write) |
| `DELETE /v1/apps/projects/{project}/status-page` | | 204: taken down | write |
| `GET /v1/apps/projects/{project}/status-page/incidents` | | `{ incidents: { id, title, state, started_at, resolved_at, automatic, updates: { at, state, text }[] }[] }`: the last 90 days' and the open ones, newest first (at most 200) | read |
| `POST /v1/apps/projects/{project}/status-page/incidents` | `{ title: string /* 1–120 */, state?: "investigating" \| "identified" \| "monitoring" \| "resolved", text: string /* 1–2000 */ }` | 201 `{ id }`: a person's incident on the page; 404 without a page | write |
| `POST /v1/apps/projects/{project}/status-page/incidents/{iid}/updates` | `{ state, text }` | 204: an update (at most 100 an incident); `resolved` closes it | write |
| `GET /v1/apps/projects/{project}/preview-domain` | | `Domain` (404: none): the project's own preview domain, a wildcard its preview environments answer under by their `preview_host`; `tls` is `none` (plain HTTP until wildcard certificates, DNS-01, are issued here) | read |
| `POST /v1/apps/projects/{project}/preview-domain` | `{ hostname: "*.preview.acme.dev" }` (a wildcard; one a project, a new one replaces it; 409 when another project's previews or any service's domain has it) | 201 `Domain` (pending: the same CNAME, `target` its own, or `_roll-challenge` TXT a custom domain takes) | write |
| `POST /v1/apps/projects/{project}/preview-domain/verify` | | `Domain` (active once its DNS checks out, else pending with what DNS answered) | write |
| `DELETE /v1/apps/projects/{project}/preview-domain` | | 204 (its names answer nothing; the environments keep their labels for another) | delete |
| `POST /v1/apps/import` | `{ roll_toml }`, `{ compose }`, another platform's file (`{ render }` a Blueprint's render.yaml, `{ fly }` fly.toml, `{ railway }` railway.json or railway.toml, `{ heroku, procfile? }` app.json and its Procfile; read as roll.toml with `notes` on what isn't carried over) (each at most 1 MiB; one at a time), `{ repo, revision? }` (that roll code repo's root roll.toml, at its default branch's head unless a revision is named: a "deploy to roll" link), or `{ git, branch? }` (a public Git repository's root roll.toml by URL, at most 64 KiB, at its branch's head; https, no credentials, a public address). From a repo or a Git URL, an app that names only a `root` becomes that repository's source at that folder (a Git URL's following the branch it was read from), not an upload | `ProjectImport` (read, nothing made: checked as `roll up --all` checks it; 400 says what's wrong) | read |
| `GET /v1/apps/services?project=&deleted=` | | `Service[]`; `deleted=true`: `{id, project, name, deleted_at, purge_at}[]` (unix ms, newest first), the deleted services kept for a restore until `purge_at`, 48 hours after the delete | read |
| `POST /v1/apps/services` | `NewService` | 201 `Service` | deploy* (a repo or image source deploys production at once; write for an upload source) |
| `GET /v1/apps/projects/{project}/services` | | `Service[]` | read |
| `POST /v1/apps/projects/{project}/services` | `NewService` (`project` may be left out) | 201 `Service` | deploy* (as above) |
| `GET /v1/apps/services/{id}` | | `Service` | read |
| `PATCH /v1/apps/services/{id}` | `ServicePatch` | `Service` | write |
| `DELETE /v1/apps/services/{id}` | | 204: the service stops (pending deploys cancelled, instances removed) and is hidden, kept 48 hours for a restore with its environments, volumes, images, backups, domains, links and name; every other route of it is a 404. A deleted service's `DELETE` deletes it for good now (frees its name); the prune sweep does after 48 hours | delete |
| `POST /v1/apps/services/{id}/restore` | | `Service`: a deleted service back as it was, its live deployments loaded again and its card placed on the canvas; 409 when it isn't deleted | deploy |
| `GET /v1/apps/services/{id}/activity?all=` | | `Event[]` (`audit.*`, newest first, 200): who changed what, never a value; `all=1`: every kind (deploys, failures, crash loops, alerts, domains, spend), the service's timeline | read |
| `GET /v1/apps/services/{id}/links` | | `Link[]` | read |
| `PUT /v1/apps/services/{id}/links/{kind}/{ref}` | `{prefix?, indices?, access?}` (`indices` and `access`: a search link's only, 400 on another kind: its key only for those index names or patterns (1–50, `tenant-*`), and `read` or `append`, create documents on exact names only (400 with a pattern); none: read and write documents. Roll search's keys route checks them again) | `Link` (a kind that makes a credential per link makes it here, once; setting the link again keeps it, unless its `indices` or `access` change: then a new key made with them replaces it and the old one is revoked) | write |
| `DELETE /v1/apps/services/{id}/links/{kind}/{ref}` | | 204 (revokes a credential the link made) | write |
| `GET /v1/apps/projects/{project}/notifications` | | `NotifyTarget[]` (the URL never returned, only its host) | read |
| `POST /v1/apps/projects/{project}/notifications` | `{ kind?: "webhook" \| "slack" \| "discord" \| "email" \| "pagerduty" \| "opsgenie", url, routing_key?, api_key?, region?, events?, environments?, services?, previews? }` (environments: names; services: names or ids, kept as ids; previews: default false; `email` takes no `url`: it's the project's owner, 400 with one, for a project without an owner, or where mail isn't set up; `pagerduty` takes its integration's `routing_key`, `opsgenie` its `api_key`, each sealed and never returned, and `region` `us` or `eu`, no `url`: the service's own API) | 201 `NotifyTarget` (+ `secret` once, for a webhook) | reveal for a webhook (its secret comes back), write for Slack, Discord, email, PagerDuty and Opsgenie |
| `PATCH /v1/apps/projects/{project}/notifications/{nid}` | `{ events?, environments?, services?, previews? }` (given ones replace, the rest stay; null is a new target's default: the default events, every environment and service, no previews; any other field 400: a new URL is a new target) | `NotifyTarget` (its URL and secret unchanged) | reveal (it can send a webhook more) |
| `DELETE /v1/apps/projects/{project}/notifications/{nid}` | | 204 | write |
| `POST /v1/apps/projects/{project}/notifications/{nid}/test` | | `{ ok, code, error }` (sent now, not retried) | write |
| `GET /v1/apps/projects/{project}/notifications/{nid}/deliveries` | | `Delivery[]`, newest first (50) | read |
| `GET /v1/apps/projects/{project}/drains` | | `LogDrain[]` (the URL never returned, only its host) | read |
| `POST /v1/apps/projects/{project}/drains` | `{ url, format?, sources?, environments?, services?, previews?, headers? }` or `{ bucket, prefix?, sources?, … }` or `{ search, namespace?, retention_days?, sources?, … }` (`search`: a roll search cluster on the project's canvas, without url, headers, format or bucket: its own data stream `logs-roll.apps-<namespace>` (1–40 of a–z, 0–9 and `_`; random by default) made with roll apps' template and its retention (`retention_days` 1–3650, 30 by default), lines as ECS documents through `_bulk` `create` actions with a key minted for the drain that only appends to that stream, revoked with the drain (the stream stays until its retention ends); the answer adds `stream`; 409 for an OpenSearch-flavor cluster; `bucket`: a roll storage bucket on the project's canvas, without url, headers or another format: an `s3` drain with a key minted for it, write on the bucket asked of the caller, revoked with the drain; https, a public host, or `syslog+tls://host:port` for `format: "syslog"`, which takes no headers; for `format: "s3"`, `https://<endpoint>/<bucket>[/<prefix>]` with headers `aws-access-key-id`, `aws-secret-access-key` and optionally `aws-region`, nothing else; `sources` some of `runtime` (default), `http`, `build`, `audit`, `traces`, `metrics`; services by name or id; `headers` `{name: value}`, at most 10, sealed, never roll's own or `content-type`) | 201 `LogDrain` + `secret` once | reveal (its secret comes back) |
| `PATCH /v1/apps/projects/{project}/drains/{did}` | `{ sources?, environments?, services?, previews?, headers? }` (given ones replace, `headers` all of them; a new URL or format is a new drain) | `LogDrain` | reveal (it can send more out) |
| `POST /v1/apps/projects/{project}/drains/{did}/test` | | `{ ok, code, error }` (one line sent now, signed and with its headers, not retried; `code` null for syslog) | write |
| `DELETE /v1/apps/projects/{project}/drains/{did}` | | 204 | write |
| `GET /v1/apps/projects/{project}/limits` | | `SpendLimits` | read |
| `GET /v1/apps/projects/{project}/usage?month=` | | `ProjectUsage`: a month's use (`YYYY-MM`, this one by default; 400 for a month to come) by service and environment at roll's prices, each service's previews as one row (`previews: true`), deleted services as one (`deleted: true`), most costly first; this month's projected to its end at the pace so far | read |
| `GET /v1/apps/projects/{project}/signing` | | `{ keys: string[], required: boolean }` (none set: no keys, not required) | read |
| `PUT /v1/apps/projects/{project}/signing` | `{ keys, required? }` (1–10 P-256 public keys in PEM, cosign.pub; `required` true by default) | `{ keys, required }` (with `required`, an image service's deploy needs its pinned digest's cosign signature by one of the keys, or fails before switching; roll's builds pass) | admin (which images may run) |
| `DELETE /v1/apps/projects/{project}/signing` | | 204 | admin |
| `PUT /v1/apps/projects/{project}/limits` | `{ alert_usd: number \| null, hard_usd: number \| null }` | `SpendLimits` (this month's alert and cap are decided again within 30 s) | admin (the hard limit pauses the whole project) |
| `GET /v1/apps/events?after=&limit=&latest=` | | `{ events: Event[], next }`: what happened to services, in order, after a cursor; `latest=1` (never with `after`: 400) answers no events and the newest cursor, for a reader that starts from now | read |
| `GET /v1/apps/services/{id}/changes` | | server-sent events: `ready`, then a `changed` (data `{}`) whenever a deploy moves, an instance wakes, sleeps or crashes, or someone changes the service; the page reads it again (on the owner, in memory; it carries nothing itself) | read |
| `PUT /v1/apps/services/{id}/registry` | `{ username, password, server? }`, or for an image on ECR `{ kind: "ecr", access_key_id, secret_access_key, region? }` (region defaults to the registry's; another is a 400) | `Service` (a private registry's login for an image source; sealed, never returned; an ECR key is exchanged for ECR's 12-hour token at a pull, user `AWS`, kept until it has an hour left and then fetched again for what runs) | write |
| `DELETE /v1/apps/services/{id}/registry` | | `Service` | write |
| `PUT /v1/apps/services/{id}/git-login` | `{ username, password }` (a deploy token, app password or access token as `password`; one line each), or `{ username, secret }` (the name of the project's secret that holds it, read from its production secrets at each fetch: a rotated token just works, nothing is kept here; or a reference `roll://production/…/KEY`, bound to the caller as production's runtime read and read as them at each fetch: 403 unless they may reveal it for the service, 400 for another environment's) | `Service` (a private Git repository's login for a Git URL source, bound to the URL's host: sent only there as Basic auth, dropped when the source moves to another repository; sealed, never returned) | write |
| `DELETE /v1/apps/services/{id}/git-login` | | `Service` | write |
| `PUT /v1/apps/services/{id}/github` | | `Service` (a Git URL service on roll's GitHub App's host connected to the installation that sees its repository: fetches use the installation's short-lived tokens, deploys post commit statuses and GitHub Deployments; 409 without the App or when no installation sees the repository; dropped when the source moves to another repository) | operator only (installations span tenants) |
| `DELETE /v1/apps/services/{id}/github` | | `Service` | operator only |
| `GET /v1/apps/services/{id}/environments` | | `Environment[]` | read |
| `PUT /v1/apps/services/{id}/environments/{env}` | `EnvironmentPut` | `Environment` | deploy*; with `variables_from` also reveal in that environment (it copies its values) |
| `GET /v1/apps/services/{id}/environments/{env}` | | `Environment` | read |
| `DELETE /v1/apps/services/{id}/environments/{env}` | | 204 | delete |
| `POST /v1/apps/services/{id}/environments/{env}/deploys` | `NewDeploy`, a gzipped tar of a tree (`application/gzip`, `roll up`), or an image built elsewhere (`application/x-docker-image`: `docker save`'s archive, gzipped or not; at most the upload limit; its names are taken out before it's loaded, and it's tagged as a build's would be; on the fleet a CodeBuild run of its own loads and pushes it to the service's ECR repository) | 202 `Deployment` | deploy*; admin with `X-Roll-Override: 1` |
| `GET /v1/apps/services/{id}/environments/{env}/deploys?before=` | | `Deployment[]`, newest first, 100 at most; `before` (unix ms, the last one's `created_at`): the next page | read |
| `GET /v1/apps/services/{id}/environments/{env}/deploys/{dep}` | | `Deployment` | read |
| `GET /v1/apps/services/{id}/environments/{env}/deploys/{dep}/logs?follow=&tail=` | | `LogLine[]` or SSE (ends with the deploy: an `end` event, then the stream closes, so a follower stops instead of reading it again; a job's and a run's log end the same way) | read |
| `GET /v1/apps/services/{id}/environments/{env}/deploys/{dep}/sbom` | | its image's SBOM, CycloneDX JSON (`application/vnd.cyclonedx+json`, as a download) from the same scan; 404 when it wasn't scanned, the SBOM was over 8 MiB, or the deployment was pruned | read |
| `POST /v1/apps/services/{id}/environments/{env}/deploys/{dep}/explain?again=` | | `Explanation`: why a failed deployment failed, read from its error, plan, steps and the newest lines of its build log and output (masked as they're written) by the model of the owner of the service's roll code repo (their own provider, else the server's within their cap; metered on their roll code usage as `explain`). Kept with the deployment and answered again without a model call, unless `again=1` (at most once a minute; one question a deployment at a time). 409 when it didn't fail, it was asked anew less than a minute ago or is being asked now, the service doesn't deploy from a roll code repo, the server has no model, or the owner has none or is past their cap; 503 when the model gave no answer | write |
| `GET /v1/apps/services/{id}/environments/{env}/deploys/{dep}/vulnerabilities?severity=` | | `{ scan: ScanSummary \| null, vulnerabilities: Vulnerability[] }`: its image's known vulnerabilities, most severe first (then those with a fix), at most 2,000 (the counts are of all); `severity` keeps those at it or worse; a pruned deployment's list is empty, its counts kept | read |
| `GET /v1/apps/services/{id}/environments/{env}/deploys/{dep}/commits` | | `DeployCommits`: what a repo deployment brings over the live one (for the live one: the one live before it), along its first parents, newest first, at most 50 | read |
| `POST /v1/apps/services/{id}/environments/{env}/deploys/{dep}/approve` | | 200 `{deployment, approved_by, needed, live, waiting?}`: a person's approval (signed in or the operator; a token's is 403) of a deployment waiting at `ready` where the environment asks for `approvals`, never its own author's (403); the one that makes the count puts it live (`live`: the new deployment), unless a freeze still stops it (`waiting` says why); 409 when the environment asks for none or it isn't waiting | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/deploys/{dep}/rollback` | `{ keep_variables? }` (default false: the variables come back as they were when it was live); `X-Roll-Override: 1` puts a never-live `ready` build live during a freeze, audited; or `{ no_traffic: true, variables: {NAME: value} }`: a view of `dep` at its own URL with some of the environment's variables given values of its own (1–50 of its names, each ≤ 64 KiB; `variables` without `no_traffic`, or `no_traffic` without `variables`, is a 400): sealed, never listed, never put live (no `deploy.ready`), not counted toward what's kept, pruned a day after its URL was last visited | 202 `Deployment` (a view's `url` is its own) | deploy*; admin with `X-Roll-Override: 1` |
| `POST /v1/apps/services/{id}/environments/{env}/rollback` | `{ keep_variables? }` | 202 `Deployment`: back to the deployment live before what runs, walking back through rollbacks (after B → A the next goes before A, never forward to B); 409 when there's none | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/push` | | `PushState[]`: the environment's `run.push` rules as they stand, read on the service's owner: each group's `lag` (records past its committed offset, on the partitions it has committed on: Kafka's rule, so 0 before its first delivery), `delivered`, `dead_lettered`, `last_delivered_at`, `failing_since` and `last_error` since that node began delivering, and `off` (why nothing is delivered) | read |
| `POST /v1/apps/services/{id}/environments/{env}/config/rotate` | | `Environment`: its runtime config gets a new read token (a leaked `ROLL_CONFIG_URL`); the old address reads until a deployment made after the rotation is live (of the service, or of its site's functions service), so a redeploy finishes it; audited as `audit.config.rotated`. 404 when it has no runtime config; 409 while the last rotation's old address still reads (redeploy first) | write |
| `POST /v1/apps/services/{id}/environments/{env}/redeploy` | `{ message?, closed? }` (`closed` as `NewDeploy`'s); `Idempotency-Key` header optional (a retry with it answers the first call's redeploy; a key a deploy of another kind used is a 409) | 202 `Deployment` (trigger `settings`) | deploy*; admin with `X-Roll-Override: 1` |
| `POST /v1/apps/services/{id}/environments/{env}/promote` | `{ from, message?, expect? }` (`expect`: a deployment id; 409 when `from`'s live deployment isn't it, checked against the read the promotion is made from) | 202 `Deployment` (trigger `promote`: `from`'s live image, no build, with `env`'s variables; `rollback_of` names the promoted deployment) | deploy*; admin with `X-Roll-Override: 1` |
| `POST /v1/apps/services/{id}/environments/{env}/hook` | | 201 `{ path }`: a new deploy hook, its path shown once (prefix the API's base URL; the token is `rlk_…` with a checksum, so secret scanning finds a leaked one); the old one stops working | admin (a standing credential that deploys) |
| `DELETE /v1/apps/services/{id}/environments/{env}/hook` | | 204 | admin |
| `POST /v1/apps/hooks/{id}/{env}/{token}?image=` | none, or a repository's push webhook (GitHub's or GitLab's `push` JSON) | 202 `Deployment` (actor `deploy hook`); for a Git URL service a push to the environment's branch deploys that commit (trigger `push`, the commit's subject as its message, keyed by the commit so a redelivery is the same deploy), and another branch, a deleted branch, a `[skip ci]` commit or GitHub's ping is 200 `{ ignored }`; `image`: another tag or digest of an image service's own repository, else 400; any token that isn't the live hook's is a 404. No API token: the URL is the secret (rolld's `RULES` let it through to the hook's own check; access lines log the route pattern, never the token). Each deploy is `audit.hook.deployed` in the activity, with the caller's address | none (the URL) |
| `GET /v1/apps/services/{id}/environments/{env}/volume/backups` | | `Backup[]`, newest first (7 kept) | read |
| `POST /v1/apps/services/{id}/environments/{env}/volume/backups` | | 201 `Backup` (now) | write |
| `POST /v1/apps/services/{id}/environments/{env}/volume/backups/{bid}/restore` | `{ from?: string }` (another environment of the service, whose backup `bid` is: seeding staging from production; 404 for one it doesn't have) | `Backup` (the volume as it was; what's there now backed up first under this environment's name; the environment asleep meanwhile) | delete (it replaces the volume's files); with `from`, reveal on that environment too |
| `POST /v1/apps/services/{id}/environments/{env}/deploys/{dep}/cancel` | | `Deployment` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/deploys/{dep}/rollout` | `{action: "advance" \| "all" \| "abort"}` | 202 `{deployment}`; 409 when it isn't waiting | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/restart` | | 202 `Environment` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/cache/purge` | `{ paths?, tags? }` (1–100 absolute paths, any query, a trailing `*` a prefix; 1–100 tags its answers carried, any case; neither: everything) | `{ purged }`: how many kept answers went (on-demand revalidation, "Public traffic"); a Next app's shared cache too (everything emptied, or those tags and the paths' own expired) | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/instances/{name}/restart` | | `Environment` (a web service's: a fresh instance answers before this one drains; 404 for no such instance, 409 if the fresh one didn't start: this one keeps serving) | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/sleep` | | `Environment` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/wake` | | `Environment` | write (it starts compute someone pays for; a visitor's request wakes a public environment through the proxy, not this route) |
| `GET /v1/apps/services/{id}/environments/{env}/variables` | | `Variables` (values masked) | read |
| `GET /v1/apps/services/{id}/environments/{env}/pending` | | `{ variables: { added, removed, changed } \| null, run: string[], secrets: boolean \| null }` (secrets: the project's secrets changed since, by roll secrets' digest; null without one): what the next deploy changes (names, never values; variables against the snapshot kept at the live deployment's switch, null without one; run: fields the service sets that the live deployment doesn't run with). Links and project secrets move `needs_deploy` too, unnamed | read |
| `GET /v1/apps/services/{id}/environments/{env}/variables/values` | | `{[name]: value}`, never a sealed variable (nor a reference that reads one); 403 naming them when some come from roll secrets and roll secrets wouldn't show them to the caller (`SecretSource::may_reveal`: each key read as the caller, its base, imports and `${env.KEY}` references only as far as they may reveal), never a partial answer | reveal |
| `PATCH /v1/apps/services/{id}/environments/{env}/variables` | `{set?: {[name]: value}, unset?: string[], seal?: string[], files?: {[name]: path \| null}, runtime_only?: {[name]: boolean}, copy_from?: string, copy?: string[]}` (`copy_from` with `copy`: those names' values from another environment's own variables, set here as `set` sets them; 400 for a sealed one or one it lacks; reveal on `copy_from` too. set, then unset, then seal: names set here or already set; 400 for another; nothing unseals, a new value stays sealed. `files`: kept as a file at an absolute path in its instances, not in their environment, from the next deploy (0444; at most 20; not under `/proc`, `/sys` or `/dev`, no `..`; two can't share a path; null puts it back; unsetting a variable drops its file; 400 otherwise; on Fargate the deploy fails for now). `runtime_only`: true gives it to its instances only, never a build's steps (from the next deploy; one of its own variables, else 400); false gives it to builds again; unsetting drops it. A value `roll://ENV[/folder…]/KEY` is read from roll secrets at each deploy: its own environment's as the service, another environment's as whoever set it (bound to them when it's set; a 403 unless they may reveal it for the service now) | `Variables` | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/diff?from=` | | `{from, to, variables: {missing: {name, sealed}[], extra: string[], differ: string[] \| null}, settings: {field, from, to, put}[]}`: how `env` differs from `from` (Railway's environment sync), never a value. `variables`: its own variables' names, `differ` null for a token without reveal on both; `settings`: `run.*`, `wait_for_ci`, `follow_tag`, `redeploy_on_secrets`, `traces`, `vitals`, `report`, `egress`, `allow_ips`, `rate_limit`, `firewall`, `headers`, `routes`, `pages.*` (whether each has its own), each with the `PUT …/environments/{env}` body that makes it `from`'s. A token needs read on `from` too (403) | read |
| `GET /v1/apps/services/{id}/environments/{env}/logs?follow=&tail=&since=&before=&instance=&deployment=&stream=&level=&where=&q=&http=&status=&min_ms=&by=` | | `LogLine[]` or SSE; `before` (unix ms) reads back to a time (not followed), `instance`/`deployment`/`stream` narrow it (followed too), `level` keeps lines at that level or worse (a line that says none isn't; 400 for another word), `where` keeps lines whose JSON or logfmt fields say all its terms (`level:error user_id:42`, `key:*`, `-key:value`, `a.b:x`, quoted values; at most 10; 400 for one that isn't `key:value`), `X-Roll-Search-Truncated: 1` when a read back stopped short; `http=1`: the proxy's access log (`stream: "http"`, text `AccessLine`), `status` (`500`, `5xx`, `400-499`), `min_ms` and `by` (one reason, or `any`: every answer roll gave itself) filtering it | read |
| `GET /v1/apps/services/{id}/environments/{env}/slo` | | `SloReport`: its objective (the live deployment's `run.slo`, else the service's) against its window: per indicator the total, the failures, the share that met it and the budget left (1 untouched, below 0 spent); minutes for the last week, hours before (an hour's latency fails whole when its slowest minute did); `burning`: the burn alert firing now; `{ slo: null }` without one | read |
| `GET /v1/apps/services/{id}/environments/{env}/metrics?minutes=&format=` | | `Metrics`; `format=prometheus`: the last finished minute as Prometheus text (`roll_apps_requests_per_minute`, `…_errors_per_minute`, `…_latency_ms{quantile}`, `…_cpu_seconds_per_minute`, `…_memory_bytes`, `…_instances`, `…_cold_starts_per_minute`, `…_cold_start_ms`, labelled `service`, `service_id`, `environment`) | read |
| `GET /v1/apps/services/{id}/environments/{env}/routes?minutes=` | | `RouteStats` (with `deployments` and `instances`: the same requests by the deployment and the instance that answered, the busiest 50 of each) (the last 5–60 minutes, default 60): the access log grouped by method and route, id-like segments `:id` and no query; the busiest, the most 5xx and the slowest (5 requests or more), each at most 50 | read |
| `GET /v1/apps/services/{id}/environments/{env}/errors?minutes=` | | `{ minutes, groups: { fingerprint, kind: "python" \| "java" \| "node" \| "go" \| "ruby" \| "php" \| "dotnet" \| "rust" \| "elixir" \| "line", title, count, first_seen, last_seen, instances, sample }[], scanned, cut }`: the environment's errors from its log (5–10,080 minutes, default 1,440), each instance's stack traces joined from their lines and grouped by their error and first frames (line numbers, addresses and ids taken out), error lines with no trace by their message the same way; the most frequent first, at most 50, `sample` the newest whole (at most 8 KiB); at most the newest 100,000 lines read (`cut`) | read |
| `PATCH /v1/apps/services/{id}/environments/{env}/errors/{fingerprint}` | `{ state: "open" \| "resolved" \| "muted" }` | `{ fingerprint, state }`; 404 for one the environment never had | write |
| `GET /v1/apps/services/{id}/environments/{env}/forms` | | `{ name, submissions, last_at }[]`: a static site's forms with submissions kept | read |
| `GET /v1/apps/services/{id}/environments/{env}/forms/{form}/submissions?limit=&before=` | | `{ id, created_at, fields: { name, value }[] }[]`, the newest first (limit 1–200, default 50; `before` a submission's id: the page after it); each read is `audit.forms.read` | reveal (what people sent) |
| `DELETE /v1/apps/services/{id}/environments/{env}/forms/{form}/submissions/{sub}` | | 204; `audit.forms.deleted` | delete |
| `GET /v1/apps/services/{id}/environments/{env}/app-metrics?minutes=` | | `{ minutes, step_minutes, series: { name, kind: "gauge" \| "sum" \| "histogram", unit, attrs: {key, value}[], shows: "value" \| "per_second" \| "mean", points: [ms, number][] }[], cut }`: what the app's OpenTelemetry SDK sent as metrics (5–10,080 minutes, default 60), each series (a name and its attributes; at most 50, the busiest first) at most 60 points: a gauge's last value, a cumulative counter's rate a second, a histogram's mean | read |
| `GET /v1/apps/services/{id}/environments/{env}/vitals?minutes=` | | `{ minutes, samples, metrics: { lcp?, inp?, cls?, fcp?, ttfb?: { p75, rating: "good" \| "needs-improvement" \| "poor", samples } }, routes: { route, samples, metrics }[], cut }`: what visitors' browsers measured with `vitals: true` (the last 60–10,080 minutes, default 1,440), p75s rated by web.dev's thresholds, overall and for the 20 routes with the most page loads | read |
| `GET /v1/apps/services/{id}/environments/{env}/analytics?minutes=` | | `Analytics` (the last 60–10,080 minutes, default 1,440): visitors and page views from the access log, no script or cookie | read |
| `POST /v1/apps/services/{id}/environments/{env}/visit` | `{ to }` | `{ url }`: a one-use ticket (a minute) into a sign-in-gated environment for the person signed in, as the URL that opens `to` (a URL at one of its names: its generated hostname or an active custom domain; 400 otherwise, and for an environment without `access.signed_in`); the dashboard's `/dashboard/apps/visit` asks for it and goes on | a person signed in only (403 for a token); rolld's gate lets in only admins until roll apps serves users |
| `GET /v1/apps/services/{id}/environments/{env}/traces?minutes=&min_ms=` | | `{ minutes, traces: TraceSummary[], cut }`: recent traces (5–10,080 minutes, default 60), newest first, at most 100; `min_ms` keeps the slower ones | read |
| `GET /v1/apps/services/{id}/environments/{env}/traces/{trace}` | | `{ trace, spans: Span[] }` in start order (the last week's); 404 when none | read |
| `GET /v1/apps/services/{id}/environments/{env}/usage?days=` | | `Usage` | read |
| `GET /v1/apps/services/{id}/environments/{env}/runs` | | `CronRun[]` | read |
| `POST /v1/apps/services/{id}/environments/{env}/runs` | | 202 `CronRun` (run now) | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/runs/{run}` | | `CronRun` | read |
| `GET /v1/apps/services/{id}/environments/{env}/runs/{run}/logs?follow=&tail=` | | `LogLine[]` or SSE (ends with the run, then `end`): its container's lines (named by the run's id) | read (they're the environment's log's lines) |
| `POST /v1/apps/services/{id}/environments/{env}/runs/{run}/cancel` | | `CronRun` (409 unless it's running) | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/files?path=&instance=` | | `application/x-tar`: what's at the absolute `path` in the instance (a shell's, or `instance`), `X-Roll-Instance` naming it; 400 for a relative path, 409 when the runtime couldn't copy | admin (files hold what the app keeps, its secrets too) |
| `PUT /v1/apps/services/{id}/environments/{env}/files?path=&instance=` | a tar (at most 100 MiB) | `{ instance, path }`: its files written under the directory `path` | admin (what's written runs next) |
| `GET /v1/apps/services/{id}/environments/{env}/shell?instance=&cols=&rows=&console=&command=&cpu=&memory_mb=&deployment=` | WebSocket | a terminal: binary both ways, text `{"resize":[cols,rows]}`; `console=true` makes it a fresh container of the live deployment (or, with `deployment`, of another kept one's image and settings) running `command` (a login shell without one), sized by `cpu` (0.25–8) and `memory_mb` (256–16384), removed when it closes (`command`, `cpu`, `memory_mb` without `console`: 400) | admin (a shell runs anything with the environment's variables, as agents' shells) |
| `GET /v1/apps/services/{id}/environments/{env}/tunnel?port=` | WebSocket | one TCP connection to the web service's own port, or with `port` another port of the app (a debugger's; reached inside its instance by a relay in its network namespace, published or not), binary both ways (`roll apps proxy`); wakes an instance and holds it while open; at most 20 per environment (409); 409 for a worker, a cron service or a static site | admin (it reaches the port past the allow list, access and maintenance) |
| `POST /v1/apps/services/{id}/environments/{env}/shell/sessions` | `{ instance?, cols?, rows?, console?, command?, cpu?, memory_mb?, deployment? }` (a console as on the WebSocket; `deployment`: another kept deployment's image and settings, 404 another environment's, 409 never built or cleaned up) | 201 `{ id }` (a shell over HTTP: the dashboard's terminal) | admin (a shell runs anything with the environment's variables, as agents' shells) |
| `GET /v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}/screen` | | SSE: base64 chunks (the scrollback first), then `event: closed` | admin (a shell runs anything with the environment's variables, as agents' shells) |
| `POST /v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}/keys` | `{ data }` (base64, at most 64 KiB) | 204 | admin (a shell runs anything with the environment's variables, as agents' shells) |
| `POST /v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}/resize` | `{ cols, rows }` | 204 | admin (all of a shell session is) |
| `DELETE /v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}` | | 204 | admin (what closes an admin's shell is admin) |
| `GET /v1/apps/services/{id}/environments/{env}/jobs` | | `Job[]`, newest first | read |
| `POST /v1/apps/services/{id}/environments/{env}/jobs` | `{ command, timeout_s?, tasks?, retries?, parallelism?, cpu?, memory_mb? }` (`cpu` 0.25–8 and `memory_mb` 256–16384: each task's container at that size, metered at it, Heroku's `run --size`; unset, the service's; tasks 1–100: that many containers, `parallelism` at a time (1–10, default ten or fewer tasks: a node's job containers are shared), each with `ROLL_TASK_INDEX` and `ROLL_TASK_COUNT`; `retries` 0–5: a failed or timed-out task tried again in a fresh container with `ROLL_TASK_ATTEMPT`, as a cron run's retries wait, never after a cancel or once another deployment is live; the job fails on the first task that does in the end) | 202 `Job` | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/jobs/{job}` | | `Job` | read |
| `GET /v1/apps/services/{id}/environments/{env}/jobs/{job}/logs?follow=&tail=` | | `LogLine[]` or SSE (ends with the job, then `end`) | reveal (a job's output can be the variables) |
| `POST /v1/apps/services/{id}/environments/{env}/jobs/{job}/cancel` | | `Job` | deploy* |
| `GET /v1/apps/services/{id}/environments/{env}/private-network` | | `PrivateNetwork` (404 none) | read |
| `PUT /v1/apps/services/{id}/environments/{env}/private-network` | `{ secret, routes, kind?, login_server? }` (a private network into the customer's own, Cloud Run's Direct VPC egress and Fly's WireGuard peering: `secret` is `roll://<this env>/…`, the WireGuard config in roll secrets, bound to whoever sets it and read as them at each use (400 another environment's, 403 one they may not reveal); the config must be one `[Interface]` with an `Address` and `[Peer]`s: `PostUp`/`PostDown`/`PreUp`/`PreDown`, `DNS`, `Table`, `FwMark` and a full tunnel (`AllowedIPs` `0.0.0.0/0` or `::/0`) are 400s, said without the config's text; `routes`: 1–20 `{ name, target }`, `name` a DNS name with a dot (`db.acme.corp`, never under `.internal`) that answers on the project network, `target` `host:port` in the customer's network (WireGuard: an address inside the config's `AllowedIPs`; Tailscale: a tailnet address or a `….ts.net` name, not port 1055; never loopback, link-local or broadcast), each port its own (400), each name one service's in the project environment (409); `kind` `wireguard` (the default) or `tailscale`: the secret is then a Tailscale auth key (one token), the node userspace and ephemeral with no capability, its routes through its SOCKS5 server (a target may be a MagicDNS name), and `login_server` a Headscale control server's http(s) URL (one line, nothing a command line could split). A closed environment's peer answers on its closed network and reaches its endpoint through a network of its own) | `PrivateNetwork` (its peer made now when something is deployed) | admin (it reaches into the customer's network, read as the setter) |
| `DELETE /v1/apps/services/{id}/environments/{env}/private-network` | | 204 (its peer and forwarders stop now) | admin |
| `GET /v1/apps/services/{id}/environments/{env}/sandboxes` | | `Sandbox[]`, oldest first | read |
| `POST /v1/apps/services/{id}/environments/{env}/sandboxes` | `{ image?, ttl_s?, cpu?, memory_mb?, variables?, network? }` (a short-lived container to run commands in, Vercel Sandbox's and Fly Machines': the live deployment's image, or `image` (a public one); up for `ttl_s` (60–86400, default 600) then removed by the owner's minute sweep; `cpu` 0.25–4 (0.5), `memory_mb` 256–8192 (512), metered as a console for as long as it ran; `variables` (false): the live deployment's variables and files, as a console has them, its links reached through them; `network`: `none` (default: a network of its own, the internet and nothing of the project) or `environment` (its private network); a closed environment's are always on its network (`none` asked is a 400). At most 10 per environment (409); 409 with nothing deployed; 402 at the spend limit, 409 paused) | 201 `Sandbox` | admin (it runs anything, with the variables when asked, as a shell) |
| `GET /v1/apps/services/{id}/environments/{env}/sandboxes/{sbx}` | | `Sandbox` | read |
| `DELETE /v1/apps/services/{id}/environments/{env}/sandboxes/{sbx}` | | 204 (stopped and removed now) | admin |
| `POST /v1/apps/services/{id}/environments/{env}/sandboxes/{sbx}/exec` | `{ command, timeout_s? }` (`sh -c <command>`, at most 64 KiB; killed after `timeout_s`, 1–600, default 60) | `{ exit_code: number \| null, timed_out, stdout, stderr, truncated }` (each stream's first 1 MiB as text, `truncated` when more was dropped; `exit_code` null when it was killed at its timeout). 409 once the sandbox ended. Audited without its text (`sandbox.command`, its length and exit code) | admin |
| `GET /v1/apps/services/{id}/environments/{env}/sandboxes/{sbx}/files?path=` | | the file's bytes (`application/octet-stream`, at most 1 MiB; 404 none, 400 a directory or more) | admin (a file can be anything the sandbox holds) |
| `PUT /v1/apps/services/{id}/environments/{env}/sandboxes/{sbx}/files?path=` | the file's bytes (at most 1 MiB) | `{ path, bytes }` (written at `path`, an absolute path without `..`, its directory made) | admin |
| `GET /v1/apps/services/{id}/environments/{env}/domains` | | `Domain[]` | read |
| `POST /v1/apps/services/{id}/environments/{env}/domains` | `{hostname}` | 201 `Domain` | deploy* |
| `POST /v1/apps/services/{id}/environments/{env}/domains/{host}/verify` | | `Domain` | read |
| `PATCH /v1/apps/services/{id}/environments/{env}/domains/{host}` | `{ redirect_to?: string \| null, port?: number \| null }` (a field left out stays) | `Domain` (`redirect_to`: another hostname of the environment that serves it: no loops or chains; null serves the app again. `port`: another of the app's ports this hostname goes to, Railway's target port, published from the next deploy (until then the host answers 502, never the main port); null: the service's own. A web service's custom domain only, not in a closed environment (409)) | deploy* (it changes what a hostname serves) |
| `DELETE /v1/apps/services/{id}/environments/{env}/domains/{host}` | | 204 | delete |

- Creating a service makes its `production` environment, places its card on the project's canvas
  (at `x`, `y`, or the first free spot when both are left out or 0), and, for a repo or an image,
  queues the first deploy at once (a Git URL too). An upload service waits for `roll up`. A Git
  source's deploy takes a `revision` only as a full commit id.
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

type Prices = { cpu_second: number; gb_second: number; egress_gb: number; build_minute: number; stored_gb_month: number /* what roll stores: logs kept past the first week (an environment's 30 or 90 days) and build caches (shared by the services that build from one), metered hourly as stored_gb_hours */ };

type Source =
  | { kind: "repo"; repo: string /* owner/name in roll code (GitHub repos are mirrored there) */; root?: string }
  | { kind: "image"; image: string /* docker.io/library/nginx:1.27 */ }
  | { kind: "upload" } /* `roll up`: each deploy carries its tree, kept only until its build ends */
  | { kind: "git"; url: string /* a public repository: https://, no credentials, a public address (400 otherwise) */; root?: string } /* each deploy fetches its branch's head shallowly; a new commit deploys within five minutes */;

type Build = {
  /** auto: detect (a Dockerfile wins); dockerfile: use `dockerfile`. */
  builder: "auto" | "dockerfile" | "functions"; // functions: a static site's netlify/functions and api/ as a Node service ("Functions in a static site")
  dockerfile?: string; // path in the tree, default "Dockerfile"
  install?: string;    // overrides the detected commands
  build?: string;
  start?: string;
  /** A repo or Git URL source deploys a push only when it changes a path these match
   *  (gitignore-style, from the repo's root, as CODEOWNERS; at most 50). roll.toml `[app] watch`
   *  wins over it. */
  watch?: string[];
  spa?: boolean;            // a single-page app (a page path with no file answers index.html); unset: detected
  target?: string;          // the tree's own Dockerfile's stage to build (--target): letters, digits, _ - . (64 at most)
  /** System packages the generated Dockerfile installs, apt (or apk on Alpine) in every stage built on an image:
   *  at most 50 names of lowercase letters, digits and + - . : = ~ _; refused for a static site and a
   *  distroless runtime, left to the tree's own Dockerfile. roll.toml `[app] packages` wins over it. */
  packages?: string[];
  /** The longest its build may take, seconds (Render's, Cloud Build's): 60–21600, held to the node's
   *  own limit (`ROLL_APPS_BUILD_TIMEOUT`, minutes, 30 by default; the build log says so when it
   *  is); unset: the node's. */
  timeout_s?: number;
  /** Its builder (Vercel's, Render's, Netlify's bigger build machines): "small" (2 vCPU, 4 GiB;
   *  the default), "medium" (4, 8 GiB) or "large" (8, 16 GiB): CodeBuild's compute type on the
   *  fleet, its build minutes metered ×1, ×2 or ×4; one node's Docker builds unsized (the build
   *  log says so) and meters ×1. */
  size?: "small" | "medium" | "large";
  /** Submodules fetched into the tree (default false: left out): each one .gitmodules names, at
   *  the commit its gitlink pins (relative URLs against the repository's, as git reads them),
   *  two levels deep and 32 at most. A Git URL service's come from their own public URLs; a roll
   *  code repo's from public URLs, public roll code repos (a relative URL, or roll code's Git
   *  address), and the private roll code repos `submodule_repos` names (any other fails the
   *  build: "<repo> isn't public; name it in build.submodule_repos"). */
  submodules?: boolean;
  /** The private roll code repos (`owner/name`, 32 at most) a roll code repo's submodules may
   *  come from. A token that sets it needs read on each (the catalogue lends it), so a writer to
   *  the service's repo can't build a repo it can't read by pinning it. */
  submodule_repos?: string[];
  /** The language's OpenTelemetry agent in a generated Dockerfile (default false): Node's
   *  (`@opentelemetry/auto-instrumentations-node`, by NODE_OPTIONS' --require) and Python's
   *  (`opentelemetry-distro`, bootstrapped, by PYTHONPATH's sitecustomize), pinned; started only
   *  while the environment's traces are on, joined to the app's own value; the plan's `otel`
   *  says which. None in the tree's own Dockerfile or another language (a note says so). */
  otel?: boolean;
  /** A Git URL service's Git LFS objects in place of their pointers (default false): from the
   *  repository's LFS batch API, checked against each pointer's sha256 and size, 2 GiB in all. */
  lfs?: boolean;
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
  retries?: number;         // cron: a failed or timed-out run is tried again this many times (0–5), after 30 s, then twice as long each time (10 min at most); each try its own run, `cron.failed` only for the last (a canceled run isn't tried again)
  min_instances: number;    // web: 0 scales to zero (default 0); worker: at least 1
  max_instances: number;    // default 3
  concurrency: number;      // requests in flight per instance before scaling out (default 50)
  cpu: number;              // vCPU per instance, 0.25–8 (default 0.5)
  memory_mb: number;        // 256–16384 (default 512)
  health_path?: string;     // web: GET it for a 2xx/3xx; none: the port accepting a connection.
                            // Also checked while running: health_failures in a row replace the instance
  health_timeout_s: number; // default 60
  health_every_s?: number;  // 2–300: between two checks while running; unset 10
  health_failures?: number; // 1–10: failed checks in a row that replace an instance; unset 3
  health_command?: string;  // worker: a shell command in each instance (`sh -c`, Docker's HEALTHCHECK), healthy on exit 0;
                            // unhealthy is replaced as a crash. Changing it is a deploy (it runs in production)
  suspend_after_s: number;  // default 30; 0 never suspends
  sleep_after_s: number;    // default 600; min_instances > 0 never sleeps below min
  volume?: { mount: string; size_gb: number; backups?: { every_hours: number /* 0–168; 0: by hand only */; keep: number /* 1–90 */ } /* none: daily, seven kept */ };
  alerts: { memory_pct: number; error_pct: number; p95_ms: number; capacity_pct: number; log_match?: string; log_count?: number; cpu_pct: number; metrics?: { name: string; above?: number; below?: number; minutes?: number; sum?: boolean }[] }; // default 90, 5, 0, 10 (0 off); cpu_pct: each instance's share of its cpu every minute of five, default 0 (off), `alert.cpu`; metrics (at most 10): one of the app's own gauges (each series' newest point, the largest, or their sum) above or below its one threshold in `minutes` (1–5, default 3) judgments running, `alert.metric`; capacity_pct: the share of five minutes' requests that found every instance busy at max_instances, `alert.capacity`: `alert.*` events over five minutes; log_match: text its stdout/stderr holds log_count times (default 10) in five minutes, `alert.log`
  /** A gradual rollout (README "Gradual rollouts"): increasing percents 1–99 (at most five; []
   *  switches at once, the default), each for step_s (1–3600, default 120); a step whose 5xx
   *  share reaches error_pct (1–100, default 5) of 20+ requests, and beats the old one's, rolls
   *  back. Web services only; refused with a volume. manual: each step, once it passes, holds
   *  its split until a person says (`…/deploys/{dep}/rollout`), still judged while it waits. */
  rollout: { steps: number[]; step_s: number; error_pct: number; manual?: boolean; slo_burn?: number /* with run.slo: a step fails when the new deployment spends its budget this many times as fast as it lasts (1–1000, default 14.4) */; p95_pct?: number /* a step fails when the new deployment's p95 response time is more than this many percent above the old one's, with 20 requests on each side (1–1000, off unless set) */; overlap?: false /* the old instances stop (draining for drain_s) before the new ones start, so two never run at once (a bot, a scheduler); a web service's requests wait for the new one; no steps with it */ };
  /** The app speaks HTTP/2 without TLS (a gRPC server): the proxy and health checks use h2c,
   *  `te: trailers` passes, trailers come back. A WebSocket upgrade stays HTTP/1.1. Default false. */
  h2c?: boolean;
  /** With h2c (a web service): its health is gRPC's, a unary `grpc.health.v1.Health/Check` for
   *  this service ("" the whole server; at most 100 characters) that must answer SERVING, at
   *  start and every health_every_s, in place of health_path (Cloud Run's gRPC probes). */
  health_grpc?: string;
  /** A worker only: its instances follow `group`'s lag on `stream` (a roll stream on the
   *  service's project; another's is never read), one per `per_instance` (default 1000) behind,
   *  within min_instances–max_instances; read every 10 s on the owner, scaled in one at a time. */
  scale?: { stream: string; group: string; per_instance?: number } | { metric: string /* a gauge of its own, by name */; sum?: boolean /* their sum; default the largest series */; per_instance?: number }; // a worker's: a stream group's lag, or its own gauge (min_instances ≥ 1 for that: a stopped worker reports nothing)
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
  busy_cpu?: number; /* 1–100: a web service's instance over this percent of its CPU counts as busy (Railway's sleep that watches activity): not suspended, put to sleep or scaled in until 30 s under it ("<instance> kept awake: CPU at N%" in the log); Docker only, as cpu_target */
  latency_target_ms?: number; /* 50–60,000: a web service also scaled on its answers' p95 (another instance while a window's p95 is over it) */
  oidc_audiences?: string[]; /* at most 5, 1–256 printable characters each: audiences its instances may ask roll's OIDC issuer for at ROLL_OIDC_URL (sealed; GET ?aud=, the first by default, also ROLL_OIDC_AUDIENCE) → {token, expires_at}: sub = roll-apps:<project>:<service>:<env>, an hour, the same token answered for a minute; 403 for another audience, an untrusted preview or a project that doesn't stand for the service; 503 with no issuer */
  /** And on memory (Render's target memory): the same, against the percent of each instance's
   *  `memory_mb` in use. 10–100; unset, not on memory. With both, whichever asks for more
   *  instances decides. Docker only, as cpu_target. */
  memory_target?: number;
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
  /** Its port speaks UDP (Fly's UDP: a game server, DNS): each environment answers on a UDP port
   *  of its own from the node's ROLL_APPS_UDP_PORTS (Environment.udp), each client a flow whose
   *  first datagram wakes an instance, relayed both ways and ended after 60 s idle (1024 flows
   *  at most), its allow list, maintenance and rate limit (flows per client) applied. A web
   *  service, not a TCP one; no health_path, health_grpc, h2c, affinity, gradual rollout or
   *  sidecars (it's up once it runs); not in a closed environment. One node. */
  udp?: boolean;
  /** A deployment that enters a crash loop within ten minutes of going live is replaced by the
   *  one before it (a rollback: its image, no build, actor "crash loop"); a rollback's own
   *  deployment never is, so there are no two in a row. */
  rollback_on_crash?: boolean;
  /** A built service: build the same revision again, without the cache, when one of its base
   *  images moves in its registry (checked hourly on the owner). */
  rebuild_on_base?: boolean;
  queue_s?: number; /* 1–60: a web service's hard limit: at max_instances with every instance at its concurrency, a request waits this long for a slot, then a 429 (counted over capacity); unset, the least busy instance takes it past its concurrency */
  skew_protection?: boolean; /* a web service: a request that names a deployment (x-deployment-id, ?dpl=, or the __roll_dpl cookie each page load gets), not a page load, goes to it when it's one of the last three live before the live one, which went live less than a day ago */
  /** A deploy whose image has a known vulnerability this severe or worse fails before it goes live: the scan runs
   *  before the rollout, and a scan that can't run or finish fails it too. Never a rollback's. Changing it is a deploy. */
  block_on?: "critical" | "high" | "medium" | "low";
  /** Its service level objective: objective% of requests not 5xx (and, with latency_ms, of minutes with requests whose p95
   *  stays under it) over window_days (1–60, default 28). A burn 14.4× as fast as the budget lasts over an hour and its
   *  last five minutes, or 6× over six hours and their last half hour, is alert.slo; a web service's. Changing it is a write. */
  slo?: { objective: number /* 90–99.999 */; latency_ms?: number /* 1–60000 */; window_days?: number };
  /** Its own Prometheus endpoint (Fly's [metrics]): GET'd on each running instance every 15 s by the owner (never one
   *  asleep), at most 4 MiB and 2,000 series a scrape; counters, gauges, and histograms' and summaries' sum and count,
   *  each with an `instance` label, on …/app-metrics. port: the app's own by default (a worker's must say); not in a
   *  closed environment. Changing it is a write. */
  metrics?: { port?: number; path?: string /* default /metrics */ };
  /** Floors by the clock: in any of a rule's weekly windows (as a freeze's, in its timezone), its min_instances (up to
   *  max_instances) is the floor when higher; at most 10 rules. Changing it is a write. */
  schedule_scale?: { windows: { from: string; to: string }[]; timezone?: string; min_instances: number }[];
  /** Counts of its log lines: each minute, the lines of its output (stdout, stderr) matching pattern (any case, at most
   *  200 characters) as a gauge named name (a metric's name, unique), on …/app-metrics with source=log and for scale; at
   *  most 10. Changing it is a write. */
  log_metrics?: { name: string; pattern: string }[];
  /** Commands on a schedule on any service: each a job of the live deployment at its due times, skipped while its
   *  last job runs; at most 20; name a-z, 0-9 and - (unique, 32 at most), command 1–2000, timeout_s 1–86400. */
  schedules?: { name: string; schedule: string; command?: string; path?: string /* a web service's path GET'd instead of a command (Vercel's crons): through its own proxy on the owner, past its allow list, firewall, rate limit and access gate, with Authorization: Bearer $CRON_SECRET when the environment has one; a call still out skips the next; a non-2xx/3xx or no answer is cron.failed (data.path, http_status or error); timeout_s default 300 */; timezone?: string; timeout_s?: number; cpu?: number /* a command's run at its own size: 0.25–8 */; memory_mb?: number /* 256–16384 */; auth?: "oidc" /* a path's call: roll's OIDC token for the service (sub roll-apps:<project>:<service>:<env>, aud the environment's URL) as the bearer instead of CRON_SECRET; none issued is cron.failed */ }[];
  push?: { stream: string /* st_…, on the service's own project (the operator's) */; topic: string; group: string; path: string; environment?: string /* default production */; dead_letter?: string /* a topic of the same stream */; max_attempts?: number /* 1–100, default 5, with dead_letter */; timeout_s?: number /* 1–600, default 30 */; max_in_flight?: number /* 1–32, default 4: records POSTed at once, each from its own partitions */; auth?: "oidc" /* roll's OIDC token for the service (sub roll-apps:<project>:<service>:<env>, aud the environment's URL) as the bearer instead of CRON_SECRET; while none is issued nothing is POSTed */ }[]; /* a web service's, at most 4 (Cloud Run's Pub/Sub push), set here only (roll.toml refuses it), needing read on each stream and write with a dead_letter: each record POSTed to path through the environment's own proxy (past its gates, as schedules' calls are) as a CloudEvent in binary mode: its value the body, its content-type header the Content-Type (else application/octet-stream), ce-specversion 1.0, ce-id <topic>/<partition>/<offset>, ce-source /streams/<stream>/topics/<topic>, ce-type sh.roll.stream.record, ce-time, ce-partitionkey (its key, base64), ce-rollpartition, ce-rolloffset, ce-rollattempt, its other headers as ce-<name> (a-z, 0-9, at most 20, never one of those), Authorization: Bearer $CRON_SECRET when the environment has one; the group's offset committed on a 2xx, each partition in order, at least once; a failed POST tried again after 1 s doubling to 60 s, or after max_attempts to dead_letter with x-roll-error and x-roll-source headers */
  sidecars?: { name: string /* a-z, 0-9 and -, at most 20, unique */; image: string /* pinned to its digest with each deployment */; command?: string /* sh -c, instead of the image's own */; env?: { [name: string]: string } /* plain settings, at most 50, shown as they are: never a secret */; variables?: string[] /* the environment's own variables it gets, by name, at most 50 */; cpu?: number /* 0.1–2, default 0.25 */; memory_mb?: number /* 64–4096, default 128 */ }[]; // at most 4, a web service's or a worker's: containers in each instance's network namespace (localhost), started after it, frozen, stopped and removed with it, metered with it; Docker only (the fleet refuses them for now)
};

type Service = {
  id: string;
  project: string;          // workspace project id
  name: string;             // 1–32 of [a-z0-9-], unique in the project
  source: Source;
  build: Build;
  run: Run;
  registry: { server: string; username: string; kind?: "ecr"; region?: string } | null; // a private registry's login (never the password; an ECR login's username is its access key's id)
  git_login: { host: string; username: string; secret?: string } | null; // a private Git repository's login (never the password; `secret`: the project secret it's read from)
  github: { installation: number; repo: string } | null; // connected to roll's GitHub App (`PUT …/github`)
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
  attention?: ("crash_loop" | "capped" | "paused" | "held" | "maintenance" | "deploy_failed")[]; // what wants its owner's eye (absent: nothing)
  needs_deploy: boolean;     // settings, variables, links or the project's secrets changed since the active deployment (cached a minute per environment)
  ephemeral: boolean;        // a preview environment
};

type FieldMatch = { name: string /* 1–128, no spaces */; contains?: string /* 1–256 */ };

type InstanceState = "none" | "stopped" | "suspended" | "waking" | "running" | "crashed" | "deploying";

type Patch<T> = { [K in keyof T]?: T[K] | null }; // null: back to the default
type NewService = { project: string; name: string; source: Source; build?: Patch<Build>; run?: Patch<Run>; variables?: { [name: string]: string } /* production, before its first deploy */; sealed?: string[] /* which of those are sealed from the start (a template's generated secrets) */; registry?: { username: string; password: string; server?: string } /* an image's login, before its first pull (as PUT …/registry) */; git_login?: { username: string; password: string } /* a private Git repository's login, before its branch is checked (as PUT …/git-login) */; links?: { kind: LinkKind; ref: string; prefix?: string }[] /* at most 20, made before the first deploy (as PUT …/links); one that doesn't resolve makes no service */; branch?: string /* a repo or Git source: production follows it, its first deploy that branch's head (default: the repo's default branch); 400 when the repo has no such branch, or a Git URL doesn't answer */; x?: number; y?: number };
type ServicePatch = { name?: string; source?: Source; build?: Patch<Build>; run?: Patch<Run> };

type EnvironmentPut = {
  branch?: string | null;
  ephemeral?: boolean;
  /** Its name on the roll domain (`<hostname>.<domain>`): 3–56 of a-z, 0-9 and -, not starting or
   *  ending with -, never `--`, not roll's own (400); another environment's is 409, and so is any
   *  name while custom domains' CNAMEs point at the old one (no fixed `cname_target`). The old name
   *  stops answering at once (within a second on other nodes); the app's `ROLL_PUBLIC_URL` follows
   *  at its next deploy. */
  hostname?: string;
  /** Private (previews): visitors need `?roll_access=<token>` once (a host-only HttpOnly cookie
   *  `roll_access_<env>`, then a 302 to the URL without it), else a 401, before anything wakes.
   *  The token is 20–200 of [A-Za-z0-9_-]; only its SHA-256 is stored, and the app never sees
   *  the cookie. null: public again. */
  /** Private: visitors need `?roll_access=<token>` once (a host-only HttpOnly cookie
   *  `roll_access_<env>`, then a 302 to the URL without it), else a 401, before anything wakes.
   *  The token is 20–200 of [A-Za-z0-9_-]; only its SHA-256 is stored, and the app never sees
   *  the cookie. `signed_in: true` (with or without a token): people signed in to roll who can
   *  open the project get in. A page load with no session goes to
   *  `<web_url>/dashboard/apps/visit?service=&env=&to=`, which asks `POST …/visit` as them and
   *  sends them back to `/__roll/visit?ticket=&to=` on the same host, where the ticket (one use,
   *  a minute) becomes a host-only HttpOnly cookie `roll_visit_<env>` (12 hours; the app never
   *  sees it) and a 302 to `to`; anything else is a 401. 409 without the node's `web_url`.
   *  Changing access ends every session, and a session asks again every 5 minutes whether its
   *  person may still open the project (a disabled account is turned away). null: public again. */
  /** `bypass_sha256`: the SHA-256 (hex) of an automation's secret (CI's end-to-end tests): a
   *  request with `X-Roll-Preview-Bypass: <secret>` goes in with no cookie or redirect. */
  access?: { token?: string; signed_in?: boolean; password?: string /* 8–200: visitors type it on roll's page (its argon2id hash only kept; a session cookie for 7 days, ended by a new password; 10 tries a client every 10 minutes) */; bypass_sha256?: string } | null;
  /** Creating it: start with this environment's variables, except its sealed ones (ignored when it exists). */
  variables_from?: string;
  /** A push deploys only once its commit's checks pass (statuses other than roll apps' own):
   *  pending waits (up to an hour; `Environment.waiting_for_ci` names it), a failure skips it.
   *  A roll code repo's service, a Git URL service on roll's GitHub App (`PUT …/github`),
   *  whose checks are GitHub's check runs and commit statuses, or one on GitLab (gitlab.com, a
   *  `gitlab.…` host) or Bitbucket with a Git login, whose checks are its commit statuses (its
   *  pipelines' jobs) read with the login's token (roll's own left out; none at all is waited
   *  on for a minute, as they may not have started; roll's deploy statuses are posted there
   *  too); 400 otherwise: another Git
   *  host's service deploys from its workflow after its tests (sdk/apps-github-action) with
   *  the environment's branch unset. */
  wait_for_ci?: boolean;
  /** An image service whose image has a tag: deploy when the tag points at a new digest
   *  (checked every 5 minutes on the owner; actor `tag watch`). 400 otherwise. */
  /** Redeploy (the same image, the new values) when the project's secrets change, checked each minute
   *  on the owner against roll secrets' digest; off by default. */
  redeploy_on_secrets?: boolean;
  /** Its instances send OpenTelemetry traces and metrics to roll: `OTEL_EXPORTER_OTLP_ENDPOINT`
   *  (this environment's address, a token of its own in it), `OTEL_EXPORTER_OTLP_PROTOCOL`,
   *  `OTEL_TRACES_EXPORTER=otlp`, `OTEL_METRICS_EXPORTER=otlp`, `OTEL_LOGS_EXPORTER=none` (its own
   *  `otlp` sends logs too) and `OTEL_SERVICE_NAME` from the next deploy (its own variables win).
   *  409 where the node has no OTLP address. */
  traces?: boolean;
  /** What its code may reach (roll previews' `egress`): `roll` runs its instances, from the next
   *  deploy, on a network of the project environment's own with no route out (Docker
   *  `--internal`): roll is reached (its links' relays, the project's services there), nothing
   *  else. `hosts` (at most 20 names, `*.` for subdomains; never addresses) are still reached on
   *  443 and 80 through its egress proxy, by `HTTP_PROXY`/`HTTPS_PROXY` (with `NO_PROXY` naming
   *  roll's own; the app's own variables win), at public addresses only. 400 for hosts with
   *  `open`; 409 where the node's runtime can't close a network (Fargate). null: `open`. */
  egress?: { mode: "roll" | "open"; hosts?: string[] } | null;
  /** Web Vitals: from the next request its HTML page loads (200, uncompressed by asking the app
   *  for identity) get `<script src="/__roll/vitals.js" defer>` before `</body>` (the page's
   *  caching stays the app's; its ETag made weak), which measures LCP, INP, CLS, FCP and TTFB in
   *  the browser and sends one beacon to `POST /__roll/vitals` on the same host when the page is
   *  hidden (at most 2 KiB; its path kept as a route, bounded numbers; 600 a minute an
   *  environment, then 429). No cookie, no id; the edge's country is kept. Off: those paths are
   *  the app's. */
  vitals?: boolean;
  log_retention_days?: 7 | 30 | 90 | null; // how long its logs are kept (null: 7); a preview's stay 7 (400)
  scheduled?: boolean; // false: its schedules don't run (a cron service's, run.schedules' commands and calls), skipped and not made up when turned back on; deploys and POST …/runs still go; default true (roll previews turns pv- environments' off)
  data?: "branch" | null; // admin: data of its own, a branch of each Postgres and Redis database it links (made now and at each deploy for links made since; an environment's services share one), its link variables the branch's, never the database's; null goes back to them, deleting the branches no other environment uses; 400 for production and previews (roll previews branches theirs)
  closed_by?: string | null; // with egress, the operator's only (roll previews in process, or any caller on an open daemon; a token needs admin; 403 for a user's token or a signed-in person): who a closed environment's placeholders are made by (usr_… or token:…); null, no one (its keys bound to hosts withheld); unset, whoever set the egress
  badge?: boolean; // a deploy status badge at GET /_roll/badge/<service>/<env>.svg on the apps listener, answered to anyone (Netlify's): live, deploying, waiting, failed, paused or not deployed; false: 404
  approvals?: 0 | 1 | 2 | 3 | null; // admin (as freeze): people other than a deployment's author (the person behind it: their session, their own token, the operator) who approve it (`…/approve`) before it goes live; a waiting build doesn't run (no instance, its own URL 409) until then: deploys, redeploys, promotions and automation's build to ready and wait (a service that can't wait beside the live one: 409, or skipped), putting one live without them is a 409 unless an admin's `X-Roll-Override: 1` (audited `approvals.bypassed`); rolling back to what was live needs none; never a preview's (400)
  report?: boolean; // false: its deploys post no commit status and no GitHub Deployment (someone else reports them: roll previews, for its `pv-` environments)
  preview_host?: string | null; // the one DNS label it answers to under its project's preview domain (`<label>.preview.acme.dev`; roll previews sets it), unique in the project (409); null: none
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
  /** Paused (Render's Suspend): its instances stop, and nothing runs, wakes (a request gets roll's
   *  503 "Paused" page), deploys (409), is scheduled (a cron run is skipped, not made up) or takes
   *  a job (409) until `false`. A push to its branch is logged, not deployed. */
  paused?: boolean;
  /** `true`: a person holds its deploys (Netlify's locked deploys): pushes, CI waits, deploy hooks
   *  and tag moves build to `ready` (`no_traffic`; a web service without a volume) for them to
   *  put live with `POST …/deploys/{dep}/rollback`, which keeps the hold; a service that can't
   *  run beside the live one isn't deployed (a hook's is a 409). Deploying by hand works as
   *  ever and doesn't end it. `false`: deploys resume, from this hold or a rollback's. */
  held?: boolean;
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
   *  `//` and dot segments resolved); `methods` any of; `user_agent` contained, case aside;
   *  `ips` the address the fleet vouches for in any of them (at most 100, kept canonical);
   *  `header` (any case), `cookie` or `query` (as written) there, holding `contains` (case
   *  aside) when it says. Checked after allow_ips, before the rate limit and any wake; HTTP
   *  only. null: none. */
  firewall?: { action: "allow" | "deny" | "limit" | "challenge"; path?: string; methods?: string[]; user_agent?: string; ips?: string[]; header?: FieldMatch; cookie?: FieldMatch; query?: FieldMatch; per_minute?: number /* limit's: 1–100000 a client */; burst?: number /* limit's (default a minute's worth) */ }[] | null;
  headers?: { path?: string; set: Record<string, string>; replace?: boolean }[] | null; // at most 20; the app's own header wins unless replace; no framing or x-roll-* names
  routes?: { path: string; service: string }[] | null; // at most 20: a path (a prefix ends in *) another service of the project answers, by name (not this one; 400 for one the project hasn't), its environment of the same name, the path and query as they came; the first that takes a path decides; checked after this environment's gates, past the target's
  checks?: { name: string /* a-z, 0-9, -, unique, ≤32 */; path: string; every_s?: number /* 30–3600, default 60 */; timeout_s?: number /* 1–60, default 10 */; status?: number /* the one it expects; default any 2xx/3xx */; contains?: string /* ≤200, its answer holds it */ }[] | null; // uptime checks, at most 5: each GET'd through the environment's own proxy on its owner (past its gates, as a schedule's call); two failures in a row are check.down, the next pass alert.resolved; a check wakes a scale-to-zero service; null: none
  pause_schedule?: { windows: { from: string /* "fri 19:00" */; to: string }[]; timezone?: string } | null; // paused by the clock: as a window starts the environment is paused (as PUT {paused: true}), as it ends it's resumed, only what the schedule paused; a person's pause or resume inside a window holds until it ends; null: none
  mirror?: { environment: string; percent: number /* 1–100 */ } | null; // traffic mirroring: a copy of that share of GET and HEAD requests to another environment of this service, past its gates
  config?: { [key: string]: unknown } | null; // runtime config: at most 200 keys (letters, digits, _ . -), 64 KiB; its instances read it at ROLL_CONFIG_URL without a deploy (the first config needs one deploy, to give them the address); null: none
  snippets?: { at: "head" | "body_end"; html: string /* 1 byte to 16 KiB */ }[] | null; // at most 5: its own HTML before </head> or </body> of every 200 HTML page the proxy answers (an analytics or chat tag); null or []: none, its answer dropped (30 s at most), in that environment's own access log and metrics; never an upgrade, roll's own scheduled call or a request another environment sent; null ends it
  freeze?: { until?: number /* unix ms */; windows?: { from: string /* "fri 17:00" */; to: string }[] /* weekly, at most 14 */; timezone?: string /* IANA, default UTC */; reason?: string /* ≤200 */ } | null; // a deploy freeze: on until lifted with neither until nor windows; while on, pushes, tags, hooks and the Git watch build to ready (a person's hold), a person's deploy, redeploy or promotion is a 409 unless its body says the header X-Roll-Override: 1, which is admin (audit.freeze.overridden), and a rollback is never frozen; null lifts it
  split?: { environment: string /* another environment of this service */; percent: number /* 1–99 */ } | null; // a split test (Netlify's): that share of visitors, by their roll_bucket cookie (the top buckets; a gradual rollout takes the bottom ones), served by the other environment past its gates; never roll's own scheduled calls or a deployment's URL; null ends it
  heartbeats?: { name: string /* a-z, 0-9, -, unique, ≤32 */; every_s: number /* 60–604800 */; grace_s?: number /* 0–86400, default 300 */ }[] | null; // at most 10: something outside roll pings `/__roll/heartbeat/<token>` (any method) on the environment's host at least every every_s; one kept by name keeps its token; a new one's URL is in this answer's heartbeat_urls, once (only its SHA-256 is kept); one not pinged within every_s and grace_s is heartbeat.missed, the next ping alert.resolved; null: none
  /** Its own size and scale over the service's (min_instances, max_instances, cpu, memory_mb,
   *  concurrency, suspend_after_s, sleep_after_s; any other field is a 400). Each replaces the
   *  service's, null goes back to it; checked with the service's settings as one run. */
  run?: { [field: string]: number | null };
};

type Environment = EnvironmentSummary & {
  service: string;
  branch: string | null;     // deploys on every landing on this branch (repo sources)
  instances: Instance[];
  domains: Domain[];
  port: number | null;       // locally, the service's own port (ROLL_APPS_PORTS)
  private: boolean;          // its hostnames answer only visitors with its access token, or who signed in
  signed_in?: true;          // people signed in to roll who can open its project get in (access.signed_in)
  password?: true;           // visitors may type its password (access.password; never returned)
  next_run_at: number | null; // a cron service's next scheduled run, once deployed
  volume_used?: { bytes: number; at: number }; // how full its volume was when last measured (hourly, on its owner), when it has one
  checks?: Check[];         // its uptime checks (as the PUT's)
  check_states?: { name: string; up: boolean | null /* null: not tried yet; false once down */; failures: number; last_at: number | null; status: number | null; ms: number | null; error: string | null; down_since: number | null }[]; // on its owner
  heartbeats?: { name: string; every_s: number; grace_s: number; added_at: number }[]; // as the PUT's (never a token)
  heartbeat_states?: { name: string; last_ping_at: number | null; due_at: number /* before the grace */; missed: boolean }[];
  heartbeat_urls?: Record<string, string>; // only in the answer to the PUT that added them: each new heartbeat's ping URL, shown once (a credential: left out for a caller that may not reveal)
  split?: { environment: string; percent: number }; // its split test, as the PUT's
  freeze?: Freeze; frozen?: true;                    // its deploy freeze, as the PUT's, and whether it's on now
  mirror?: { environment: string; percent: number }; // its traffic mirroring, as the PUT's
  snippets?: { at: "head" | "body_end"; html: string }[]; // its snippets, as the PUT's
  config?: { [key: string]: unknown }; config_version?: number; // its runtime config and version
  config_rotated_at?: number; // when its config's token was last rotated: the address before it reads until a deploy made after it is live
  pause_schedule?: { windows: { from: string; to: string }[]; timezone?: string }; // as the PUT's
  schedules?: { name: string; next_run_at: number | null; last: { job: string; status: RunStatus; started_at: number } | null; last_call?: { at: number; status?: number; ms: number; error?: string } /* a path schedule's last call */ }[]; // run.schedules as they stand: next due (none paused, in a preview or undeployed) and the job each last started
  crash_loop: { crashes: number; last_at: number; last_error: string } | null; // 3+ crashes in 10 minutes
  canary: { deployment: string; share: number; waiting?: true } | null; // a gradual rollout under way: the percent of requests the new deployment takes; waiting: a manual rollout's step passed and it waits for a person
  run: { [field: string]: number }; // its own run settings over the service's ({}: the service's)
  hook: boolean;              // it has a deploy hook
  follow_tag: boolean;        // an image service's tag is watched: a new digest behind it deploys
  redeploy_on_secrets: boolean; // the project's secrets changing redeploys it (same image, new values)
  traces: boolean; // its instances send OpenTelemetry traces to roll
  vitals: boolean; // its pages measure their Web Vitals for roll (`…/vitals`)
  approvals?: number; // approvals a deployment needs before it goes live (absent: none)
  badge_url?: string; // its deploy status badge, when `badge` is on
  log_retention_days: number; // 7 (default), 30 or 90: its logs (output, access log, traces, metrics, events, Vitals) are kept that long, then deleted hourly by the owner; a preview's are 7
  report: boolean; // its deploys are reported to the Git host (default true)
  preview_host?: string; // its label under the project's preview domain
  egress: { mode: "roll" | "open"; hosts: string[] }; // what its code may reach (roll: roll and the hosts)
  egress_refused?: { host: string; private?: true }[]; // hosts its egress proxy refused lately, oldest first, at most 20: not listed, or (private) listed but resolving only to private addresses
  scheduled?: false; // its schedules don't run (absent: they do)
  data?: { mode: "branch"; branches: { kind: "postgres" | "redis"; ref: string }[] }; // data of its own: the branches made for it (never their credentials); absent: its links reach the databases themselves
  closed_by?: string | null; // a closed environment's placeholders' maker (usr_…, token:…, operator); null: no one (its keys bound to hosts withheld); absent when open or closed before it was kept
  intercepted?: { requests: number; swapped: number; refused?: string[] }; // a closed environment's interception on its owner (roll secrets' placeholders) since that node started: requests filtered, placeholders swapped, the latest refusals' reasons (at most 20; never a value or a placeholder)
  feedback: boolean;          // a preview's pages carry roll previews' feedback button
  maintenance: { message: string | null } | null; // in maintenance (null message: roll's words); null: serving
  paused?: true;              // paused (absent: not)
  held?: true;                // a person holds its deploys (`PUT … {held: true}`): automation builds to ready, a person puts it live
  held_after?: string;        // deploys held after this rollback: pushes, CI waits, deploy hooks (409) and tag moves aren't put live until a person deploys, redeploys or promotes, or PUTs held: false; a secrets redeploy (the live image again) still runs
  pages?: { error?: string; maintenance?: string }; // its own pages (absent: roll's): error for the 502/503/504 while it can't answer or wake, maintenance while in maintenance
  allow_ips: { cidr: string; description?: string }[]; // only these networks get in through its public names ([]: anyone)
  tcp: string | null; // a TCP service's address, host:port (null until it has its port)
  udp?: string; // a UDP service's address, host:port (absent until it has its port)
  rate_limit: { per_minute: number; burst: number | null } | null; // each client's requests a minute (null: no limit)
  firewall: { action: "allow" | "deny" | "limit" | "challenge"; path?: string; methods?: string[]; user_agent?: string; per_minute?: number; burst?: number }[]; // its rules, in order
  headers?: { path?: string; set: Record<string, string>; replace?: boolean }[]; // headers its answers carry (names lower case)
  routes?: { path: string; service: string }[]; // paths another service of the project answers
  scaling: { backlog: number | null; wanted: number } | null; // a worker following its work: the backlog last read (every 10 s) and the instances it wants
  wait_for_ci: boolean;
  waiting_for_ci: string | null; // the push (sha) waiting for its checks
  build_closed?: true;      // egress roll: its builds get no secret values (a build has the default network)
  base_updated?: string[];  // base images the live deployment was built on that have moved since in their registries
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

type NewDeploy = { revision?: string /* a commit, or a branch or tag by name (deployed once: the environment keeps following its branch); a repo service's: a full or short sha or a branch */; image?: string; message?: string; no_traffic?: boolean; no_cache?: boolean /* build from scratch (400 with an image): no cached layer, base images pulled again, the service's cache mounts empty from then on; an upload says it with `X-Roll-No-Cache: 1` */; /* during a freeze, the header X-Roll-Override: 1 (admin; audited): a body `override` is a 400 */ closed?: boolean /* true: 409 unless the environment is built closed (egress `roll` on a node that closes it: its own network, no secret value); a node that predates the field answers 400, so the request fails closed (roll previews sends it for a fork's head) */ };

type Deployment = {
  id: string;
  service: string;
  environment: string;
  status: DeployStatus;
  trigger: "push" | "manual" | "rollback" | "upload" | "api" | "settings" | "promote"; // settings: a redeploy
  approved_by?: string[];   // who approved putting it live, where its environment asks for approvals (a single deployment's GET)
  url: string | null;        // its own URL (`<id>--<env slug>` under the apps domain, the environment's scheme, port and access rules; never a project's preview domain): null for a worker, a service with a volume, one not public over HTTP, one never built, removed, pruned or waiting for its approvals
  actor: string | null;     // who asked: "git:<name>" for a push (its commit's author, as the commit claims it), the calling token or connection ("operator" for the operator's token); null when roll did it on its own
  revision: string | null;   // git commit of a repo source
  message: string;
  image: string | null;      // the built or pinned image, by digest once known
  plan: Plan | null;         // what the build detected
  rollback_of: string | null;
  error_detail?: string;     // what the app or the build printed when it failed (the step, its last line): the owner's, never in a commit status, an event or a notification
  fix?: string;             // how to fix a failure roll knows (the setting and its command), read from the error
  pruned_at?: number;        // pruned (older than the environment keeps): history only, its image gone; a rollback to it is a 409
  vulnerabilities?: ScanSummary; // its image's last scan for known vulnerabilities, once one ran (a builder that scans: one node's Docker)
  steps?: { step: "build" | "pre_deploy" | "rollout" | "live" | "failed"; at: number }[]; // when each began: on GET …/deploys/{dep} only
  error: string | null;
  created_at: number;
  started_at: number | null;
  finished_at: number | null;
  /** Milliseconds from the deploy's start to its first healthy answer. */
  ready_ms: number | null;
};
type SloIndicator = { total: number /* requests, or minutes with requests */; bad: number; sli: number | null /* percent that met it */; budget_left: number | null };
type SloReport = { slo: { objective: number; latency_ms?: number; window_days: number } | null; availability?: SloIndicator; latency?: SloIndicator | null; burning?: { indicator: "availability" | "latency"; burn_rate: number; window: "1h" | "6h" } | null };
type Severity = "critical" | "high" | "medium" | "low" | "unknown";
type PushState = { /* the rule's own fields, as in run.push */ stream: string; topic: string; group: string; path: string; environment?: string; dead_letter?: string; max_attempts?: number; timeout_s?: number; max_in_flight?: number; auth?: "oidc"; lag?: number; off?: string; delivered: number; dead_lettered: number; last_delivered_at?: number; failing_since?: number; last_error?: string };
type ProjectUsage = { month: string; total_usd: number; projected_usd: number | null /* this month only */; rows: { service: string | null; name: string | null; environment: string | null; previews?: true; deleted?: true; usd: number; cpu_seconds: number; gb_seconds: number; requests: number; egress_bytes: number; build_seconds: number }[] };
type Explanation = { summary: string /* one sentence */; cause: string; steps: string[] /* what to change, in order, at most 5 */; confidence: "high" | "medium" | "low"; model: string; at: number; lines_left_out: number /* older log lines it didn't see */ } /* the model's words: shown as text */
type ScanSummary = { sbom?: true /* its SBOM is kept */; critical: number; high: number; medium: number; low: number; unknown: number; fixable: number /* with a fixed version */; scanned_at: number; scanner: string /* "trivy 0.76.1" */; error?: string /* the scan didn't finish: the counts mean nothing */ };
type Vulnerability = { id: string /* CVE-…, GHSA-…, a distribution's */; severity: Severity; package: string; installed: string; fixed?: string; title?: string; target: string /* the OS's packages ("debian 12.15") or a lockfile's path */ };


/** A project in one file: a root roll.toml's `[apps]` and `[databases]`, or a Compose file as them. */
type ProjectImport = {
  roll_toml: string;          // the file as roll.toml (a Compose file's, to keep)
  notes: string[];            // what a Compose file's import left out, and why
  apps: { [name: string]: {
    source: { kind: "repo"; repo: string; root: string | null } | { kind: "git"; url: string; root: string | null } | { kind: "image"; image: string } | { kind: "upload"; root: string };
    branch: string | null;    // a repo's production branch
    run: Patch<Run>;          // the run settings the entry names
    links: { kind: "postgres" | "redis" | "bucket" | "stream" | "nosql" | "service"; name: string }[]; // databases below by name, or another app of the file (`service`)
    environments: { [env: string]: { branch: string | null; run: object /* an environment's own run settings */; variables: { [name: string]: string } /* set when it's made */ } }; // besides production
    variables: { [name: string]: string }; // set when the service is made, never over what it has
    generate: string[];       // `{ generate = "secret" }`: 32 random bytes in hex, made by whoever makes the service, once
    ask: { name: string; ask: string; default: string | null }[]; // `{ ask = "…" }`: asked of whoever makes it
  } };
  databases: { [name: string]: { kind: "postgres" | "redis" | "bucket" | "stream" | "nosql" } };
};

type PlanRequest = { repo: string; revision?: string; root?: string } | { git: string /* a Git URL, fetched at its branch's head as its deploy would be */; branch?: string; root?: string; git_login?: { username: string; password: string } /* a private repository's */ } | { files: Record<string, string> };

type Plan = {
  provider: "dockerfile" | "deno" | "node" | "python" | "go" | "rust" | "ruby" | "php" | "java" | "dotnet" | "elixir" | "static" | "functions";
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
  functions?: string[]; // a static site's function directories the functions builder runs (netlify/functions, api; "edge" with edge functions)
  edge?: { name: string; kind: "netlify" | "vercel"; file: string; paths: string[]; excluded?: string[] }[]; // its edge functions, run before its files on their paths by its <site>-fn service, on Deno
  /** The Procfile's `release:` (Heroku's release phase): the deployment's pre-deploy command when
   *  the service sets none. Absent without one, and for a tree's own Dockerfile. A Procfile's `web:`
   *  (`worker:` for a worker) is `start` when the service names none. */
  release?: string;
  /** roll's Next.js adapter is in the image (a Next 16 server, not its own Dockerfile): its
   *  instances share ISR, fetch and "use cache" entries (README, "Next.js across instances"). */
  next_cache?: boolean;
};

/** Wired on a node today: postgres (DATABASE_URL), redis (REDIS_URL, REDIS_REST_URL/TOKEN) and search. */
type LinkKind = "postgres" | "redis" | "bucket" | "stream" | "queue" | "nosql" | "search" | "service";
type Link = { kind: LinkKind; ref: string; prefix: string; vars: string[]; indices?: string[] /* a search link's key's indices */; access?: "read" | "append" };
// `search`: `ref` is a roll search cluster (id or name). The link makes a data-only credential of
// its own, once (never the cluster's password): ELASTICSEARCH_URL and ELASTICSEARCH_API_KEY, or on
// an OpenSearch-flavor cluster OPENSEARCH_URL, OPENSEARCH_USERNAME and OPENSEARCH_PASSWORD (an
// internal user of its own). Kept
// sealed with the link; unlinking (or deleting the service) revokes it. A scoped token needs
// `reveal` on the cluster.
// `service`: `ref` is another web service of the project (its id); `URL` is
// `http://<name>.<env>.internal:<port>` in the linking environment, `PUBLIC_URL` its public URL
// when it has one there. The prefix defaults to its name (`API_`). A service's links go with it.

type Variables = { vars: { name: string; masked: string; source: "service" | "secret" | "link" | "roll" | "reference"; reference?: string /* a reference variable's template, `${{ api.KEY }}` */; sealed?: true /* never shown again: masked "••••••••" whatever its length */; file?: string /* kept as a file at this path in its instances */; runtime_only?: true /* only its instances get it, never a build's steps */ }[] };
// A value set with `${{ VAR }}` / `${{ service.VAR }}` is a reference, filled in at each deploy;
// setting one that names another service needs `reveal` on it (403), and one naming no service of
// the project is a 400.

type LogLine = { t: number; stream: "stdout" | "stderr" | "build" | "system"; instance: string | null; deployment: string | null; text: string; level?: "trace" | "debug" | "info" | "warn" | "error" | "fatal" /* what the app's own line says (JSON's level/severity/lvl/log.level, a name or pino's number; logfmt's level=; an upper-case level word among its first three); absent when it says none */; trace?: "start" | "more" /* part of a stack trace (the errors' joining, as the line is written): start on its first line, more on the rest from the same instance and stream; absent otherwise */ };

type RouteStat = { method: string; route: string; requests: number; errors: number /* 5xx */; client_errors: number /* 4xx */; p50_ms: number; p95_ms: number; max_ms: number };
/** A page view: a browser's page load (`Sec-Fetch-Dest: document`, else `Accept: text/html`) answered 2xx or 304. A visitor: an address and user agent on one day, counted for this answer only (never kept). */
/** A trace's root span, its duration and its spans' count. */
type TraceSummary = { trace: string; name: string; service: string; start_ms: number; ms: number; spans: number; error: boolean };
/** A span (OTLP's, as kept): ids in hex, times in Unix nanoseconds, its string/number/bool attributes. */
type Span = { trace: string; span: string; parent?: string; name: string; service?: string; start: number; end: number; error?: boolean; attrs?: [string, string][] };
type Analytics = { minutes: number; step: number /* minutes a point of series stands for */; page_views: number; visitors: number;
  pages: { path: string; views: number; visitors: number }[]; referrers: { host: string /* the referring site's, never this one's */; views: number }[];
  countries: { country: string /* the edge's CF-IPCountry */; views: number }[];
  campaigns: { source: string; medium: string; campaign: string /* a page view's utm_source, utm_medium, utm_campaign, lowercased, "" where not given */; views: number; visitors: number }[];
  devices: { browsers: { browser: string; views: number }[]; os: { os: string; views: number }[]; classes: { class: "mobile" | "tablet" | "desktop" | "bot"; views: number }[] } /* from the user agent */;
  events: { name: string; count: number; props: { [name: string]: { value: string; count: number }[] /* its 5 most common */ } }[] /* POST /__roll/event {name, props?, path?} on the environment's host (or /__roll/track.js's roll.track), the 20 most counted */;
  series: { t: number; views: number; visitors: number }[]; cut: boolean /* only the newest 200,000 requests (and events) read */ };
type RouteStats = { minutes: number; requests: number; cut: boolean /* only the newest 50,000 requests read */; routes: RouteStat[] };

type Metrics = {
  step: number;              // minutes a point stands for: 1 up to a week (minutes=10080), 60 past it (up to 90 days: hourly rollups, counts summed, the rest their peak)
  minutes: number[];         // unix minute of each point, oldest first
  requests: number[]; errors: number[]; // 5xx
  p50_ms: number[]; p95_ms: number[]; p99_ms: number[];
  cpu: number[];             // cores used, average over the minute
  memory_bytes: number[];    // resident, max over the minute
  instances: number[];
  cold_starts: number[];
  cold_start_ms: number[];   // average of the minute's cold starts (0 when none)
  egress_bytes: number[];    // bytes answered (what egress is billed by); format=prometheus: roll_apps_egress_bytes_per_minute
  over_capacity: number[];   // requests that found every instance busy at max_instances (saturation); roll_apps_over_capacity_per_minute
  by_instance: {             // each instance it has now, its own last hour whatever `minutes` asks (on its owner, in memory)
    instance: string; deployment: string;
    minutes: number[]; cpu: number[] /* cores, average over the minute */; memory_bytes: number[] /* the minute's peak */;
  }[];
};

type Usage = {
  days: { day: number; cpu_seconds: number; gb_seconds: number; requests: number; egress_bytes: number; build_seconds: number; usd: number }[];
  total_usd: number;
};

/** In the event log, each written in the same transaction as the change it records. Sequence
 *  numbers commit in order with no gaps, so a reader moving past `next` never misses one. Kept 7 days. */
type Event = { seq: number; at: number; kind: "deploy.active" | "deploy.failed" | "deploy.base_updated" /* data.image, was, now: a base image the live deployment was built on moved in its registry, once per new digest */ | "rollout.waiting" /* data.share: a manual rollout's step passed and it waits for a person (notified by default) */ | "deploy.ready" /* data.revision, url: built and healthy beside the live one (no_traffic, or a hold), waiting for a person to put it live (notified by default) */ | "crash_loop" | "spend.alert" | "spend.limit" | "alert.memory" | "alert.cpu" /* data.value, threshold, cpu: its instances' share of their cpu for five minutes (notified by default) */ | "alert.metric" /* data.metric, value, above or below, minutes: one of its own gauges past its threshold that many minutes running (notified by default) */ | "alert.errors" | "alert.latency" | "alert.capacity" /* data.value, threshold, requests, max_instances, concurrency: that share of five minutes' requests found every instance busy at max_instances (notified by default) */ | "alert.log" | "error.regressed" /* data.fingerprint, kind, name: an error marked resolved seen again (notified by default) */ | "error.new" /* data.fingerprint, kind, name: an error (a stack trace or an error line) the environment's log has never had, once ever; its type, never its message (notified by default) */ | "cron.failed" | "job.failed" | "domain.active" | "domain.expiring" | "domain.expired" | "domain.tls_failed" | "volume.backup_failed" /* data.error: the daily backup failed (notified by default) */ | "volume.full" /* data.used_bytes, size_gb, pct: past 90% of its size, once until it's under 80% (notified by default) */ | "form.submitted" /* data.form, submission, fields: a static site's form kept a submission; its fields' names, never their values */ | "check.down" /* data.check, path, http_status, error: an uptime check failed twice in a row, once until it passes (notified by default) */ | "heartbeat.missed" /* data.heartbeat, last_ping_at, every_s: no ping within its period and grace, once until the next (notified by default) */ | "alert.slo" /* data.indicator (availability or latency), burn_rate, window (1h or 6h), objective: its error budget burning fast (notified by default) */ | "alert.anomaly" /* data.kind (traffic, errors, latency), value, usual: the last hour against the median of the same hour of the week over the last four weeks (two at least), with `alerts.anomaly` on (notified by default) */ | "deploy.vulnerable" /* data.critical, high, new, ids (up to 5): a scan found advisories at high or worse that the deployment's last scan (else the environment's one before) didn't have */ | "deploy.skipped" /* data.revision, tag: a push whose message said [skip ci] (the timeline only) */ | "alert.resolved" /* data.alert: which alert cleared (data.metric too for alert.metric); sent to targets that get that alert */ | `audit.${string}` /* changes people made: what /activity lists */; service: string; environment: string; deployment: string | null; data: object };
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
type LogDrain = {
  id: string; project: string; host: string; created_at: number; secret?: string;
  sources: ("runtime" | "http" | "build" | "audit" | "traces" | "metrics")[]; // traces: the spans its instances send; metrics: the points they send or roll scrapes (an otlp drain sends them to …/v1/traces and …/v1/metrics beside its …/v1/logs, as OTLP JSON); // runtime: the instances' output and roll's lines about them; http: the access log, a request a line (text: an AccessLine); audit: each audit entry as it's written (stream "system", text JSON {event: "audit.<what>", data: {actor, …}}; a service's own, such as audit.deleted, has environment ""); build: each deploy's build
  environments: string[];     // only these environments' (empty: every one)
  services: string[];         // only these services' (ids; empty: every one)
  previews: boolean;          // preview environments' too (a drain made before this field: true), and a preview's services on roll's own runtime (its `pv-` environment, `service` null: never to a drain narrowed to `services`)
  headers: string[];          // the names of the headers it's sent with (values sealed, never returned)
  format: "ndjson" | "otlp" | "syslog" | "s3" | "search"; // search: ECS documents in a roll search cluster's data stream (made with `search`); ndjson: signed batches of lines; otlp: an OTLP/HTTP JSON ExportLogsServiceRequest to the URL (a collector's /v1/logs); syslog: RFC 5424 over TLS (RFC 5425's octet counting), no headers; s3: an NDJSON object a batch (≤5,000 lines or a minute) at <prefix>/YYYY/MM/DD/HH/<time>-<delivery>.ndjson, PUT signed with SigV4 by its sealed keys
};
// A batch's line: { t, source, service, service_name, environment, instance, deployment, stream, text }.
/** A request through the proxy (`stream: "http"` lines' text, JSON). The path has no query string. */
type AccessLine = {
  method: string; path: string; status: number; ms: number; bytes: number;
  ip: string | null; ua: string; host: string;      // who asked: the address the fleet vouches for, the user agent, the host
  // An instance's answer:
  instance?: string; request_id?: string; cold?: boolean; waited_ms?: number;
  // Or one roll gave itself, before any instance, and why (metered: a failed wake counts as an error):
  by?: "allow_list" | "firewall" | "rate_limit" | "private" | "maintenance" | "wake" | "capped" | "paused" | "not_deployed" | "moving" | "unavailable";
};
/** A volume's backup: a tar in object storage (`apps/volumes/<service>/<env>/<id>.tar`). */
type Backup = { id: string; at: number; size: number };
type NotifyTarget = { id: string; project: string; kind: "webhook" | "slack" | "discord" | "teams" /* a Workflows (…logic.azure.com, …powerplatform.com) or …webhook.office.com URL: an Adaptive Card */ | "googlechat" /* https://chat.googleapis.com/v1/spaces/…: text, no mentions */ | "email" | "pagerduty" | "opsgenie"; host: string /* an email target's: "the project's owner" */; events: ("deploy.failed" | "deploy.base_updated" | "rollout.waiting" | "deploy.ready" | "crash_loop" | "deploy.active" | "spend.alert" | "spend.limit" | "alert.memory" | "alert.cpu" | "alert.metric" | "alert.errors" | "alert.latency" | "alert.capacity" | "alert.log" | "error.new" | "error.regressed" | "cron.failed" | "job.failed" | "domain.active" | "domain.expiring" | "domain.expired" | "domain.tls_failed" | "volume.backup_failed" | "volume.full" | "form.submitted" | "check.down" | "heartbeat.missed" | "deploy.vulnerable" | "alert.slo" | "alert.anomaly" | "audit" /* every audit.* entry, as the event log has it (never on pagerduty or opsgenie) */)[];
  environments: string[];  // only these environments' events; empty: every one's
  services: string[];      // only these services' (ids); empty: every one's
  previews: boolean;       // preview (ephemeral) environments' events too (an environment named in `environments` always)
  created_at: number; secret?: string };
type Delivery = { id: string; event: number; kind: string; status: "pending" | "delivered" | "failed"; attempts: number; code: number | null; error: string | null; at: number };
type RunStatus = "running" | "succeeded" | "failed" | "skipped" | "timed_out" | "canceled";
type CronRun = { id: string; deployment: string; status: RunStatus; exit_code: number | null; started_at: number; finished_at: number | null; attempt: number /* which try of its due time: 1, then up to run.retries more after a failure */ };
/** A one-off command in a fresh container of the active deployment: its variables and project
 *  network, no port, no volume. timeout_s 1–86400 (default 3600). */
type PrivateNetwork = { kind: "wireguard" | "tailscale"; secret: string /* roll://ENV/KEY, never the config or key */; routes: { name: string; target: string }[]; login_server: string | null; status: string /* running, failed: why, waiting for a deploy */ };
type Sandbox = { id: string /* sbx_… */; service: string; environment: string; image: string; cpu: number; memory_mb: number; variables: boolean; network: "none" | "environment"; created_at: number; expires_at: number };
type Job = { id: string; deployment: string; command: string; tasks: number; retries?: number /* tries after a task's failure */; parallelism: number /* tasks at once */; status: RunStatus; exit_code: number | null; started_at: number; finished_at: number | null };

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

A 200 whose app lets a shared cache keep it (`CDN-Cache-Control: max-age=N`, else
`Cache-Control: s-maxage=N`, with `stale-while-revalidate=M` honoured; never with `private`,
`no-store`, `no-cache`, `Vary: *`, `Set-Cookie`, a body over 1 MiB or of unknown length) answers
the environment's next GET or HEAD of the same path and query (and the headers it `Vary`s on)
from the owner's cache, before anything wakes: `X-Roll-Cache: HIT`, `STALE` (fetched again in the
background), or `MISS` when it was just kept, with `Age`. A request with `Authorization`, a
deployment's own URL, a gradual rollout and a preview's feedback page never use it. A new live
deployment empties it, and so does `POST …/cache/purge` (all of it, some paths, or some tags).
An answer's tags are its `Cache-Tag`, `Netlify-Cache-Tag` and `Vercel-Cache-Tag` (comma-separated)
and `Surrogate-Key` (space-separated), at most 128, matched in any case; those headers never reach
a visitor, kept or not.

`POST /_roll/awake/{id}/{env}/{token}?instance=&for=` on the apps listener (a web service's
instances' `ROLL_AWAKE_URL`, sealed; the environment's `rlr_` instance token): the instance named
isn't suspended, put to sleep or scaled in for `for` seconds (1–900, default 60; a longer hold
stays), on the service's owner. `{held_until}` (unix ms); a wrong token or an instance that isn't
awake, 404; a missing instance or another `for`, 400.

`POST /_roll/otlp/{id}/{env}/{token}/v1/{traces|logs|metrics}` on the apps listener (any host;
not the API) takes an environment's traces, log records and metrics from its instances: OTLP/HTTP
`ExportTraceServiceRequest`, `ExportLogsServiceRequest` or `ExportMetricsServiceRequest`, protobuf
(`application/x-protobuf`) or JSON (`application/json`), gzip or not, at most 8 MiB; 200 with an
empty export response. The token in the path is the credential (`traces: true` sets a sealed
`OTEL_EXPORTER_OTLP_ENDPOINT` to it); any other is a 404.
