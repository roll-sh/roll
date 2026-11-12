/**
 * Client for roll apps (`crates/apps-control`, `/v1/apps`): the contract is docs/apps/http-api.md,
 * and the types below are its "Shapes". Change both together.
 */

import { operatorHeaders } from "./code-token";
import { call, ROLL_API } from "./roll-api";
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
}

export type ServiceType = "web" | "worker" | "cron";

export interface Run {
  type: ServiceType;
  port?: number;
  start?: string;
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
  created_at: number;
  environments: EnvironmentSummary[];
}

export interface NewService {
  project: string;
  name: string;
  source: Source;
  build?: Partial<Build>;
  run?: Partial<Run>;
  x?: number;
  y?: number;
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
  status: "pending" | "issuing" | "active" | "failed";
  target: string;
  token: string;
  error: string | null;
  created_at: number;
}

export interface Environment extends EnvironmentSummary {
  service: string;
  branch: string | null;
  ephemeral: boolean;
  instances: Instance[];
  domains: Domain[];
  port: number | null;
  needs_deploy: boolean;
  created_at: number;
}

export interface NewDeploy {
  revision?: string;
  image?: string;
  message?: string;
}

export type DeployTrigger = "push" | "manual" | "rollback" | "upload" | "api" | "settings";

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
}

export type PlanRequest = { repo: string; revision?: string; root?: string } | { files: Record<string, string> };

export interface Deployment {
  id: string;
  service: string;
  environment: string;
  status: DeployStatus;
  trigger: DeployTrigger;
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
  stream: "stdout" | "stderr" | "build" | "system";
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

export interface CronRun {
  id: string;
  deployment: string;
  status: "running" | "succeeded" | "failed" | "skipped" | "timed_out";
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
  putEnvironment: (id: string, e: string, body: EnvironmentPut) => call<Environment>("PUT", env(id, e), body),
  removeEnvironment: (id: string, e: string) => call<null>("DELETE", env(id, e)),

  deploy: (id: string, e: string, body: NewDeploy = {}) => call<Deployment>("POST", `${env(id, e)}/deploys`, body),
  deploys: (id: string, e: string) => call<Deployment[]>("GET", `${env(id, e)}/deploys`),
  deployment: (id: string, e: string, d: string) => call<Deployment>("GET", dep(id, e, d)),
  buildLog: (id: string, e: string, d: string, q: LogQuery = {}) => call<LogLine[]>("GET", `${dep(id, e, d)}/logs${qs({ tail: q.tail })}`),
  rollback: (id: string, e: string, d: string) => call<Deployment>("POST", `${dep(id, e, d)}/rollback`),
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
  metrics: (id: string, e: string, minutes = 60) => call<Metrics>("GET", `${env(id, e)}/metrics${qs({ minutes })}`),
  usage: (id: string, e: string, days = 30) => call<Usage>("GET", `${env(id, e)}/usage${qs({ days })}`),

  runs: (id: string, e: string) => call<CronRun[]>("GET", `${env(id, e)}/runs`),
  runNow: (id: string, e: string) => call<CronRun>("POST", `${env(id, e)}/runs`),

  domains: (id: string, e: string) => call<Domain[]>("GET", `${env(id, e)}/domains`),
  addDomain: (id: string, e: string, hostname: string) => call<Domain>("POST", `${env(id, e)}/domains`, { hostname }),
  verifyDomain: (id: string, e: string, host: string) => call<Domain>("POST", `${env(id, e)}/domains/${enc(host)}/verify`),
  removeDomain: (id: string, e: string, host: string) => call<null>("DELETE", `${env(id, e)}/domains/${enc(host)}`),
};

/**
 * Follows a log over server-sent events (`?follow=1`), with the operator token EventSource can't
 * send. `onReset` runs on every (re)connect: the stream starts again with the last `tail` lines,
 * so the caller drops what it had. Returns the function that stops it.
 */
export function followLog(
  kind: { runtime: { service: string; env: string } } | { build: { service: string; env: string; deployment: string } },
  opts: { tail?: number; onLine: (l: LogLine) => void; onReset: () => void; onOpen?: (open: boolean) => void },
): () => void {
  const path = "runtime" in kind ? `${env(kind.runtime.service, kind.runtime.env)}/logs` : `${dep(kind.build.service, kind.build.env, kind.build.deployment)}/logs`;
  return streamSse(`${ROLL_API}${path}${qs({ follow: true, tail: opts.tail ?? 500 })}`, {
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
  settings: "Settings",
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
