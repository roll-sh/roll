/**
 * Client for roll apps (`crates/apps-control`, `/v1/apps`): the contract is docs/apps/http-api.md,
 * and the types below are its "Shapes". Change both together.
 */

import { operatorHeaders } from "./code-token";
import { apiHeaders, call, onApiStatus, ROLL_API } from "./roll-api";
import { streamSse } from "./sse";

// ── shapes (docs/apps/http-api.md) ──

export interface Prices {
  cpu_second: number;
  gb_second: number;
  egress_gb: number;
  build_minute: number;
}

export interface Overview {
  services: Service[];
  runtime: "docker" | "fargate";
  /** Where generated URLs live: `up.roll.site`, `apps.localhost`. */
  domain: string;
  prices: Prices;
}

export type Source =
  | { kind: "repo"; repo: string; root?: string }
  | { kind: "image"; image: string }
  | { kind: "upload" };

export interface Build {
  builder: "auto" | "dockerfile";
  dockerfile?: string;
  install?: string;
  build?: string;
  start?: string;
  /** A push deploys only when it changes a path these match (gitignore-style, from the repo's root). */
  watch?: string[];
  /** A single-page app (a page path with no file answers index.html); unset: detected. */
  spa?: boolean;
  /** The Dockerfile's stage to build (a multi-stage one whose last stage isn't what runs). */
  target?: string;
}

export type ServiceType = "web" | "worker" | "cron";

export interface Run {
  type: ServiceType;
  port?: number;
  start?: string;
  /** Once per deploy before the switch (a migration); a non-zero exit fails the deploy. */
  pre_deploy?: string;
  schedule?: string;
  /** The zone a cron schedule's fields are in (IANA); UTC when unset. */
  timezone?: string;
  timeout_s?: number;
  min_instances: number;
  max_instances: number;
  concurrency: number;
  cpu: number;
  memory_mb: number;
  health_path?: string;
  health_timeout_s: number;
  /** A replaced instance's time to finish: its requests, then SIGTERM, SIGKILL when it's up (1–300; unset 30). */
  drain_s?: number;
  /** A web service also scales on CPU: another instance while they average over this percent for a minute (10–100). */
  cpu_target?: number;
  suspend_after_s: number;
  sleep_after_s: number;
  volume?: { mount: string; size_gb: number };
  /** Metric alerts (`alert.*` events), over five minutes; each 0 is off. */
  /** log_match: text its output holds log_count times in five minutes (alert.log). */
  alerts?: { memory_pct: number; error_pct: number; p95_ms: number; log_match?: string; log_count?: number };
  /** A gradual rollout: the new deployment takes each percent of `steps` for `step_s`, rolled
   *  back by a 5xx share of `error_pct` or more (worse than the old one); no steps: at once. */
  rollout?: { steps: number[]; step_s: number; error_pct: number };
  /** The app speaks HTTP/2 without TLS (a gRPC server). */
  h2c?: boolean;
  /** false: no public URL, only the project's private network (absent: public). */
  public?: boolean;
  /** A worker that stops: started again always (default), on failure (up to restart_tries in a row), or never. */
  restart?: "always" | "on_failure" | "never";
  restart_tries?: number;
  /** A web service keeps each visitor on one instance (a cookie) while it's up. */
  affinity?: boolean;
  /** Its port speaks TCP, not HTTP: each environment answers on a port of its own. */
  tcp?: boolean;
  /** A deployment that crash-loops within ten minutes of going live is replaced by the one before it. */
  rollback_on_crash?: boolean;
  /** A worker follows `group`'s lag on `stream` (a roll stream of its project): one instance per `per_instance` behind. */
  scale?: { stream: string; group: string; per_instance?: number };
}

export type InstanceState = "none" | "stopped" | "suspended" | "waking" | "running" | "crashed" | "deploying";

export type DeployStatus = "queued" | "building" | "deploying" | "active" | "inactive" | "failed" | "canceled" | "superseded" | "removed" | "ready";

export interface EnvironmentSummary {
  name: string;
  state: InstanceState;
  url: string | null;
  active: { id: string; revision: string | null; created_at: number } | null;
  latest: { id: string; status: DeployStatus } | null;
}

export interface Service {
  id: string;
  project: string;
  name: string;
  source: Source;
  build: Build;
  run: Run;
  /** The private registry login its image pulls with (the password is never returned). */
  registry: { server: string; username: string } | null;
  /** What it links (the canvas draws them). */
  links?: { kind: LinkKind; ref: string }[];
  created_at: number;
  environments: EnvironmentSummary[];
}

export interface NewService {
  project: string;
  name: string;
  source: Source;
  build?: Partial<Build>;
  run?: Partial<Run>;
  /** Production's variables, set before its first deploy. */
  variables?: Record<string, string>;
  /** An image source's private registry login, before its first pull. */
  registry?: { username: string; password: string; server?: string };
  /** Resources to link before the first deploy (their variables are in it). */
  links?: { kind: LinkKind; ref: string; prefix?: string }[];
  x?: number;
  y?: number;
}

/**
 * `.env` text as variables: `KEY=value` lines (`export` allowed, `#` comments and blank lines
 * skipped, a value in single quotes as is, in double quotes with `\n`, `\r`, `\t`, `\"` and `\\`
 * as Compose's env files read them). Lines it can't read are named in `errors` (1-based).
 */
export function parseEnv(text: string): { vars: Record<string, string>; errors: number[] } {
  // No prototype: `__proto__` and `toString` are names like any other.
  const vars: Record<string, string> = Object.create(null);
  const errors: number[] = [];
  text.split(/\r?\n/).forEach((raw, i) => {
    const line = raw.trim().replace(/^export\s+/, "");
    if (!line || line.startsWith("#")) return;
    const eq = line.indexOf("=");
    const key = eq > 0 ? line.slice(0, eq).trim() : "";
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) {
      errors.push(i + 1);
      return;
    }
    let value = line.slice(eq + 1).trim();
    const q = value[0];
    if ((q === '"' || q === "'") && value.length >= 2 && value.endsWith(q)) {
      value = value.slice(1, -1);
      if (q === '"') value = value.replace(/\\([nrt"\\])/g, (_, c: string) => ({ n: "\n", r: "\r", t: "\t" })[c] ?? c);
    }
    vars[key] = value;
  });
  return { vars, errors };
}

/** Variables as `.env` text that {@link parseEnv} reads back as they are: plain when safe, else quoted. */
export function toEnv(vars: Record<string, string>): string {
  const quote = (v: string) => {
    if (/^[A-Za-z0-9_\-./:@,+=%?&~]*$/.test(v)) return v;
    if (!/['\r\n]/.test(v)) return `'${v}'`;
    return `"${v.replace(/[\\"\n\r\t]/g, (c) => ({ "\n": "\\n", "\r": "\\r", "\t": "\\t" })[c] ?? `\\${c}`)}"`;
  };
  return Object.entries(vars)
    .map(([k, v]) => `${k}=${quote(v)}\n`)
    .join("");
}

/** A field set to `null` goes back to its default (apps-control's `merge`). */
export type Reset<T> = { [K in keyof T]?: T[K] | null };

export interface ServicePatch {
  name?: string;
  source?: Source;
  build?: Reset<Build>;
  run?: Reset<Run>;
}

export interface IpRule {
  /** Canonical: `203.0.113.7/32`, `10.0.0.0/8`, `2001:db8::/32`. */
  cidr: string;
  description?: string;
}

/** What the next deploy changes (names only; null variables: no snapshot to compare). */
export interface Pending {
  variables: { added: string[]; removed: string[]; changed: string[] } | null;
  run: string[];
  /** The project's secrets changed since the live deployment (roll secrets' digest); null: nothing to compare. */
  secrets?: boolean | null;
}

/** The changes as a line ("MODE changed, ADDED added, memory_mb"), or null when there are none to name. */
export function describePending(p: Pending): string | null {
  const parts = [
    ...(p.variables?.changed ?? []).map((n) => `${n} changed`),
    ...(p.variables?.added ?? []).map((n) => `${n} added`),
    ...(p.variables?.removed ?? []).map((n) => `${n} removed`),
    ...p.run.map((f) => `${f.replaceAll("_", " ")} set`),
    ...(p.secrets ? ["the project's secrets changed"] : []),
  ];
  return parts.length ? parts.join(", ") : null;
}

export interface FirewallRule {
  action: "allow" | "deny";
  /** A path, or a prefix ending in `*`. */
  path?: string;
  methods?: string[];
  /** Text the User-Agent contains. */
  user_agent?: string;
}

/** Firewall rules as typed, one a line: `deny /admin/*`, `deny method=DELETE,PUT`, `allow /admin/health`, `deny ua=BadBot` (`ua=` last: the rest of the line). */
export function parseFirewall(text: string): FirewallRule[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith("#"))
    .map((l) => {
      const [action, ...rest] = l.split(/\s+/);
      const rule: FirewallRule = { action: action === "allow" ? "allow" : "deny" };
      for (let i = 0; i < rest.length; i++) {
        const t = rest[i];
        if (t.startsWith("ua=")) {
          rule.user_agent = [t.slice(3), ...rest.slice(i + 1)].join(" ");
          break;
        }
        if (t.startsWith("method=")) rule.methods = t.slice(7).split(",").filter(Boolean);
        else if (t.startsWith("/")) rule.path = t;
      }
      return rule;
    });
}

/** The rules as one line each, as `parseFirewall` reads them. */
export function formatFirewall(rules: FirewallRule[]): string {
  return rules
    .map((r) => [r.action, r.path, r.methods?.length ? `method=${r.methods.join(",")}` : undefined, r.user_agent ? `ua=${r.user_agent}` : undefined].filter(Boolean).join(" "))
    .join("\n");
}

/** Headers an environment's answers carry, on a path (or every path); the app's own of the same name wins unless `replace`. */
export interface HeaderRule {
  path?: string;
  set: Record<string, string>;
  replace?: boolean;
}

/**
 * Header rules as typed, one header a line: `[!][/path] Name: value` (`!`: replace the app's own;
 * no path: every answer). Lines with the same path and `!` are one rule.
 */
export function parseHeaders(text: string): HeaderRule[] {
  const rules: HeaderRule[] = [];
  for (const raw of text.split("\n")) {
    let l = raw.trim();
    if (!l || l.startsWith("#")) continue;
    const replace = l.startsWith("!");
    if (replace) l = l.slice(1).trim();
    let path: string | undefined;
    if (l.startsWith("/")) {
      const at = l.search(/\s/);
      path = at < 0 ? l : l.slice(0, at);
      l = at < 0 ? "" : l.slice(at).trim();
    }
    const colon = l.indexOf(":");
    if (colon <= 0) continue;
    const [name, value] = [l.slice(0, colon).trim(), l.slice(colon + 1).trim()];
    let rule = rules.find((r) => r.path === path && !!r.replace === replace);
    if (!rule) {
      rule = { ...(path ? { path } : {}), set: {}, ...(replace ? { replace } : {}) };
      rules.push(rule);
    }
    rule.set[name] = value;
  }
  return rules;
}

/** The rules as one header a line, as `parseHeaders` reads them. */
export function formatHeaders(rules: HeaderRule[]): string {
  return rules.flatMap((r) => Object.entries(r.set).map(([k, v]) => `${r.replace ? "!" : ""}${r.path ? `${r.path} ` : ""}${k}: ${v}`)).join("\n");
}

/** An allow list as typed, one network per line with an optional description after it. */
export function parseAllowList(text: string): IpRule[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l) => {
      const [cidr, ...rest] = l.split(/\s+/);
      const description = rest.join(" ").replace(/^#\s*/, "");
      return description ? { cidr, description } : { cidr };
    });
}

export interface EnvironmentPut {
  branch?: string | null;
  ephemeral?: boolean;
  /** Private: visitors need `?roll_access=<token>` once; null makes it public again. */
  access?: { token: string; bypass_sha256?: string } | null;
  /** Creating it: start with this environment's variables. */
  variables_from?: string;
  wait_for_ci?: boolean;
  /** Deploy when the image's tag moves (an image service with a tag). */
  follow_tag?: boolean;
  /** Redeploy (same image, new values) when the project's secrets change. */
  redeploy_on_secrets?: boolean;
  /** Maintenance: its public names answer roll's page with the message; false ends it. */
  maintenance?: boolean | { message?: string | null };
  /** Its own HTML (at most 64 KiB) instead of roll's pages; null goes back to roll's, an absent field stays. */
  pages?: { error?: string | null; maintenance?: string | null };
  /** Only these networks get in through its public names; null or [] lets anyone in. */
  allow_ips?: (string | IpRule)[] | null;
  /** Each client's requests a minute through its public names, with a burst; null: no limit. */
  rate_limit?: { per_minute: number; burst?: number } | null;
  /** Its firewall rules, in order (the first match decides); null or [] for none. */
  firewall?: FirewallRule[] | null;
  headers?: HeaderRule[] | null;
  /** Its own size and scale over the service's; a null field goes back to the service's. */
  run?: Partial<Record<EnvRunField, number | null>>;
}

/** The run settings an environment may set for itself. */
export const ENV_RUN_FIELDS = ["min_instances", "max_instances", "cpu", "memory_mb", "concurrency", "suspend_after_s", "sleep_after_s"] as const;
export type EnvRunField = (typeof ENV_RUN_FIELDS)[number];

export interface Instance {
  id: string;
  deployment: string;
  state: InstanceState;
  started_at: number | null;
  in_flight: number;
}

export interface Domain {
  hostname: string;
  kind: "roll" | "custom";
  /** The records a custom domain needs: a CNAME (a subdomain), the TXT challenge (an apex), or both
   *  (where the CNAME target is shared by every tenant, the TXT record proves the domain is yours). */
  record: "cname" | "txt" | "both" | "none";
  status: "pending" | "issuing" | "active" | "failed";
  target: string;
  token: string;
  error: string | null;
  /** The certificate: `none` where nothing issues them, else pending → issued, or failed. */
  tls: "none" | "pending" | "issued" | "failed";
  /** Why it failed, or what it's waiting for. */
  tls_error: string | null;
  created_at: number;
  /** Its requests go, 308, to this hostname of the same environment (www → apex). */
  redirect_to?: string | null;
}

export interface Environment extends EnvironmentSummary {
  service: string;
  branch: string | null;
  ephemeral: boolean;
  instances: Instance[];
  domains: Domain[];
  port: number | null;
  /** A TCP service's address (`host:port`), once it has its port. */
  tcp?: string | null;
  needs_deploy: boolean;
  /** Its hostnames answer only visitors with its access token (a preview). */
  private: boolean;
  /** A cron service's next scheduled run, once deployed. */
  next_run_at: number | null;
  /** Its instances keep crashing: three or more in the last ten minutes. */
  crash_loop: { crashes: number; last_at: number; last_error: string } | null;
  /** A gradual rollout under way: the new deployment and the percent of requests it takes. */
  canary?: { deployment: string; share: number } | null;
  /** Its own run settings over the service's; empty: the service's. */
  run?: Partial<Record<EnvRunField, number>>;
  /** It has a deploy hook. */
  hook?: boolean;
  /** A worker that follows its work: the backlog it last read and the instances that wants. */
  scaling?: { backlog: number | null; wanted: number } | null;
  /** An image service's tag is watched: a new digest behind it deploys. */
  follow_tag?: boolean;
  /** Redeploys (same image, new values) when the project's secrets change, checked each minute. */
  redeploy_on_secrets?: boolean;
  /** A push deploys only once its commit's checks pass. */
  wait_for_ci?: boolean;
  /** The push (sha) waiting for its checks, if one is. */
  waiting_for_ci?: string | null;
  /** In maintenance: what visitors read (null: roll's words). Null: serving. */
  maintenance?: { message: string | null } | null;
  /** Its own pages (Heroku's): `error` for the 502/503/504 while the app can't answer or wake, `maintenance` while in maintenance. Absent: roll's. */
  pages?: { error?: string; maintenance?: string };
  /** Only these networks get in through its public names (empty: anyone). */
  allow_ips?: IpRule[];
  /** Each client's requests a minute through its public names (null: no limit). */
  rate_limit?: { per_minute: number; burst?: number | null } | null;
  /** Its firewall rules, in order. */
  firewall?: FirewallRule[];
  /** Headers its answers carry. */
  headers?: HeaderRule[];
  created_at: number;
}

/** `…/deploys/{dep}/commits`: newest first, at most 50; `complete`: the one it's measured against was reached. Null: nothing to compare. */
export interface DeployCommits {
  since: { id: string; revision: string } | null;
  commits: { sha: string; summary: string; author: string }[] | null;
  complete: boolean;
}

export interface NewDeploy {
  revision?: string;
  image?: string;
  message?: string;
  /** Build from scratch: no cached layer, base images pulled again, the cache mounts empty from then on. */
  no_cache?: boolean;
  /** Built and health-checked, never put live: tried at its own URL first, then put live from the list. */
  no_traffic?: boolean;
}

export type DeployTrigger = "push" | "manual" | "rollback" | "upload" | "api" | "settings" | "promote";

export interface Plan {
  provider: "dockerfile" | "node" | "python" | "go" | "rust" | "ruby" | "php" | "java" | "static";
  framework: string | null;
  language_version: string | null;
  install: string | null;
  build: string | null;
  start: string | null;
  port: number;
  dockerfile: string;
  notes: string[];
  /** A single-page app: the proxy answers a page path with no file with index.html. */
  spa?: boolean;
  /** Datastores the code has client libraries for (New service offers to make and link them). */
  uses?: ("postgres" | "redis")[];
}

export type PlanRequest = { repo: string; revision?: string; root?: string } | { files: Record<string, string> };

export interface Deployment {
  id: string;
  service: string;
  environment: string;
  status: DeployStatus;
  trigger: DeployTrigger;
  /** Who asked: a push's commit author, the calling token ("operator" for the operator's); null when roll did it on its own. */
  actor: string | null;
  revision: string | null;
  message: string;
  image: string | null;
  plan: Plan | null;
  rollback_of: string | null;
  error: string | null;
  created_at: number;
  started_at: number | null;
  finished_at: number | null;
  ready_ms: number | null;
  /** What the app or the build printed when it failed (the step, its last line): only shown here, never sent in a commit status or a notification. */
  error_detail?: string;
  /** Pruned (older than its environment keeps): history only, its image gone, so it can't be rolled back to. */
  pruned_at?: number;
  /** When each step began (`build`, `pre_deploy`, `rollout`, then `live` or `failed`): on a single deployment's GET. */
  steps?: { step: string; at: number }[];
}

/** `service`: another web service of the project, its `<NAME>_URL` (private) and `<NAME>_PUBLIC_URL`. */
export type LinkKind = "postgres" | "redis" | "bucket" | "stream" | "queue" | "nosql" | "service";

export interface Link {
  kind: LinkKind;
  ref: string;
  prefix: string;
  vars: string[];
}

/** `reference`: one of its own with `${{ … }}` in it, filled in at the deploy. */
export type VariableSource = "service" | "secret" | "link" | "roll" | "reference";

export interface Variables {
  /**
   * reference: a reference variable's template (names, never a value). sealed: run with, never
   * shown again (not by `reveal` either); a reference that reads a sealed value is sealed too.
   */
  vars: { name: string; masked: string; source: VariableSource; reference?: string; sealed?: boolean }[];
}

export interface LogLine {
  t: number;
  /** `http`: the proxy's access log, its text JSON (`AccessLine`). */
  stream: "stdout" | "stderr" | "build" | "system" | "http";
  instance: string | null;
  deployment: string | null;
  text: string;
}

export interface Metrics {
  minutes: number[];
  requests: number[];
  errors: number[];
  p50_ms: number[];
  p95_ms: number[];
  p99_ms: number[];
  cpu: number[];
  memory_bytes: number[];
  instances: number[];
  cold_starts: number[];
  cold_start_ms: number[];
}

export interface Usage {
  /** `day` is a unix day (days since 1970-01-01, UTC); every day of the window is there. */
  days: { day: number; cpu_seconds: number; gb_seconds: number; requests: number; egress_bytes: number; build_seconds: number; usd: number }[];
  total_usd: number;
}

/** Where a project's events go: a signed webhook, a Slack or a Discord incoming webhook. */
export interface NotifyTarget {
  id: string;
  project: string;
  kind: "webhook" | "slack" | "discord";
  /** The URL's host (the URL itself isn't returned). */
  host: string;
  events: NotifyEvent[];
  /** Only these environments' events; empty: every environment's. */
  environments: string[];
  /** Only these services' events (ids); empty: every service's. */
  services: string[];
  /** Preview (ephemeral) environments' events too. */
  previews: boolean;
  created_at: number;
  /** Only in the answer to its creation, for a webhook: what its requests are signed with. */
  secret?: string;
}

export type NotifyEvent =
  | "deploy.failed"
  | "crash_loop"
  | "deploy.active"
  | "spend.alert"
  | "spend.limit"
  | "alert.memory"
  | "alert.errors"
  | "alert.latency"
  | "alert.log"
  | "cron.failed"
  | "job.failed"
  | "domain.active"
  | "domain.expiring"
  | "domain.expired"
  | "domain.tls_failed";

/** An entry of the event log (`GET /v1/apps/events`, a service's `…/activity`). */
export interface AppEvent {
  seq: number;
  at: number;
  kind: string;
  service: string;
  environment: string;
  deployment: string | null;
  data: Record<string, unknown>;
}

/** A project's log drain: where its services' runtime output is POSTed (NDJSON). */
export interface LogDrain {
  id: string;
  project: string;
  host: string;
  created_at: number;
  /** Only in the answer that made it. */
  secret?: string;
}

/** A project's monthly spend limits (US dollars at roll's prices, the calendar month in UTC). */
export interface SpendLimits {
  alert_usd: number | null;
  hard_usd: number | null;
  month_usd: number;
  /** Past `hard_usd` this month: its services are stopped, nothing wakes or deploys. */
  capped: boolean;
}

export interface NotifyDelivery {
  id: string;
  event: number;
  kind: string;
  status: "pending" | "delivered" | "failed";
  attempts: number;
  code: number | null;
  error: string | null;
  at: number;
}

export type RunStatus = "running" | "succeeded" | "failed" | "skipped" | "timed_out" | "canceled";

export interface CronRun {
  id: string;
  deployment: string;
  status: RunStatus;
  exit_code: number | null;
  started_at: number;
  finished_at: number | null;
}

/** A one-off command in a fresh container of the active deployment (`roll apps run`). */
/** A volume's backup in object storage (`…/volume/backups`). */
export interface Backup {
  id: string;
  at: number;
  size: number;
}

export interface Job {
  id: string;
  deployment: string;
  command: string;
  /** Containers it ran, each with ROLL_TASK_INDEX and ROLL_TASK_COUNT. */
  tasks?: number;
  status: RunStatus;
  exit_code: number | null;
  started_at: number;
  finished_at: number | null;
}

// ── routes ──

export const PRODUCTION = "production";

const enc = encodeURIComponent;
const svc = (id: string) => `/v1/apps/services/${enc(id)}`;
const env = (id: string, e: string) => `${svc(id)}/environments/${enc(e)}`;
const dep = (id: string, e: string, d: string) => `${env(id, e)}/deploys/${enc(d)}`;

function qs(q: Record<string, string | number | boolean | undefined>) {
  const p = Object.entries(q).filter(([, v]) => v !== undefined && v !== false);
  return p.length ? `?${p.map(([k, v]) => `${k}=${enc(v === true ? "1" : String(v))}`).join("&")}` : "";
}

export interface LogQuery {
  tail?: number;
  /** Runtime logs only: unix ms. */
  since?: number;
}

export const appsApi = {
  overview: () => call<Overview>("GET", "/v1/apps"),
  plan: (req: PlanRequest) => call<Plan>("POST", "/v1/apps/plan", req),

  services: (project?: string) => call<Service[]>("GET", `/v1/apps/services${qs({ project })}`),
  /** Makes the service and places its card; a repo or an image source deploys to production at once. */
  create: (body: NewService) => call<Service>("POST", "/v1/apps/services", body),
  service: (id: string) => call<Service>("GET", svc(id)),
  update: (id: string, patch: ServicePatch) => call<Service>("PATCH", svc(id), patch),
  remove: (id: string) => call<null>("DELETE", svc(id)),

  links: (id: string) => call<Link[]>("GET", `${svc(id)}/links`),
  activity: (id: string) => call<AppEvent[]>("GET", `${svc(id)}/activity`),
  link: (id: string, kind: LinkKind, ref: string, prefix?: string) =>
    call<Link>("PUT", `${svc(id)}/links/${kind}/${enc(ref)}`, prefix ? { prefix } : {}),
  unlink: (id: string, kind: LinkKind, ref: string) => call<null>("DELETE", `${svc(id)}/links/${kind}/${enc(ref)}`),

  environments: (id: string) => call<Environment[]>("GET", `${svc(id)}/environments`),
  environment: (id: string, e: string) => call<Environment>("GET", env(id, e)),
  notifyTargets: (project: string) => call<NotifyTarget[]>("GET", `/v1/apps/projects/${enc(project)}/notifications`),
  drains: (project: string) => call<LogDrain[]>("GET", `/v1/apps/projects/${enc(project)}/drains`),
  addDrain: (project: string, url: string) => call<LogDrain>("POST", `/v1/apps/projects/${enc(project)}/drains`, { url }),
  removeDrain: (project: string, id: string) => call<null>("DELETE", `/v1/apps/projects/${enc(project)}/drains/${enc(id)}`),
  limits: (project: string) => call<SpendLimits>("GET", `/v1/apps/projects/${enc(project)}/limits`),
  setLimits: (project: string, body: { alert_usd: number | null; hard_usd: number | null }) => call<SpendLimits>("PUT", `/v1/apps/projects/${enc(project)}/limits`, body),
  addNotifyTarget: (project: string, body: { kind: NotifyTarget["kind"]; url: string; events?: NotifyEvent[]; environments?: string[]; services?: string[]; previews?: boolean }) => call<NotifyTarget>("POST", `/v1/apps/projects/${enc(project)}/notifications`, body),
  removeNotifyTarget: (project: string, id: string) => call<null>("DELETE", `/v1/apps/projects/${enc(project)}/notifications/${enc(id)}`),
  testNotifyTarget: (project: string, id: string) => call<{ ok: boolean; code: number | null; error: string | null }>("POST", `/v1/apps/projects/${enc(project)}/notifications/${enc(id)}/test`),
  notifyDeliveries: (project: string, id: string) => call<NotifyDelivery[]>("GET", `/v1/apps/projects/${enc(project)}/notifications/${enc(id)}/deliveries`),
  putRegistry: (id: string, body: { username: string; password: string; server?: string }) => call<Service>("PUT", `${svc(id)}/registry`, body),
  deleteRegistry: (id: string) => call<Service>("DELETE", `${svc(id)}/registry`),
  putEnvironment: (id: string, e: string, body: EnvironmentPut) => call<Environment>("PUT", env(id, e), body),
  removeEnvironment: (id: string, e: string) => call<null>("DELETE", env(id, e)),

  deploy: (id: string, e: string, body: NewDeploy = {}) => call<Deployment>("POST", `${env(id, e)}/deploys`, body),
  deploys: (id: string, e: string) => call<Deployment[]>("GET", `${env(id, e)}/deploys`),
  deployment: (id: string, e: string, d: string) => call<Deployment>("GET", dep(id, e, d)),
  /** What a repo deployment brings over the live one (for the live one, the one before it). */
  commits: (id: string, e: string, d: string) => call<DeployCommits>("GET", `${dep(id, e, d)}/commits`),
  buildLog: (id: string, e: string, d: string, q: LogQuery = {}) => call<LogLine[]>("GET", `${dep(id, e, d)}/logs${qs({ tail: q.tail })}`),
  /** `keepVariables`: its image with the variables as they are now (after a rotated secret), not as they were then. */
  rollback: (id: string, e: string, d: string, keepVariables = false) => call<Deployment>("POST", `${dep(id, e, d)}/rollback`, keepVariables ? { keep_variables: true } : undefined),
  /** Back to the one live before what runs (the server walks back through rollbacks). */
  rollbackPrevious: (id: string, e: string) => call<Deployment>("POST", `${env(id, e)}/rollback`),
  /** The live image again with the current variables and settings: no build, same revision. */
  redeploy: (id: string, e: string, body: { message?: string } = {}) => call<Deployment>("POST", `${env(id, e)}/redeploy`, body),
  promote: (id: string, e: string, from: string) => call<Deployment>("POST", `${env(id, e)}/promote`, { from }),
  backups: (id: string, e: string) => call<Backup[]>("GET", `${env(id, e)}/volume/backups`),
  /** A new deploy hook (the old one stops working): its URL, shown once. */
  createHook: async (id: string, e: string) => `${ROLL_API}${(await call<{ path: string }>("POST", `${env(id, e)}/hook`)).path}`,
  removeHook: (id: string, e: string) => call<void>("DELETE", `${env(id, e)}/hook`),
  backUp: (id: string, e: string) => call<Backup>("POST", `${env(id, e)}/volume/backups`),
  restore: (id: string, e: string, b: string) => call<Backup>("POST", `${env(id, e)}/volume/backups/${encodeURIComponent(b)}/restore`),
  cancel: (id: string, e: string, d: string) => call<Deployment>("POST", `${dep(id, e, d)}/cancel`),

  restart: (id: string, e: string) => call<Environment>("POST", `${env(id, e)}/restart`),
  /** Just this one: a fresh instance answers before it leaves (a web service's), the others stay. */
  restartInstance: (id: string, e: string, name: string) => call<Environment>("POST", `${env(id, e)}/instances/${encodeURIComponent(name)}/restart`),
  sleep: (id: string, e: string) => call<Environment>("POST", `${env(id, e)}/sleep`),
  wake: (id: string, e: string) => call<Environment>("POST", `${env(id, e)}/wake`),

  variables: (id: string, e: string) => call<Variables>("GET", `${env(id, e)}/variables`),
  /** What the next deploy changes: its own variables by name, and run settings. */
  pending: (id: string, e: string) => call<Pending>("GET", `${env(id, e)}/pending`),
  /** The values in the clear (the `reveal` action). */
  reveal: (id: string, e: string) => call<Record<string, string>>("GET", `${env(id, e)}/variables/values`),
  /** `seal`: names set here or already set, sealed for good (a new value keeps them sealed). */
  setVariables: (id: string, e: string, change: { set?: Record<string, string>; unset?: string[]; seal?: string[] }) =>
    call<Variables>("PATCH", `${env(id, e)}/variables`, change),

  logs: (id: string, e: string, q: LogQuery = {}) => call<LogLine[]>("GET", `${env(id, e)}/logs${qs({ tail: q.tail, since: q.since })}`),
  /** Lines that contain `q` (any case), back through the stored logs (7 days), newest last. */
  /** `truncated`: more matched than came back (the newest are shown). */
  searchLogs: (id: string, e: string, q: string, since?: number) => appsApi.readLogs(id, e, { q, since, tail: 2000 }),
  /**
   * Lines read back through the stored logs (7 days), newest last: `before` (unix ms) a time
   * window's end, `instance`, `deployment`, `stream` narrowing it, `q` a search. `truncated`: the
   * read stopped before it reached them all.
   */
  readLogs: async (
    id: string,
    e: string,
    q: { q?: string; since?: number; before?: number; instance?: string; deployment?: string; stream?: string; http?: boolean; by?: string; tail?: number },
  ): Promise<{ lines: LogLine[]; truncated: boolean }> => {
    const res = await fetch(`${ROLL_API}${env(id, e)}/logs${qs({ ...q, http: q.http || undefined, tail: q.tail ?? 500 })}`, { headers: apiHeaders(), cache: "no-store" });
    onApiStatus(res.status);
    const data = await res.json().catch(() => null);
    if (!res.ok) throw new Error(data?.error ?? `${res.status} ${res.statusText}`.trim());
    return { lines: data as LogLine[], truncated: res.headers.get("x-roll-search-truncated") === "1" };
  },
  metrics: (id: string, e: string, minutes = 60) => call<Metrics>("GET", `${env(id, e)}/metrics${qs({ minutes })}`),
  usage: (id: string, e: string, days = 30) => call<Usage>("GET", `${env(id, e)}/usage${qs({ days })}`),

  runs: (id: string, e: string) => call<CronRun[]>("GET", `${env(id, e)}/runs`),
  runNow: (id: string, e: string) => call<CronRun>("POST", `${env(id, e)}/runs`),
  run: (id: string, e: string, r: string) => call<CronRun>("GET", `${env(id, e)}/runs/${encodeURIComponent(r)}`),
  cancelRun: (id: string, e: string, r: string) => call<CronRun>("POST", `${env(id, e)}/runs/${encodeURIComponent(r)}/cancel`),
  jobs: (id: string, e: string) => call<Job[]>("GET", `${env(id, e)}/jobs`),
  startJob: (id: string, e: string, body: { command: string; timeout_s?: number; tasks?: number }) => call<Job>("POST", `${env(id, e)}/jobs`, body),
  job: (id: string, e: string, j: string) => call<Job>("GET", `${env(id, e)}/jobs/${encodeURIComponent(j)}`),
  cancelJob: (id: string, e: string, j: string) => call<Job>("POST", `${env(id, e)}/jobs/${encodeURIComponent(j)}/cancel`),

  /** A shell in a running instance over HTTP (a browser can't send the token on a WebSocket). */
  openShell: (id: string, e: string, size: { cols: number; rows: number }) => call<{ id: string }>("POST", `${env(id, e)}/shell/sessions`, size),
  shellKeys: (id: string, e: string, sh: string, data: string) => call<null>("POST", `${env(id, e)}/shell/sessions/${encodeURIComponent(sh)}/keys`, { data }),
  resizeShell: (id: string, e: string, sh: string, size: { cols: number; rows: number }) => call<null>("POST", `${env(id, e)}/shell/sessions/${encodeURIComponent(sh)}/resize`, size),
  closeShell: (id: string, e: string, sh: string) => call<null>("DELETE", `${env(id, e)}/shell/sessions/${encodeURIComponent(sh)}`),

  domains: (id: string, e: string) => call<Domain[]>("GET", `${env(id, e)}/domains`),
  addDomain: (id: string, e: string, hostname: string) => call<Domain>("POST", `${env(id, e)}/domains`, { hostname }),
  verifyDomain: (id: string, e: string, host: string) => call<Domain>("POST", `${env(id, e)}/domains/${enc(host)}/verify`),
  removeDomain: (id: string, e: string, host: string) => call<null>("DELETE", `${env(id, e)}/domains/${enc(host)}`),
  redirectDomain: (id: string, e: string, host: string, to: string | null) => call<Domain>("PATCH", `${env(id, e)}/domains/${enc(host)}`, { redirect_to: to }),
};

/**
 * Follows a log over server-sent events (`?follow=1`), with the operator token EventSource can't
 * send. `onReset` runs on every (re)connect: the stream starts again with the last `tail` lines,
 * so the caller drops what it had. Returns the function that stops it.
 */
/** A shell's screen over SSE: base64 chunks (the scrollback first, again after a reconnect: `onReset`), then `onClosed`. */
export function followShell(
  service: string,
  e: string,
  sh: string,
  opts: { onChunk: (bytes: Uint8Array) => void; onReset: () => void; onClosed: () => void },
): () => void {
  let opened = false;
  const stop = streamSse(`${ROLL_API}${env(service, e)}/shell/sessions/${encodeURIComponent(sh)}/screen`, {
    headers: operatorHeaders,
    onOpen: (open) => {
      if (open && opened) opts.onReset();
      if (open) opened = true;
    },
    onEvent: (ev) => {
      if (ev.event === "closed") {
        stop();
        opts.onClosed();
        return;
      }
      const bin = atob(ev.data);
      const out = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
      opts.onChunk(out);
    },
  });
  return stop;
}

/** An access line's text (`http` lines). */
export interface AccessLine {
  method: string;
  path: string;
  status: number;
  ms: number;
  bytes: number;
  /** Who asked: the address the fleet vouches for, the user agent, the host. */
  ip?: string | null;
  ua?: string;
  host?: string;
  instance?: string;
  request_id?: string;
  cold?: boolean;
  waited_ms?: number;
  /** An answer roll gave itself, before any instance, and why. */
  by?: "allow_list" | "firewall" | "rate_limit" | "private" | "maintenance" | "wake" | "capped" | "not_deployed" | "moving" | "unavailable";
}

/** Follows a service's changes (`…/changes`): `onChange` for each tick, `onOpen` as the stream comes and goes. */
export function followChanges(service: string, opts: { onChange: () => void; onOpen?: (open: boolean) => void }): () => void {
  return streamSse(`${ROLL_API}/v1/apps/services/${encodeURIComponent(service)}/changes`, {
    headers: operatorHeaders,
    onOpen: opts.onOpen,
    onEvent: (e) => {
      if (e.event === "changed") opts.onChange();
    },
  });
}

export function followLog(
  kind:
    | { runtime: { service: string; env: string; http?: boolean } }
    | { build: { service: string; env: string; deployment: string } }
    | { job: { service: string; env: string; job: string } }
    | { run: { service: string; env: string; run: string } },
  opts: { tail?: number; onLine: (l: LogLine) => void; onReset: () => void; onOpen?: (open: boolean) => void },
): () => void {
  const path =
    "runtime" in kind
      ? `${env(kind.runtime.service, kind.runtime.env)}/logs`
      : "job" in kind
        ? `${env(kind.job.service, kind.job.env)}/jobs/${encodeURIComponent(kind.job.job)}/logs`
        : "run" in kind
          ? `${env(kind.run.service, kind.run.env)}/runs/${encodeURIComponent(kind.run.run)}/logs`
          : `${dep(kind.build.service, kind.build.env, kind.build.deployment)}/logs`;
  const http = "runtime" in kind && kind.runtime.http ? true : undefined;
  return streamSse(`${ROLL_API}${path}${qs({ follow: true, tail: opts.tail ?? 500, http })}`, {
    headers: operatorHeaders,
    onOpen: (open) => {
      if (open) opts.onReset();
      opts.onOpen?.(open);
    },
    onEvent: (e) => {
      try {
        opts.onLine(JSON.parse(e.data) as LogLine);
      } catch {
        // Not a log line (a comment the parser kept): skip it.
      }
    },
  });
}

// ── how the dashboard shows them ──

export type Tone = "busy" | "ok" | "error" | "muted";

const DEPLOY: Record<DeployStatus, { label: string; tone: Tone }> = {
  queued: { label: "Queued", tone: "busy" },
  building: { label: "Building", tone: "busy" },
  deploying: { label: "Deploying", tone: "busy" },
  active: { label: "Active", tone: "ok" },
  inactive: { label: "Inactive", tone: "muted" },
  failed: { label: "Failed", tone: "error" },
  canceled: { label: "Canceled", tone: "muted" },
  superseded: { label: "Superseded", tone: "muted" },
  removed: { label: "Removed", tone: "muted" },
  ready: { label: "Ready, no traffic", tone: "muted" },
};

/** A deployment's own URL (`<id>--<slug>`), from its environment's: a web service's built deployment runs there, whatever is live. */
export function deploymentUrl(envUrl: string | null | undefined, deployment: string): string | null {
  if (!envUrl) return null;
  try {
    const u = new URL(envUrl);
    u.hostname = `${deployment.replace(/^dep_/, "")}--${u.hostname}`;
    return u.toString().replace(/\/$/, "");
  } catch {
    return null;
  }
}

export const deployStatus = (s: DeployStatus) => DEPLOY[s];

const STEP_LABEL: Record<string, string> = { build: "Build", pre_deploy: "Pre-deploy", rollout: "Rollout", live: "Live", failed: "Failed" };

/** A deployment's steps as label and duration: queued until the first, each until the next; the last (live, failed) has none. `now` closes a step still running. */
export function stepDurations(d: Pick<Deployment, "created_at" | "steps">, now = Date.now()): { label: string; ms: number | null }[] {
  const steps = d.steps ?? [];
  if (!steps.length) return [];
  const out: { label: string; ms: number | null }[] = [{ label: "Queued", ms: steps[0].at - d.created_at }];
  steps.forEach((s, i) => {
    const end = steps[i + 1]?.at ?? (s.step === "live" || s.step === "failed" ? null : now);
    out.push({ label: STEP_LABEL[s.step] ?? (s.step.startsWith("canary_") ? `${s.step.slice(7)}% of requests` : s.step), ms: end === null ? null : end - s.at });
  });
  return out;
}

const RUN: Record<RunStatus, { label: string; tone: Tone }> = {
  running: { label: "Running", tone: "busy" },
  succeeded: { label: "Succeeded", tone: "ok" },
  failed: { label: "Failed", tone: "error" },
  timed_out: { label: "Timed out", tone: "error" },
  canceled: { label: "Canceled", tone: "muted" },
  skipped: { label: "Skipped", tone: "muted" },
};

export const runStatus = (s: RunStatus) => RUN[s];

/** Still on its way: queued, building or rolling out. */
export const isPending = (s: DeployStatus) => DEPLOY[s].tone === "busy";

const INSTANCE: Record<InstanceState, string> = {
  none: "Not deployed",
  stopped: "Asleep",
  suspended: "Suspended",
  waking: "Waking",
  running: "Running",
  crashed: "Crashed",
  deploying: "Deploying…",
};

export const instanceStateLabel = (s: InstanceState) => INSTANCE[s];

/** The canvas's state for an instance state: busy ones are `starting` (the accent), asleep is `idle`. */
export function instanceTone(s: InstanceState): "online" | "starting" | "idle" | "failed" {
  switch (s) {
    case "running":
      return "online";
    case "waking":
    case "deploying":
      return "starting";
    case "crashed":
      return "failed";
    case "stopped":
    case "suspended":
    case "none":
      return "idle";
  }
}

export const TRIGGER_LABEL: Record<DeployTrigger, string> = {
  push: "Push",
  manual: "Manual",
  rollback: "Rollback",
  upload: "roll up",
  api: "API",
  settings: "Redeploy",
  promote: "Promote",
};

/** The environment a service's card and page show first: production, else the oldest. */
export const mainEnvironment = (s: Pick<Service, "environments">) =>
  s.environments.find((e) => e.name === PRODUCTION) ?? s.environments[0] ?? null;

export const hostOf = (url: string | null | undefined) => {
  if (!url) return "";
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
};

export const shortSha = (sha: string | null | undefined) => (sha ? sha.slice(0, 7) : "");

export function sourceLabel(s: Source) {
  switch (s.kind) {
    case "repo":
      return s.root ? `${s.repo} · ${s.root}` : s.repo;
    case "image":
      return s.image;
    case "upload":
      return "roll up";
  }
}

/** `412 ms`, `3.2 s`, `2m 10s`. */
export function formatMs(ms: number | null | undefined) {
  if (ms == null) return "—";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`;
  const s = Math.round(ms / 1000);
  return `${Math.floor(s / 60)}m ${s % 60}s`;
}

/** Service and environment names (http-api.md): 1–32 of `[a-z0-9-]`, no `-` at either end. */
export const SERVICE_NAME = /^[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?$/;

/** Variable names as a shell takes them. */
export const VARIABLE_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

export interface DeployWatch {
  /** `service:env` → the latest deployment's id and status, as last seen. */
  seen: Map<string, { id: string; status: DeployStatus }>;
}

/**
 * What the shared mark should do between two looks at the services: `deploying` while any
 * deploy is on its way, and how the ones that finished since `before` ended (an error wins).
 * A deploy seen for the first time already finished says nothing.
 */
export function deployChange(
  before: DeployWatch["seen"],
  services: Pick<Service, "id" | "environments">[],
): { pending: boolean; finished: "success" | "error" | null; seen: DeployWatch["seen"] } {
  const seen: DeployWatch["seen"] = new Map();
  let pending = false;
  let finished: "success" | "error" | null = null;
  for (const s of services)
    for (const e of s.environments) {
      if (!e.latest) continue;
      const key = `${s.id}:${e.name}`;
      seen.set(key, e.latest);
      if (isPending(e.latest.status)) {
        pending = true;
        continue;
      }
      const was = before.get(key);
      const wasPending = was && isPending(was.status);
      // The deploy that was pending ended, or a newer one came and went between two looks.
      if (wasPending || (was && was.id !== e.latest.id)) {
        const failed = deployStatus(e.latest.status).tone === "error";
        if (failed) finished = "error";
        else if (e.latest.status === "active" && finished === null) finished = "success";
      }
    }
  return { pending, finished, seen };
}
