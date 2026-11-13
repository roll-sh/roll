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
}

export type ServiceType = "web" | "worker" | "cron";

export interface Run {
  type: ServiceType;
  port?: number;
  start?: string;
  /** Once per deploy before the switch (a migration); a non-zero exit fails the deploy. */
  pre_deploy?: string;
  schedule?: string;
  timeout_s?: number;
  min_instances: number;
  max_instances: number;
  concurrency: number;
  cpu: number;
  memory_mb: number;
  health_path?: string;
  health_timeout_s: number;
  suspend_after_s: number;
  sleep_after_s: number;
  volume?: { mount: string; size_gb: number };
  /** Metric alerts (`alert.*` events), over five minutes; each 0 is off. */
  alerts?: { memory_pct: number; error_pct: number; p95_ms: number };
}

export type InstanceState = "none" | "stopped" | "suspended" | "waking" | "running" | "crashed" | "deploying";

export type DeployStatus = "queued" | "building" | "deploying" | "active" | "inactive" | "failed" | "canceled" | "superseded" | "removed";

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
  x?: number;
  y?: number;
}

/**
 * `.env` text as variables: `KEY=value` lines (`export` allowed, `#` comments and blank lines
 * skipped, a value in single quotes as is, in double quotes with `\n` for a line break). Lines it
 * can't read are named in `errors` (1-based).
 */
export function parseEnv(text: string): { vars: Record<string, string>; errors: number[] } {
  const vars: Record<string, string> = {};
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
      if (q === '"') value = value.replace(/\\n/g, "\n").replace(/\\"/g, '"');
    }
    vars[key] = value;
  });
  return { vars, errors };
}

/** A field set to `null` goes back to its default (apps-control's `merge`). */
export type Reset<T> = { [K in keyof T]?: T[K] | null };

export interface ServicePatch {
  name?: string;
  source?: Source;
  build?: Reset<Build>;
  run?: Reset<Run>;
}

export interface EnvironmentPut {
  branch?: string | null;
  ephemeral?: boolean;
  /** Private: visitors need `?roll_access=<token>` once; null makes it public again. */
  access?: { token: string; bypass_sha256?: string } | null;
  /** Creating it: start with this environment's variables. */
  variables_from?: string;
  wait_for_ci?: boolean;
}

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
  needs_deploy: boolean;
  /** Its hostnames answer only visitors with its access token (a preview). */
  private: boolean;
  /** A cron service's next scheduled run, once deployed. */
  next_run_at: number | null;
  /** Its instances keep crashing: three or more in the last ten minutes. */
  crash_loop: { crashes: number; last_at: number; last_error: string } | null;
  /** A push deploys only once its commit's checks pass. */
  wait_for_ci?: boolean;
  /** The push (sha) waiting for its checks, if one is. */
  waiting_for_ci?: string | null;
  created_at: number;
}

export interface NewDeploy {
  revision?: string;
  image?: string;
  message?: string;
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
  /** When each step began (`build`, `pre_deploy`, `rollout`, then `live` or `failed`): on a single deployment's GET. */
  steps?: { step: string; at: number }[];
}

export type LinkKind = "postgres" | "redis" | "bucket" | "stream" | "queue" | "nosql";

export interface Link {
  kind: LinkKind;
  ref: string;
  prefix: string;
  vars: string[];
}

export type VariableSource = "service" | "secret" | "link" | "roll";

export interface Variables {
  vars: { name: string; masked: string; source: VariableSource }[];
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
  created_at: number;
  /** Only in the answer to its creation, for a webhook: what its requests are signed with. */
  secret?: string;
}

export type NotifyEvent = "deploy.failed" | "crash_loop" | "deploy.active" | "spend.alert" | "spend.limit" | "alert.memory" | "alert.errors" | "alert.latency" | "cron.failed" | "job.failed";

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
export interface Job {
  id: string;
  deployment: string;
  command: string;
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
  addNotifyTarget: (project: string, body: { kind: NotifyTarget["kind"]; url: string; events?: NotifyEvent[] }) => call<NotifyTarget>("POST", `/v1/apps/projects/${enc(project)}/notifications`, body),
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
  buildLog: (id: string, e: string, d: string, q: LogQuery = {}) => call<LogLine[]>("GET", `${dep(id, e, d)}/logs${qs({ tail: q.tail })}`),
  rollback: (id: string, e: string, d: string) => call<Deployment>("POST", `${dep(id, e, d)}/rollback`),
  /** The live image again with the current variables and settings: no build, same revision. */
  redeploy: (id: string, e: string, body: { message?: string } = {}) => call<Deployment>("POST", `${env(id, e)}/redeploy`, body),
  promote: (id: string, e: string, from: string) => call<Deployment>("POST", `${env(id, e)}/promote`, { from }),
  cancel: (id: string, e: string, d: string) => call<Deployment>("POST", `${dep(id, e, d)}/cancel`),

  restart: (id: string, e: string) => call<Environment>("POST", `${env(id, e)}/restart`),
  sleep: (id: string, e: string) => call<Environment>("POST", `${env(id, e)}/sleep`),
  wake: (id: string, e: string) => call<Environment>("POST", `${env(id, e)}/wake`),

  variables: (id: string, e: string) => call<Variables>("GET", `${env(id, e)}/variables`),
  /** The values in the clear (the `reveal` action). */
  reveal: (id: string, e: string) => call<Record<string, string>>("GET", `${env(id, e)}/variables/values`),
  setVariables: (id: string, e: string, change: { set?: Record<string, string>; unset?: string[] }) =>
    call<Variables>("PATCH", `${env(id, e)}/variables`, change),

  logs: (id: string, e: string, q: LogQuery = {}) => call<LogLine[]>("GET", `${env(id, e)}/logs${qs({ tail: q.tail, since: q.since })}`),
  /** Lines that contain `q` (any case), back through the stored logs (7 days), newest last. */
  /** `truncated`: more matched than came back (the newest are shown). */
  searchLogs: async (id: string, e: string, q: string, since?: number): Promise<{ lines: LogLine[]; truncated: boolean }> => {
    const res = await fetch(`${ROLL_API}${env(id, e)}/logs${qs({ q, since, tail: 2000 })}`, { headers: apiHeaders(), cache: "no-store" });
    onApiStatus(res.status);
    const data = await res.json().catch(() => null);
    if (!res.ok) throw new Error(data?.error ?? `${res.status} ${res.statusText}`.trim());
    return { lines: data as LogLine[], truncated: res.headers.get("x-roll-search-truncated") === "1" };
  },
  metrics: (id: string, e: string, minutes = 60) => call<Metrics>("GET", `${env(id, e)}/metrics${qs({ minutes })}`),
  usage: (id: string, e: string, days = 30) => call<Usage>("GET", `${env(id, e)}/usage${qs({ days })}`),

  runs: (id: string, e: string) => call<CronRun[]>("GET", `${env(id, e)}/runs`),
  runNow: (id: string, e: string) => call<CronRun>("POST", `${env(id, e)}/runs`),
  jobs: (id: string, e: string) => call<Job[]>("GET", `${env(id, e)}/jobs`),
  startJob: (id: string, e: string, body: { command: string; timeout_s?: number }) => call<Job>("POST", `${env(id, e)}/jobs`, body),
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
  instance: string;
  request_id: string;
  cold: boolean;
  waited_ms: number;
}

export function followLog(
  kind: { runtime: { service: string; env: string; http?: boolean } } | { build: { service: string; env: string; deployment: string } } | { job: { service: string; env: string; job: string } },
  opts: { tail?: number; onLine: (l: LogLine) => void; onReset: () => void; onOpen?: (open: boolean) => void },
): () => void {
  const path =
    "runtime" in kind
      ? `${env(kind.runtime.service, kind.runtime.env)}/logs`
      : "job" in kind
        ? `${env(kind.job.service, kind.job.env)}/jobs/${encodeURIComponent(kind.job.job)}/logs`
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
};

export const deployStatus = (s: DeployStatus) => DEPLOY[s];

const STEP_LABEL: Record<string, string> = { build: "Build", pre_deploy: "Pre-deploy", rollout: "Rollout", live: "Live", failed: "Failed" };

/** A deployment's steps as label and duration: queued until the first, each until the next; the last (live, failed) has none. `now` closes a step still running. */
export function stepDurations(d: Pick<Deployment, "created_at" | "steps">, now = Date.now()): { label: string; ms: number | null }[] {
  const steps = d.steps ?? [];
  if (!steps.length) return [];
  const out: { label: string; ms: number | null }[] = [{ label: "Queued", ms: steps[0].at - d.created_at }];
  steps.forEach((s, i) => {
    const end = steps[i + 1]?.at ?? (s.step === "live" || s.step === "failed" ? null : now);
    out.push({ label: STEP_LABEL[s.step] ?? s.step, ms: end === null ? null : end - s.at });
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
