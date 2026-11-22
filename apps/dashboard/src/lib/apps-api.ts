/**
 * Client for roll apps (`crates/apps-control`, `/v1/apps`): the contract is docs/apps/http-api.md,
 * and the types below are its "Shapes". Change both together.
 */

import { operatorHeaders, withSession } from "./code-token";
import { apiHeaders, call, onApiStatus, ROLL_API } from "./roll-api";
import { followScreen, streamSse, type ScreenHandlers } from "./sse";

// ── shapes (docs/apps/http-api.md) ──

export interface Prices {
  cpu_second: number;
  gb_second: number;
  egress_gb: number;
  build_minute: number;
  /** A GB-month stored: logs kept past the first week, build caches. */
  stored_gb_month: number;
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
  | { kind: "upload" }
  /** A public Git repository by URL: each deploy fetches its branch's head. */
  | { kind: "git"; url: string; root?: string };

export interface Build {
  builder: "auto" | "dockerfile" | "functions";
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
  /** System packages the generated Dockerfile installs (apt, or apk on Alpine): `ffmpeg`, `chromium`. */
  packages?: string[];
  /** The longest its build may take, seconds (60–21600, held to the node's own limit); unset: the node's. */
  timeout_s?: number;
  /** Its builder: small (2 vCPU, 4 GiB; default), medium (4, 8 GiB) or large (8, 16 GiB), minutes metered by size on the fleet. */
  size?: "small" | "medium" | "large";
  /** Submodules fetched into the tree (default: left out): a Git URL's, or a roll code repo's. */
  submodules?: boolean;
  /** The private roll code repos (owner/name) a roll code repo's submodules may come from. */
  submodule_repos?: string[];
  /** The language's OpenTelemetry agent (Node, Python), started while traces are on. */
  otel?: boolean;
  /** A Git URL service's Git LFS objects in place of their pointers (default: pointers as text). */
  lfs?: boolean;
}

export type ServiceType = "web" | "worker" | "cron";

/** One of `run.push`: a topic of a stream (`st_…`, on the project) delivered to one environment's path. */
export interface Push {
  stream: string;
  topic: string;
  group: string;
  path: string;
  /** Default production. */
  environment?: string;
  /** A topic of the same stream a record goes to after `max_attempts` (default 5) failed tries. */
  dead_letter?: string;
  max_attempts?: number;
  timeout_s?: number;
  /** Records POSTed at once, each from its own partitions (1–32, default 4). */
  max_in_flight?: number;
  /** `oidc`: each POST carries roll's OIDC token for the service instead of `CRON_SECRET`. */
  auth?: "oidc";
}

/** A push rule as it stands (`…/environments/{env}/push`), on the service's owner since it began delivering. */
export interface PushState extends Push {
  /** Records its group hasn't committed, every partition. */
  lag?: number;
  /** Why nothing is delivered. */
  off?: string;
  delivered: number;
  dead_lettered: number;
  last_delivered_at?: number;
  failing_since?: number;
  last_error?: string;
}

/** An alert on one of the app's own gauges: above or below a threshold for 1–5 minutes running. */
export interface MetricAlert {
  name: string;
  above?: number;
  below?: number;
  minutes?: number;
  sum?: boolean;
}

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
  /** A cron service's failed run is tried again this many times (0–5). */
  retries?: number;
  min_instances: number;
  max_instances: number;
  concurrency: number;
  cpu: number;
  memory_mb: number;
  health_path?: string;
  health_timeout_s: number;
  /** Seconds between two health checks while running (2–300; unset 10). */
  health_every_s?: number;
  /** Failed checks in a row that replace an instance (1–10; unset 3). */
  health_failures?: number;
  /** A worker's health check: a shell command in its instance, healthy on exit 0. */
  health_command?: string;
  /** A replaced instance's time to finish: its requests, then SIGTERM, SIGKILL when it's up (1–300; unset 30). */
  drain_s?: number;
  /** A web service also scales on CPU: another instance while they average over this percent for a minute (10–100). */
  cpu_target?: number;
  /** 1–100: an instance over this percent of its CPU counts as busy and isn't put to sleep or scaled in. */
  busy_cpu?: number;
  /** A web service also scaled on its answers' p95 (50–60,000 ms). */
  latency_target_ms?: number;
  /** Audiences its instances may ask roll's OIDC issuer for, at ROLL_OIDC_URL (`?aud=`, the first by default). */
  oidc_audiences?: string[];
  /** And on memory: another while they average over this percent of their memory for a minute (10–100). */
  memory_target?: number;
  suspend_after_s: number;
  sleep_after_s: number;
  /** backups: how often (hours, 0: by hand only) and how many are kept; none: daily, seven. */
  volume?: { mount: string; size_gb: number; backups?: { every_hours: number; keep: number } };
  /** Metric alerts (`alert.*` events), over five minutes; each 0 is off. */
  /** log_match: text its output holds log_count times in five minutes (alert.log). */
  alerts?: { memory_pct: number; error_pct: number; p95_ms: number; capacity_pct?: number; log_match?: string; log_count?: number; cpu_pct?: number; metrics?: MetricAlert[] };
  /** Its service level objective: `objective`% of requests not 5xx (and of minutes under `latency_ms` at p95) over `window_days`; `alert.slo` when its budget burns fast. */
  slo?: Slo;
  /** Its own Prometheus endpoint, scraped every 15 s on each running instance (port: the app's own by default). */
  metrics?: { port?: number; path?: string };
  /** Counts of its own log lines as gauges (Cloud Logging's log-based metrics): each minute, the lines holding `pattern` (any case). At most ten. */
  log_metrics?: { name: string; pattern: string }[];
  /** A gradual rollout: the new deployment takes each percent of `steps` for `step_s`, rolled
   *  back by a 5xx share of `error_pct` or more (worse than the old one); no steps: at once. */
  rollout?: { steps: number[]; step_s: number; error_pct: number; /** Each step waits for a person once it passes. */ manual?: boolean; /** With run.slo: a step fails past this burn rate (default 14.4). */ slo_burn?: number; /** A step fails when the new deployment's p95 is this many percent above the old one's. */ p95_pct?: number; /** false: the old instances stop before the new ones start (two never run at once). */ overlap?: boolean };
  /** The app speaks HTTP/2 without TLS (a gRPC server). */
  h2c?: boolean;
  /** With h2c: the gRPC service whose grpc.health.v1 check must answer SERVING ("" the whole server). */
  health_grpc?: string;
  /** false: no public URL, only the project's private network (absent: public). */
  public?: boolean;
  /** A worker that stops: started again always (default), on failure (up to restart_tries in a row), or never. */
  restart?: "always" | "on_failure" | "never";
  restart_tries?: number;
  /** A web service keeps each visitor on one instance (a cookie) while it's up. */
  affinity?: boolean;
  /** Its port speaks TCP, not HTTP: each environment answers on a port of its own. */
  tcp?: boolean;
  /** Its port speaks UDP: a UDP port of its own per environment (one node). */
  udp?: boolean;
  /** A deployment that crash-loops within ten minutes of going live is replaced by the one before it. */
  rollback_on_crash?: boolean;
  /** Build the same revision again, without the cache, when one of its base images moves. */
  rebuild_on_base?: boolean;
  /** A web service's page loaded before a deploy keeps talking to the deployment that served it, for a day (`x-deployment-id`, `?dpl=`, or the `__roll_dpl` cookie). */
  skew_protection?: boolean;
  /** At max_instances with every instance at its concurrency, a request waits this long (1–60 s) for a slot, then gets a 429; unset, the least busy takes it past its concurrency. */
  queue_s?: number;
  /** A deploy whose image has a known vulnerability this severe or worse fails before it goes live (a rollback never does). */
  block_on?: Exclude<Severity, "unknown">;
  /** A worker follows `group`'s lag on `stream` (a roll stream of its project): one instance per `per_instance` behind. */
  /** A worker follows a stream group's lag, or a gauge of its own (`metric`: the largest series, or their `sum`). */
  scale?: { stream?: string; group?: string; metric?: string; sum?: boolean; per_instance?: number };
  /** Commands on a schedule, each a job of the live deployment at its due times (Heroku Scheduler); at most 20. */
  schedules?: Scheduled[];
  /** A web service's: a roll stream's records POSTed to `path` as they arrive, the group's offset committed on a 2xx (Cloud Run's Pub/Sub push); at most 4. */
  push?: Push[];
  /** Containers beside each instance, in its network namespace (a web service's or a worker's); at most 4. */
  sidecars?: Sidecar[];
}

/** A container beside each instance: `name` a-z, 0-9 and - (unique, at most 20); its image pinned with each deployment. */
export interface Sidecar {
  name: string;
  image: string;
  /** Run as `sh -c` instead of the image's own. */
  command?: string;
  /** Plain settings, shown as they are: never a secret. */
  env?: Record<string, string>;
  /** The environment's own variables it gets, by name (its secrets too). */
  variables?: string[];
  /** 0.1–2, default 0.25. */
  cpu?: number;
  /** 64–4096, default 128. */
  memory_mb?: number;
}

/** A command on a schedule: `name` a-z, 0-9 and - (unique), `schedule` five fields or `@daily`, `timezone` IANA (default UTC). */
export interface Scheduled {
  name: string;
  schedule: string;
  /** A command each due time, or (absent) `path`'s call. */
  command?: string;
  /** A web service's path GET'd each due time instead (Vercel's crons), with `Authorization: Bearer $CRON_SECRET`. */
  path?: string;
  timezone?: string;
  timeout_s?: number;
  /** `oidc`: a path's call carries roll's OIDC token for the service instead of `CRON_SECRET`. */
  auth?: "oidc";
}

export type InstanceState = "none" | "stopped" | "suspended" | "waking" | "running" | "crashed" | "deploying";

export type DeployStatus = "queued" | "building" | "deploying" | "active" | "inactive" | "failed" | "canceled" | "superseded" | "removed" | "ready";

export interface EnvironmentSummary {
  name: string;
  state: InstanceState;
  url: string | null;
  active: { id: string; revision: string | null; created_at: number } | null;
  latest: { id: string; status: DeployStatus } | null;
  /** What wants its owner's eye: a crash loop, the project capped by its spend limit, paused, maintenance, its latest deploy failed. */
  attention?: ("crash_loop" | "capped" | "paused" | "held" | "maintenance" | "deploy_failed")[];
  /** Settings, variables, links or the project's secrets changed since the live deployment: a redeploy applies them. */
  needs_deploy: boolean;
  /** A preview's (made for a branch or a pull request). */
  ephemeral: boolean;
}

/** A deleted service, kept stopped for a restore until `purge_at` (ms), then deleted for good. */
export interface DeletedService {
  id: string;
  project: string;
  name: string;
  deleted_at: number;
  purge_at: number;
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
  /** A private Git repository's login, bound to the source URL's host (the password is never returned). */
  git_login: { host: string; username: string; secret?: string } | null;
  /** Connected to roll's GitHub App: fetches use its installation's tokens, deploys post statuses. */
  github: { installation: number; repo: string } | null;
  /** What it links (the canvas draws them). */
  links?: { kind: LinkKind; ref: string }[];
  created_at: number;
  environments: EnvironmentSummary[];
}

/** A project in one file (`POST /v1/apps/import`): a roll.toml's `[apps]` and `[databases]`, or a Compose file as them. */
export interface ProjectImport {
  /** The file as roll.toml (a Compose file's, to keep). */
  roll_toml: string;
  /** What a Compose file's import left out, and why. */
  notes: string[];
  apps: Record<
    string,
    {
      source: { kind: "repo"; repo: string; root: string | null } | { kind: "git"; url: string; root: string | null } | { kind: "image"; image: string } | { kind: "upload"; root: string };
      branch: string | null;
      run: Partial<Run>;
      /** A database's kind, or `service`: another app of the file, by its name. */
      links: { kind: "postgres" | "redis" | "bucket" | "stream" | "nosql" | "search" | "service"; name: string }[];
      /** What it's set to as it is. */
      variables: Record<string, string>;
      /** Made once, when the service is (`{ generate = "secret" }`): 32 random bytes in hex. */
      generate: string[];
      /** Asked of whoever makes it (`{ ask = "…" }`), its default offered. */
      ask: { name: string; ask: string; default: string | null }[];
      /** Its other environments (`[apps.<name>.environments.<env>]`). */
      environments?: Record<string, { branch: string | null; run: Partial<Record<EnvRunField, number>>; variables: Record<string, string> }>;
    }
  >;
  databases: Record<string, { kind: "postgres" | "redis" | "bucket" | "stream" | "nosql" | "search" }>;
}

export interface NewService {
  project: string;
  name: string;
  source: Source;
  build?: Partial<Build>;
  run?: Partial<Run>;
  /** Production's variables, set before its first deploy. */
  variables?: Record<string, string>;
  /** Which of those are sealed from the start: never shown again. */
  sealed?: string[];
  /** An image source's private registry login, before its first pull. */
  registry?: { username: string; password: string; server?: string };
  /** A Git URL source's private repository login, before its branch is checked. */
  git_login?: { username: string; password: string };
  /** Resources to link before the first deploy (their variables are in it). */
  links?: { kind: LinkKind; ref: string; prefix?: string }[];
  /** A repo source: production follows it (default: the repo's default branch). */
  branch?: string;
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
  /** `limit`: let in at `per_minute` a client, a 429 past it. */
  action: "allow" | "deny" | "limit" | "challenge";
  per_minute?: number;
  burst?: number;
  /** A path, or a prefix ending in `*`. */
  path?: string;
  methods?: string[];
  /** Text the User-Agent contains. */
  user_agent?: string;
  /** Clients by address: addresses or networks (`203.0.113.0/24`). */
  ips?: string[];
  /** A request header, cookie or query parameter there (and holding `contains`, case aside). */
  header?: FieldMatch;
  cookie?: FieldMatch;
  query?: FieldMatch;
}

export interface FieldMatch {
  name: string;
  contains?: string;
}

/** Firewall rules as typed, one a line: `deny /admin/*`, `deny method=DELETE,PUT`, `allow /admin/health`, `deny ip=203.0.113.0/24`, `deny header=X-Bot`, `deny query=debug:1`, `deny ua=BadBot` (`ua=` last: the rest of the line). */
export function parseFirewall(text: string): FirewallRule[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith("#"))
    .map((l) => {
      const [action, ...words] = l.split(/\s+/);
      const rule: FirewallRule = { action: action === "allow" ? "allow" : action === "limit" ? "limit" : action === "challenge" ? "challenge" : "deny" };
      let rest = words;
      // `limit N[:BURST] …`
      if (rule.action === "limit") {
        const [n, b] = (rest[0] ?? "").split(":");
        rule.per_minute = Number(n) || 60;
        if (b) rule.burst = Number(b) || undefined;
        rest = rest.slice(1);
      }
      for (let i = 0; i < rest.length; i++) {
        const t = rest[i];
        if (t.startsWith("ua=")) {
          rule.user_agent = [t.slice(3), ...rest.slice(i + 1)].join(" ");
          break;
        }
        // `name[:contains]`: there, or there and holding that.
        const field = (v: string) => {
          const at = v.indexOf(":");
          return at < 0 ? { name: v } : { name: v.slice(0, at), contains: v.slice(at + 1) };
        };
        if (t.startsWith("method=")) rule.methods = t.slice(7).split(",").filter(Boolean);
        else if (t.startsWith("ip=")) rule.ips = t.slice(3).split(",").filter(Boolean);
        else if (t.startsWith("header=")) rule.header = field(t.slice(7));
        else if (t.startsWith("cookie=")) rule.cookie = field(t.slice(7));
        else if (t.startsWith("query=")) rule.query = field(t.slice(6));
        else if (t.startsWith("/")) rule.path = t;
      }
      return rule;
    });
}

/** The rules as one line each, as `parseFirewall` reads them. */
export function formatFirewall(rules: FirewallRule[]): string {
  return rules
    .map((r) => {
      const field = (k: string, f?: FieldMatch) => (f ? `${k}=${f.name}${f.contains ? `:${f.contains}` : ""}` : undefined);
      return [
        r.action,
        r.action === "limit" ? `${r.per_minute ?? 60}${r.burst ? `:${r.burst}` : ""}` : undefined,
        r.path,
        r.methods?.length ? `method=${r.methods.join(",")}` : undefined,
        r.ips?.length ? `ip=${r.ips.join(",")}` : undefined,
        field("header", r.header),
        field("cookie", r.cookie),
        field("query", r.query),
        // Last: it takes the rest of the line.
        r.user_agent ? `ua=${r.user_agent}` : undefined,
      ]
        .filter(Boolean)
        .join(" ");
    })
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
/** A path (a prefix ends in `*`) another service of the project answers. */
export interface RouteRule {
  path: string;
  service: string;
}

/** Routes as lines, `/api/* api`. */
export function parseRoutes(text: string): RouteRule[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l) => {
      const [path, service = ""] = l.split(/\s+/);
      return { path, service };
    });
}

export function formatRoutes(rules: RouteRule[]): string {
  return rules.map((r) => `${r.path} ${r.service}`).join("\n");
}

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

/** How an environment differs from another (`GET …/diff?from=`): never a value. */
export interface EnvironmentDiff {
  from: string;
  to: string;
  variables: {
    /** `from`'s own variables this one lacks (a sealed one can't be copied). */
    missing: { name: string; sealed: boolean }[];
    extra: string[];
    /** Set in both with other values; null: the caller can't reveal both, so isn't told. */
    differ: string[] | null;
  };
  /** Each setting that differs, and the `PUT …/environments/{to}` body that makes it `from`'s. */
  settings: { field: string; from: unknown; to: unknown; put: EnvironmentPut }[];
}

export interface EnvironmentPut {
  /** Data of its own: a branch of each Postgres and Redis database it links (admin); null goes back. */
  data?: "branch" | null;
  branch?: string | null;
  /** Its name on the roll domain (`<hostname>.<domain>`): the old one stops answering. */
  hostname?: string;
  ephemeral?: boolean;
  /** Private: visitors need `?roll_access=<token>` once; null makes it public again. */
  /** `signed_in`: people signed in to roll who can open the project get in (alone, or with a token). */
  /** Who gets in: a link's token, people signed in to roll, or a password visitors type (8–200; replaces the whole of access). */
  access?: { token?: string; signed_in?: boolean; password?: string; bypass_sha256?: string } | null;
  /** Creating it: start with this environment's variables. */
  variables_from?: string;
  wait_for_ci?: boolean;
  /** Deploy when the image's tag moves (an image service with a tag). */
  follow_tag?: boolean;
  /** Redeploy (same image, new values) when the project's secrets change. */
  redeploy_on_secrets?: boolean;
  /** Its instances send OpenTelemetry traces to roll (`OTEL_*` from the next deploy). */
  traces?: boolean;
  /** Its pages measure their Web Vitals (a tag roll adds). */
  vitals?: boolean;
  /** How long its logs are kept (null: 7 days). */
  log_retention_days?: 7 | 30 | 90 | null;
  /** Approvals a deployment needs before it goes live (0 or null: none). */
  approvals?: number | null;
  /** A deploy status badge anyone may load. */
  badge?: boolean;
  /** Its deploys post commit statuses and GitHub Deployments (false: someone else reports them). */
  report?: boolean;
  /** What its code may reach from the next deploy: roll and `hosts` only, or anything. null: open. */
  egress?: Egress | null;
  /** Paused (Render's Suspend): nothing runs, wakes, deploys or is scheduled until false. */
  paused?: boolean;
  /** `false` resumes deploys held after a rollback. */
  held?: boolean;
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
  /** Paths another service of the project answers (its environment of the same name); null or [] for none. */
  routes?: RouteRule[] | null;
  /** Its uptime checks (at most 5), or null for none. */
  checks?: UptimeCheck[] | null;
  /** Its deploy freeze: on until lifted (null), until `until`, or in weekly `windows`; a person's deploy then needs the header `X-Roll-Override: 1` (admin). */
  freeze?: Freeze | null;
  /** Paused by the clock: as a window starts it's paused, as it ends resumed; null removes it. */
  pause_schedule?: { windows: { from: string; to: string }[]; timezone?: string } | null;
  /** Traffic mirroring: copies of `percent` of GET and HEAD requests to `environment`, answers dropped; null ends it. */
  mirror?: { environment: string; percent: number } | null;
  /** Its own HTML in every page: before `</head>` or `</body>` (at most 5, each up to 16 KiB); null: none. */
  snippets?: Snippet[] | null;
  /** Runtime config its instances read at `ROLL_CONFIG_URL` without a deploy (at most 200 keys, 64 KiB); null: none. */
  config?: Record<string, unknown> | null;
  /** Its split test: `percent` of visitors (1–99) served by `environment`, or null to end it. */
  split?: { environment: string; percent: number } | null;
  /** Its heartbeats (at most 10), or null for none; one kept by name keeps its URL. */
  heartbeats?: Pick<Heartbeat, "name" | "every_s" | "grace_s">[] | null;
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
  /** The app's port it's sent to, when it isn't the service's own (published from the next deploy). */
  port?: number;
}

/** What an environment's code may reach: `roll` keeps it on a network with no route out, the
 *  listed hosts (on 443 and 80) through its egress proxy. */
export interface Egress {
  mode: "roll" | "open";
  hosts?: string[];
}

export interface Environment extends EnvironmentSummary {
  service: string;
  branch: string | null;
  instances: Instance[];
  domains: Domain[];
  port: number | null;
  /** A TCP service's address (`host:port`), once it has its port. */
  tcp?: string | null;
  /** A UDP service's address (host:port). */
  udp?: string;
  /** Its hostnames answer only visitors with its access token (a preview). */
  private: boolean;
  /** A cron service's next scheduled run, once deployed. */
  next_run_at: number | null;
  /** How full its volume was when last measured (hourly), when it has one. */
  volume_used?: { bytes: number; at: number };
  /** Its schedules as they stand: when each is next due, and the job it last started. */
  schedules?: { name: string; next_run_at: number | null; last: { job: string; status: Job["status"]; started_at: number } | null; last_call?: { at: number; status?: number; ms: number; error?: string } }[];
  /** Its instances keep crashing: three or more in the last ten minutes. */
  crash_loop: { crashes: number; last_at: number; last_error: string } | null;
  /** A gradual rollout under way: the new deployment and the percent of requests it takes. */
  canary?: { deployment: string; share: number; /** Its step passed and it waits for a person (`rollout.manual`). */ waiting?: boolean } | null;
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
  /** Its instances send OpenTelemetry traces to roll. */
  traces?: boolean;
  /** Its pages measure their Web Vitals for roll. */
  vitals?: boolean;
  /** Days its logs are kept: 7 (default), 30 or 90; a preview's are 7. */
  log_retention_days?: number;
  /** People other than a deployment's author who approve it before it goes live (0–3). */
  approvals?: number;
  /** Its deploy status badge (an SVG for a README), when it has one. */
  badge_url?: string;
  /** Its deploys are reported to the Git host (commit statuses, GitHub Deployments); false:
   *  someone else reports them (roll previews' `pv-` environments). */
  report?: boolean;
  /** People signed in to roll who can open its project get in. */
  signed_in?: boolean;
  /** Visitors may type its password (`access.password`, never returned). */
  password?: boolean;
  /** What its code may reach: roll and the listed hosts (`roll`), or anything (`open`). */
  egress?: Egress;
  /** Hosts its egress proxy refused lately: not listed, or (`private`) resolving only to private addresses. */
  egress_refused?: { host: string; private?: true }[];
  /** Who a closed environment's placeholders are made by; null: no one (its bound keys withheld). */
  closed_by?: string | null;
  /** Data of its own: a branch of each Postgres and Redis database it links. */
  data?: { mode: "branch"; branches: { kind: "postgres" | "redis"; ref: string }[] };
  /** A closed environment's interception (roll secrets' placeholders): requests filtered, placeholders swapped, the latest refusals. */
  intercepted?: { requests: number; swapped: number; refused?: string[] };
  /** A push deploys only once its commit's checks pass. */
  wait_for_ci?: boolean;
  /** The push (sha) waiting for its checks, if one is. */
  waiting_for_ci?: string | null;
  /** `egress` roll: its builds get no secret values. */
  build_closed?: boolean;
  /** Base images the live deployment was built on that have moved since in their registries. */
  base_updated?: string[];
  /** Paused: nothing runs, wakes, deploys or is scheduled. Absent: not paused. */
  paused?: boolean;
  /** Deploys are held after this rollback: pushes, hooks and tag moves aren't put live until a person deploys or resumes them. */
  held_after?: string;
  /** A person holds its deploys: pushes, hooks and tag moves build to `ready` for them to put live. */
  held?: boolean;
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
  /** Paths another service of the project answers. */
  routes?: RouteRule[];
  checks?: UptimeCheck[];
  /** How each check stands on its owner. */
  check_states?: UptimeCheckState[];
  heartbeats?: Heartbeat[];
  heartbeat_states?: HeartbeatState[];
  /** When it's paused by the clock (weekly windows). */
  pause_schedule?: { windows: { from: string; to: string }[]; timezone?: string };
  /** Where a share of its GET and HEAD requests is copied, answers dropped. */
  mirror?: { environment: string; percent: number };
  snippets?: Snippet[];
  /** Its runtime config, and the version its instances read. */
  config?: Record<string, unknown>;
  config_version?: number;
  /** When its config's token was last rotated: the address before it reads until a deploy made after it is live. */
  config_rotated_at?: number;
  freeze?: Freeze;
  /** Its freeze is on now: automatic deploys build to ready, a person's deploy needs the `X-Roll-Override: 1` header (admin). */
  frozen?: boolean;
  /** Its split test: a share of its visitors, kept by their bucket cookie, served by another environment. */
  split?: { environment: string; percent: number };
  /** Only in the answer to the PUT that added them: each new heartbeat's ping URL (shown once). */
  heartbeat_urls?: Record<string, string>;
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
  provider: "dockerfile" | "deno" | "node" | "python" | "go" | "rust" | "ruby" | "php" | "java" | "dotnet" | "elixir" | "scala" | "clojure" | "gleam" | "shell" | "static" | "functions";
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
  /** A static site's function directories (`netlify/functions`, `api`) the `functions` builder runs. */
  functions?: string[];
  /** A static site's edge functions: run before its files on their paths by its `-fn` service. */
  edge?: { name: string; kind: "netlify" | "vercel"; file: string; paths: string[]; excluded?: string[] }[];
}

export interface RootCandidate {
  root: string;
  provider: string;
  framework?: string;
}

export type PlanRequest = { repo: string; revision?: string; root?: string } | { git: string; branch?: string; root?: string; git_login?: { username: string; password: string } } | { files: Record<string, string> };

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
  /** How to fix a failure roll knows (a private image's login, more memory, a lockfile): the setting and its command. */
  fix?: string;
  /** Pruned (older than its environment keeps): history only, its image gone, so it can't be rolled back to. */
  pruned_at?: number;
  /** When each step began (`build`, `pre_deploy`, `rollout`, then `live` or `failed`): on a single deployment's GET. */
  steps?: { step: string; at: number }[];
  /** Its image's last scan for known vulnerabilities, once one ran. */
  vulnerabilities?: ScanSummary;
  /** Who approved putting it live, where its environment asks for approvals (single GET only). */
  approved_by?: string[];
}

export interface Slo {
  objective: number;
  latency_ms?: number;
  window_days?: number;
}

/** One indicator over the objective's window: `budget_left` is the share of the budget still unspent (negative once spent). */
export interface SloIndicator {
  total: number;
  bad: number;
  sli: number | null;
  budget_left: number | null;
}

/** `…/slo`: the live objective against its window, and the burn alert firing now. */
export interface SloReport {
  slo: Slo | null;
  availability?: SloIndicator;
  latency?: SloIndicator | null;
  burning?: { indicator: "availability" | "latency"; burn_rate: number; window: "1h" | "6h" } | null;
}

export type Severity = "critical" | "high" | "medium" | "low" | "unknown";

/** A deployment's scan: counts by severity (`error`: it didn't finish, and the counts mean nothing). */
export interface ScanSummary {
  critical: number;
  high: number;
  medium: number;
  low: number;
  unknown: number;
  fixable: number;
  scanned_at: number;
  scanner: string;
  error?: string;
  /** Its SBOM (CycloneDX) is kept: `…/sbom`. */
  sbom?: boolean;
}

/** One row of a project's month: a service's environment, its previews (`previews`), or deleted services (`deleted`). */
export interface UsageRow {
  service: string | null;
  name: string | null;
  environment: string | null;
  previews?: boolean;
  deleted?: boolean;
  usd: number;
  cpu_seconds: number;
  gb_seconds: number;
  requests: number;
  egress_bytes: number;
  build_seconds: number;
}

/** A project's month (`…/projects/{project}/usage`). */
export interface ProjectUsage {
  month: string;
  total_usd: number;
  /** This month at the pace so far (the current month only). */
  projected_usd: number | null;
  rows: UsageRow[];
}

/** Why a failed deployment failed, from the repo owner's model (`…/explain`). The model's words: shown as text. */
export interface Explanation {
  summary: string;
  cause: string;
  /** What to change, in order (at most 5; none when it isn't the app's fault). */
  steps: string[];
  confidence: "high" | "medium" | "low";
  model: string;
  at: number;
  /** Older log lines it didn't see. */
  lines_left_out: number;
}

/** One known vulnerability: a package at a version an advisory names. */
export interface Vulnerability {
  id: string;
  severity: Severity;
  package: string;
  installed: string;
  fixed?: string;
  title?: string;
  /** Where in the image: the OS's packages or a lockfile. */
  target: string;
}

/**
 * `search`: a roll search cluster, an API key of its own per link (`ELASTICSEARCH_URL`, `ELASTICSEARCH_API_KEY`; an
 * OpenSearch-flavor cluster's `OPENSEARCH_URL`, `OPENSEARCH_USERNAME`, `OPENSEARCH_PASSWORD`). `service`: another web service of the project, its `<NAME>_URL`
 * (private) and `<NAME>_PUBLIC_URL`.
 */
export type LinkKind = "postgres" | "redis" | "bucket" | "stream" | "queue" | "nosql" | "search" | "service";

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
  vars: { name: string; masked: string; source: VariableSource; reference?: string; sealed?: boolean; /** Kept as a file at this path in its instances (not in their environment). */ file?: string; /** Only its instances get it, never a build's steps. */ runtime_only?: boolean }[];
}

export interface LogLine {
  t: number;
  /** `http`: the proxy's access log, its text JSON (`AccessLine`). */
  stream: "stdout" | "stderr" | "build" | "system" | "http";
  instance: string | null;
  deployment: string | null;
  text: string;
  /** The level the app's line says (JSON's `level`/`severity`, logfmt's `level=`, a leading `ERROR`). */
  level?: LogLevel;
  /** Part of a stack trace: `start` on its first line, `more` on the rest from its instance and stream. */
  trace?: "start" | "more";
}

export type LogLevel = "trace" | "debug" | "info" | "warn" | "error" | "fatal";
export const LOG_LEVELS: LogLevel[] = ["trace", "debug", "info", "warn", "error", "fatal"];
/** At `min` or worse: a line that says no level isn't. */
export const atLeast = (l: LogLine, min: LogLevel | "") => !min || (!!l.level && LOG_LEVELS.indexOf(l.level) >= LOG_LEVELS.indexOf(min));

/** One route's last minutes, from the access log: ids folded (`/users/:id`), no query. */
export interface RouteStat {
  method: string;
  route: string;
  requests: number;
  /** 5xx */
  errors: number;
  /** 4xx */
  client_errors: number;
  p50_ms: number;
  p95_ms: number;
  max_ms: number;
}

/** One deployment's or instance's requests in a `RouteStats` window. */
export interface SplitStat {
  deployment?: string;
  instance?: string;
  requests: number;
  errors: number;
  client_errors: number;
  p50_ms: number;
  p95_ms: number;
  max_ms: number;
}

export interface RouteStats {
  minutes: number;
  requests: number;
  /** Read only the newest 50,000 requests: the counts are theirs. */
  cut: boolean;
  /** The busiest, the most 5xx and the slowest (5 requests or more), each at most 50. */
  routes: RouteStat[];
  /** The same requests by the deployment that answered (a canary beside the live one), the busiest first. */
  deployments?: SplitStat[];
  /** And by the instance that answered. */
  instances?: SplitStat[];
}

/** A trace's root span, its duration and its spans' count. */
export interface TraceSummary {
  trace: string;
  name: string;
  service: string;
  start_ms: number;
  ms: number;
  spans: number;
  error: boolean;
}

/** A span: ids in hex, times in Unix nanoseconds. */
export interface Span {
  trace: string;
  span: string;
  parent?: string;
  name: string;
  service?: string;
  start: number;
  end: number;
  error?: boolean;
  attrs?: [string, string][];
}

/** Visitors and page views from the access log (no script, no cookie). */
/** An environment's errors, grouped from its log: a trace joined from its lines, or an error line. */
export interface ErrorGroup {
  fingerprint: string;
  kind: "python" | "java" | "node" | "go" | "ruby" | "php" | "dotnet" | "rust" | "elixir" | "line";
  title: string;
  count: number;
  first_seen: number;
  last_seen: number;
  instances: number;
  /** The newest one, whole (at most 8 KiB). */
  sample: string;
  /** Open; resolved (said again, `error.regressed`, if it comes back); muted (never said). */
  state?: "open" | "resolved" | "muted";
  /** The deployment it was first seen in. */
  first_deployment?: string | null;
}

/** One series of the app's own metrics (OpenTelemetry, traces on): a name and its attributes. */
export interface AppSeries {
  name: string;
  kind: "gauge" | "sum" | "histogram";
  unit: string;
  attrs: { key: string; value: string }[];
  /** What the points are: a gauge's value, a counter's rate a second, a histogram's mean. */
  shows: "value" | "per_second" | "mean";
  /** [unix ms, value] a bucket. */
  points: [number, number][];
}
export interface AppMetrics {
  minutes: number;
  step_minutes: number;
  series: AppSeries[];
  cut: boolean;
}

/** A file `POST /v1/apps/import` reads: roll's own, Compose, or another platform's (Heroku's
 *  app.json with its Procfile's text, if it has one). */
export type ImportFile = { roll_toml: string } | { compose: string } | { render: string } | { fly: string } | { railway: string } | { heroku: string; procfile?: string };

/** A Web Vital's 75th percentile, rated by web.dev's thresholds. */
export interface Vital {
  p75: number;
  rating: "good" | "needs-improvement" | "poor";
  samples: number;
}
export type VitalName = "lcp" | "inp" | "cls" | "fcp" | "ttfb";
/** An uptime check: GET `path` every `every_s` (30–3600, default 60) through the environment's own proxy; passes on `status` (any 2xx/3xx when none) and, with `contains`, an answer holding it. */
export interface UptimeCheck {
  name: string;
  path: string;
  every_s?: number;
  timeout_s?: number;
  status?: number;
  contains?: string;
}

export interface UptimeCheckState {
  name: string;
  /** null: not tried yet; false once it failed twice in a row. */
  up: boolean | null;
  failures: number;
  last_at: number | null;
  status: number | null;
  ms: number | null;
  error: string | null;
  down_since: number | null;
}

/** A heartbeat: something outside roll pings its URL at least every `every_s` (60–604800); one not come within that and `grace_s` (default 300) is `heartbeat.missed`. */
/** A deploy freeze: `until` (unix ms), weekly `windows` (`{ from: "fri 17:00", to: "mon 09:00" }`) in `timezone`, or always. */
export interface Freeze {
  until?: number;
  windows?: { from: string; to: string }[];
  timezone?: string;
  reason?: string;
}

/** A project's public status page: its checks and heartbeats (all, or `items`), at `url`. */
export interface StatusPage {
  title: string;
  url: string;
  items: { service: string; environment?: string; check?: string; heartbeat?: string }[];
}

export interface Heartbeat {
  name: string;
  every_s: number;
  grace_s?: number;
  added_at: number;
}

export interface HeartbeatState {
  name: string;
  last_ping_at: number | null;
  /** When the next ping is due (before the grace). */
  due_at: number;
  missed: boolean;
}

/** What visitors' browsers measured (`vitals: true`): overall and for the busiest routes. */
export interface Vitals {
  minutes: number;
  samples: number;
  metrics: Partial<Record<VitalName, Vital>>;
  routes: { route: string; samples: number; metrics: Partial<Record<VitalName, Vital>> }[];
  cut: boolean;
}

export interface Analytics {
  minutes: number;
  /** Minutes a point of `series` stands for. */
  step: number;
  page_views: number;
  /** An address and browser a day, counted for this answer only. */
  visitors: number;
  pages: { path: string; views: number; visitors: number }[];
  /** Referring sites' hosts (never the site's own). */
  referrers: { host: string; views: number }[];
  /** The edge's country codes (Cloudflare's), where it said one. */
  countries: { country: string; views: number }[];
  /** Page views that came with UTM parameters (empty strings where one wasn't given). */
  campaigns?: { source: string; medium: string; campaign: string; views: number; visitors: number }[];
  /** From the user agent: browsers, operating systems, and `mobile`/`tablet`/`desktop`/`bot`. */
  devices?: { browsers: { browser: string; views: number }[]; os: { os: string; views: number }[]; classes: { class: string; views: number }[] };
  /** Custom events (`roll.track(name, props)` or `POST /__roll/event`), by name, each property's most common values. */
  events?: { name: string; count: number; props: Record<string, { value: string; count: number }[]> }[];
  series: { t: number; views: number; visitors: number }[];
  cut: boolean;
}

export interface Metrics {
  /** Minutes a point stands for: 1, or 60 past a week. */
  step?: number;
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
  /** Bytes answered a minute (what egress is billed by). */
  egress_bytes: number[];
  /** Requests a minute that found every instance busy at `max_instances` (saturation). */
  over_capacity: number[];
  /** Each instance's own last hour by minute (those it has now), whatever the range. */
  by_instance?: InstanceSeries[];
}

/** One instance's last hour (Railway's per replica): cores in use on average each minute, and
 *  the minute's peak memory. */
export interface InstanceSeries {
  instance: string;
  deployment: string;
  minutes: number[];
  cpu: number[];
  memory_bytes: number[];
}

export interface Usage {
  /** `day` is a unix day (days since 1970-01-01, UTC); every day of the window is there. */
  days: { day: number; cpu_seconds: number; gb_seconds: number; requests: number; egress_bytes: number; build_seconds: number; stored_gb_hours: number; usd: number }[];
  total_usd: number;
}

/** Where a project's events go: a signed webhook, a Slack or a Discord incoming webhook. */
export interface NotifyTarget {
  id: string;
  project: string;
  /** `email`: the project's owner, at their account's address (never an address someone names). */
  kind: "webhook" | "slack" | "discord" | "teams" | "googlechat" | "email" | "pagerduty" | "opsgenie";
  /** The URL's host (the URL itself isn't returned); an email target's says who it reaches. */
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
  | "deploy.base_updated"
  | "rollout.waiting"
  | "deploy.ready"
  | "crash_loop"
  | "deploy.active"
  | "spend.alert"
  | "spend.limit"
  | "alert.memory"
  | "alert.cpu"
  | "alert.metric"
  | "alert.errors"
  | "alert.latency"
  | "alert.capacity"
  | "alert.log"
  | "error.new"
  | "error.regressed"
  | "cron.failed"
  | "job.failed"
  | "domain.active"
  | "domain.expiring"
  | "domain.expired"
  | "domain.tls_failed"
  | "volume.backup_failed"
  | "volume.full"
  | "check.down"
  | "heartbeat.missed"
  | "deploy.vulnerable"
  | "alert.slo"
  /** Every `audit.*` entry: who changed what (never paged). */
  | "audit";

/** HTML of an environment's own the proxy puts in every page (Netlify's snippet injection). */
export interface Snippet {
  at: "head" | "body_end";
  html: string;
}

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
export type DrainSource = "runtime" | "http" | "build" | "audit" | "traces" | "metrics";

export interface DrainSettings {
  sources?: DrainSource[];
  environments?: string[];
  services?: string[];
  previews?: boolean;
  /** `{name: value}`: a vendor's key (DD-API-KEY, Authorization); sealed. */
  headers?: Record<string, string>;
}

export interface LogDrain {
  id: string;
  project: string;
  host: string;
  created_at: number;
  /** What it's sent: the instances' output, the requests (a line each), the builds. */
  sources: DrainSource[];
  /** Only these environments' (empty: every one). */
  environments: string[];
  /** Only these services' (ids; empty: every one). */
  services: string[];
  /** Preview environments' too, and a preview's services on roll's own runtime (in its `pv-`
   *  environment, with no service id, so never to a drain narrowed to services). */
  previews: boolean;
  /** The names of the headers it's sent with; their values are sealed, never shown. */
  headers: string[];
  /** How its lines go: roll's signed NDJSON, OpenTelemetry logs (OTLP/HTTP JSON), syslog (RFC 5424 over TLS), or NDJSON objects in an S3 bucket. */
  format: DrainFormat;
  /** Only in the answer that made it. */
  secret?: string;
}

/** A static site's form that has kept submissions. */
export interface SiteForm {
  name: string;
  submissions: number;
  last_at: number;
}

/** What someone sent through a form: its fields as they came. */
export interface FormSubmission {
  id: string;
  created_at: number;
  fields: { name: string; value: string }[];
}

export type DrainFormat = "ndjson" | "otlp" | "syslog" | "s3";

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
  /** Which try of its due time (1: the first; `run.retries` more may follow a failure). */
  attempt: number;
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
  /** Tries after a task's failure (0–5), each with ROLL_TASK_ATTEMPT. */
  retries?: number;
  /** Tasks running at once. */
  parallelism?: number;
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
  /** The folders of a repo's or a Git URL's tree that build on their own (a monorepo's apps). */
  roots: (req: { repo: string } | { git: string; branch?: string; git_login?: { username: string; password: string } }) => call<{ roots: RootCandidate[] }>("POST", "/v1/apps/plan", { ...req, roots: true }),
  /** The project as roll.toml: `databases` names what its services link (`{"postgres:<id>": "db"}`). */
  /** The project's own preview domain (a wildcard its preview environments answer under). */
  statusPage: (project: string) => call<StatusPage>("GET", `/v1/apps/projects/${enc(project)}/status-page`),
  putStatusPage: (project: string, body: { title?: string; items?: StatusPage["items"] }) => call<StatusPage>("PUT", `/v1/apps/projects/${enc(project)}/status-page`, body),
  postIncident: (project: string, body: { title: string; state?: string; text: string }) => call<{ id: string }>("POST", `/v1/apps/projects/${enc(project)}/status-page/incidents`, body),
  deleteStatusPage: (project: string) => call<void>("DELETE", `/v1/apps/projects/${enc(project)}/status-page`),
  previewDomain: (project: string) => call<Domain>("GET", `/v1/apps/projects/${enc(project)}/preview-domain`),
  setPreviewDomain: (project: string, hostname: string) => call<Domain>("POST", `/v1/apps/projects/${enc(project)}/preview-domain`, { hostname }),
  verifyPreviewDomain: (project: string) => call<Domain>("POST", `/v1/apps/projects/${enc(project)}/preview-domain/verify`),
  removePreviewDomain: (project: string) => call<null>("DELETE", `/v1/apps/projects/${enc(project)}/preview-domain`),
  exportProject: (project: string, databases: Record<string, string>) => call<{ roll_toml: string; notes: string[] }>("POST", `/v1/apps/projects/${enc(project)}/export`, { databases }),
  /** A roll.toml or Compose file read as a project (nothing made). */
  importProject: (file: ImportFile | { repo: string; revision?: string } | { git: string; branch?: string }) => call<ProjectImport>("POST", "/v1/apps/import", file),

  services: (project?: string) => call<Service[]>("GET", `/v1/apps/services${qs({ project })}`),
  /** Makes the service and places its card; a repo or an image source deploys to production at once. */
  create: (body: NewService) => call<Service>("POST", "/v1/apps/services", body),
  service: (id: string) => call<Service>("GET", svc(id)),
  update: (id: string, patch: ServicePatch) => call<Service>("PATCH", svc(id), patch),
  /** Deletes it until it's restored (48 hours); a deleted service's delete deletes it for good. */
  remove: (id: string) => call<null>("DELETE", svc(id)),
  deletedServices: (project?: string) => call<DeletedService[]>("GET", `/v1/apps/services${qs({ project, deleted: "true" })}`),
  restoreService: (id: string) => call<Service>("POST", `${svc(id)}/restore`),

  links: (id: string) => call<Link[]>("GET", `${svc(id)}/links`),
  /** `all`: every kind of event (its timeline), not only what people changed. */
  activity: (id: string, all = false) => call<AppEvent[]>("GET", `${svc(id)}/activity${qs({ all: all || undefined })}`),
  link: (id: string, kind: LinkKind, ref: string, prefix?: string) =>
    call<Link>("PUT", `${svc(id)}/links/${kind}/${enc(ref)}`, prefix ? { prefix } : {}),
  unlink: (id: string, kind: LinkKind, ref: string) => call<null>("DELETE", `${svc(id)}/links/${kind}/${enc(ref)}`),

  environments: (id: string) => call<Environment[]>("GET", `${svc(id)}/environments`),
  environment: (id: string, e: string) => call<Environment>("GET", env(id, e)),
  notifyTargets: (project: string) => call<NotifyTarget[]>("GET", `/v1/apps/projects/${enc(project)}/notifications`),
  drains: (project: string) => call<LogDrain[]>("GET", `/v1/apps/projects/${enc(project)}/drains`),
  /** A drain to a URL, or (`bucket`) into a roll storage bucket on the project, an `s3` drain with a key roll mints for it. */
  addDrain: (project: string, body: ({ url: string; format?: DrainFormat } | { bucket: string; prefix?: string }) & DrainSettings) => call<LogDrain>("POST", `/v1/apps/projects/${enc(project)}/drains`, body),
  /** Its sources, filters and headers (given ones replace; `headers` replaces them all); its URL and secret stay. */
  updateDrain: (project: string, id: string, body: DrainSettings) => call<LogDrain>("PATCH", `/v1/apps/projects/${enc(project)}/drains/${enc(id)}`, body),
  testDrain: (project: string, id: string) => call<{ ok: boolean; code: number | null; error: string | null }>("POST", `/v1/apps/projects/${enc(project)}/drains/${enc(id)}/test`),
  /** A static site's forms with submissions kept. */
  forms: (id: string, e: string) => call<SiteForm[]>("GET", `${env(id, e)}/forms`),
  /** A form's submissions in the clear (a reveal), the newest first. */
  formSubmissions: (id: string, e: string, form: string, before?: string) => call<FormSubmission[]>("GET", `${env(id, e)}/forms/${encodeURIComponent(form)}/submissions${before ? `?before=${encodeURIComponent(before)}` : ""}`),
  deleteFormSubmission: (id: string, e: string, form: string, sub: string) => call<null>("DELETE", `${env(id, e)}/forms/${encodeURIComponent(form)}/submissions/${encodeURIComponent(sub)}`),
  removeDrain: (project: string, id: string) => call<null>("DELETE", `/v1/apps/projects/${enc(project)}/drains/${enc(id)}`),
  limits: (project: string) => call<SpendLimits>("GET", `/v1/apps/projects/${enc(project)}/limits`),
  /** A month's use by service and environment (this one by default), most costly first. */
  projectUsage: (project: string, month?: string) => call<ProjectUsage>("GET", `/v1/apps/projects/${enc(project)}/usage${qs({ month })}`),
  setLimits: (project: string, body: { alert_usd: number | null; hard_usd: number | null }) => call<SpendLimits>("PUT", `/v1/apps/projects/${enc(project)}/limits`, body),
  /** PagerDuty takes its integration's `routing_key`, Opsgenie its `api_key` (sealed, never returned), each with a `region`. */
  addNotifyTarget: (project: string, body: { kind: NotifyTarget["kind"]; url?: string; routing_key?: string; api_key?: string; region?: "us" | "eu"; events?: NotifyEvent[]; environments?: string[]; services?: string[]; previews?: boolean }) => call<NotifyTarget>("POST", `/v1/apps/projects/${enc(project)}/notifications`, body),
  /** Its events and filters (given ones replace); its URL and secret stay. */
  updateNotifyTarget: (project: string, id: string, body: { events?: NotifyEvent[]; environments?: string[]; services?: string[]; previews?: boolean }) =>
    call<NotifyTarget>("PATCH", `/v1/apps/projects/${enc(project)}/notifications/${enc(id)}`, body),
  removeNotifyTarget: (project: string, id: string) => call<null>("DELETE", `/v1/apps/projects/${enc(project)}/notifications/${enc(id)}`),
  testNotifyTarget: (project: string, id: string) => call<{ ok: boolean; code: number | null; error: string | null }>("POST", `/v1/apps/projects/${enc(project)}/notifications/${enc(id)}/test`),
  notifyDeliveries: (project: string, id: string) => call<NotifyDelivery[]>("GET", `/v1/apps/projects/${enc(project)}/notifications/${enc(id)}/deliveries`),
  putRegistry: (id: string, body: { username: string; password: string; server?: string }) => call<Service>("PUT", `${svc(id)}/registry`, body),
  deleteRegistry: (id: string) => call<Service>("DELETE", `${svc(id)}/registry`),
  putGitLogin: (id: string, body: { username: string; password: string } | { username: string; secret: string }) => call<Service>("PUT", `${svc(id)}/git-login`, body),
  deleteGitLogin: (id: string) => call<Service>("DELETE", `${svc(id)}/git-login`),
  connectGitHub: (id: string) => call<Service>("PUT", `${svc(id)}/github`),
  disconnectGitHub: (id: string) => call<Service>("DELETE", `${svc(id)}/github`),
  putEnvironment: (id: string, e: string, body: EnvironmentPut) => call<Environment>("PUT", env(id, e), body),
  /** A one-use way into a sign-in-gated environment, as the signed-in person: the URL to go on to. */
  visit: (id: string, e: string, to: string) => call<{ url: string }>("POST", `${env(id, e)}/visit`, { to }),
  removeEnvironment: (id: string, e: string) => call<null>("DELETE", env(id, e)),

  deploy: (id: string, e: string, body: NewDeploy = {}) => call<Deployment>("POST", `${env(id, e)}/deploys`, body),
  /** Newest first, 100 at most; `before` (the last one's `created_at`): the page after it. */
  deploys: (id: string, e: string, before?: number) => call<Deployment[]>("GET", `${env(id, e)}/deploys${qs({ before })}`),
  deployment: (id: string, e: string, d: string) => call<Deployment>("GET", dep(id, e, d)),
  /** What a repo deployment brings over the live one (for the live one, the one before it). */
  commits: (id: string, e: string, d: string) => call<DeployCommits>("GET", `${dep(id, e, d)}/commits`),
  slo: (id: string, e: string) => call<SloReport>("GET", `${env(id, e)}/slo`),
  sbom: (id: string, e: string, d: string) => call<unknown>("GET", `${dep(id, e, d)}/sbom`),
  /** A failed deployment explained by its repo owner's model: kept, unless `again` asks anew (charged again). */
  explain: (id: string, e: string, d: string, again = false) => call<Explanation>("POST", `${dep(id, e, d)}/explain${qs({ again })}`),
  vulnerabilities: (id: string, e: string, d: string) => call<{ scan: ScanSummary | null; vulnerabilities: Vulnerability[] }>("GET", `${dep(id, e, d)}/vulnerabilities`),
  buildLog: (id: string, e: string, d: string, q: LogQuery = {}) => call<LogLine[]>("GET", `${dep(id, e, d)}/logs${qs({ tail: q.tail })}`),
  /** `keepVariables`: its image with the variables as they are now (after a rotated secret), not as they were then. */
  rollback: (id: string, e: string, d: string, keepVariables = false) => call<Deployment>("POST", `${dep(id, e, d)}/rollback`, keepVariables ? { keep_variables: true } : undefined),
  /** Back to the one live before what runs (the server walks back through rollbacks). */
  rollbackPrevious: (id: string, e: string) => call<Deployment>("POST", `${env(id, e)}/rollback`),
  /** A person's approval of a deployment waiting at ready; the last one needed puts it live. */
  approve: (id: string, e: string, d: string) => call<{ deployment: string; approved_by: string[]; needed: number; live: string | null; waiting?: string }>("POST", `${dep(id, e, d)}/approve`),
  /** The live image again with the current variables and settings: no build, same revision. */
  redeploy: (id: string, e: string, body: { message?: string } = {}) => call<Deployment>("POST", `${env(id, e)}/redeploy`, body),
  pushStates: (id: string, e: string) => call<PushState[]>("GET", `${env(id, e)}/push`),
  /** A new `ROLL_CONFIG_URL` token (a leaked address): the old one reads until a deploy made after it is live. */
  rotateConfig: (id: string, e: string) => call<Environment>("POST", `${env(id, e)}/config/rotate`),
  /** `expect`: the deployment of `from` that was looked at; a 409 when `from` runs another now. */
  promote: (id: string, e: string, from: string, expect?: string) => call<Deployment>("POST", `${env(id, e)}/promote`, { from, ...(expect ? { expect } : {}) }),
  backups: (id: string, e: string) => call<Backup[]>("GET", `${env(id, e)}/volume/backups`),
  /** A new deploy hook (the old one stops working): its URL, shown once. */
  createHook: async (id: string, e: string) => `${ROLL_API}${(await call<{ path: string }>("POST", `${env(id, e)}/hook`)).path}`,
  removeHook: (id: string, e: string) => call<void>("DELETE", `${env(id, e)}/hook`),
  backUp: (id: string, e: string) => call<Backup>("POST", `${env(id, e)}/volume/backups`),
  /** `from`: a backup of another environment of the service (seeding staging from production). */
  restore: (id: string, e: string, b: string, from?: string) => call<Backup>("POST", `${env(id, e)}/volume/backups/${encodeURIComponent(b)}/restore`, from ? { from } : undefined),
  cancel: (id: string, e: string, d: string) => call<Deployment>("POST", `${dep(id, e, d)}/cancel`),
  /** What a person says to a manual rollout waiting for them. */
  rollout: (id: string, e: string, d: string, action: "advance" | "all" | "abort") => call<{ deployment: string }>("POST", `${dep(id, e, d)}/rollout`, { action }),

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
  setVariables: (
    id: string,
    e: string,
    change: { set?: Record<string, string>; unset?: string[]; seal?: string[]; files?: Record<string, string | null>; runtime_only?: Record<string, boolean>; copy_from?: string; copy?: string[] },
  ) => call<Variables>("PATCH", `${env(id, e)}/variables`, change),
  diff: (id: string, e: string, from: string) => call<EnvironmentDiff>("GET", `${env(id, e)}/diff${qs({ from })}`),

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
    q: { q?: string; where?: string; since?: number; before?: number; instance?: string; deployment?: string; stream?: string; level?: LogLevel; http?: boolean; by?: string; status?: string; min_ms?: number; tail?: number },
  ): Promise<{ lines: LogLine[]; truncated: boolean }> => {
    const res = await fetch(`${ROLL_API}${env(id, e)}/logs${qs({ ...q, http: q.http || undefined, tail: q.tail ?? 500 })}`, withSession({ headers: apiHeaders(), cache: "no-store" }));
    onApiStatus(res.status);
    const data = await res.json().catch(() => null);
    if (!res.ok) throw new Error(data?.error ?? `${res.status} ${res.statusText}`.trim());
    return { lines: data as LogLine[], truncated: res.headers.get("x-roll-search-truncated") === "1" };
  },
  /** What's at `path` (absolute) in a running instance, as a tar (`GET …/files`, admin); the instance answering is `X-Roll-Instance`. */
  downloadFiles: async (id: string, e: string, path: string, instance?: string): Promise<{ tar: Blob; instance: string | null }> => {
    const res = await fetch(`${ROLL_API}${env(id, e)}/files${qs({ path, instance })}`, withSession({ headers: apiHeaders(), cache: "no-store" }));
    onApiStatus(res.status);
    if (!res.ok) {
      const data = await res.json().catch(() => null);
      throw new Error(data?.error ?? `${res.status} ${res.statusText}`.trim());
    }
    return { tar: await res.blob(), instance: res.headers.get("x-roll-instance") };
  },
  /** A tar written under the directory `path` in a running instance (`PUT …/files`, admin: what's written runs next). */
  uploadFiles: async (id: string, e: string, path: string, tar: Blob, instance?: string): Promise<{ instance: string; path: string }> => {
    const res = await fetch(`${ROLL_API}${env(id, e)}/files${qs({ path, instance })}`, withSession({ method: "PUT", headers: { ...apiHeaders(), "content-type": "application/x-tar" }, body: tar }));
    onApiStatus(res.status);
    const data = await res.json().catch(() => null);
    if (!res.ok) throw new Error(data?.error ?? `${res.status} ${res.statusText}`.trim());
    return data as { instance: string; path: string };
  },
  /** Drops kept answers (`POST …/cache/purge`): some paths (a trailing `*` a prefix), some tags, or (neither) all of them. */
  purgeCache: (id: string, e: string, body: { paths?: string[]; tags?: string[] }) => call<{ purged: number }>("POST", `${env(id, e)}/cache/purge`, body),
  /** A folder's bundle (`lib/bundle.ts`, what `roll up` sends) deployed to an upload service. */
  uploadDeploy: async (id: string, e: string, body: Blob, message?: string): Promise<Deployment> => {
    const headers: Record<string, string> = { ...apiHeaders(), "content-type": "application/gzip" };
    if (message) headers["x-roll-message"] = encodeURIComponent(message);
    const res = await fetch(`${ROLL_API}${env(id, e)}/deploys`, withSession({ method: "POST", headers, body }));
    onApiStatus(res.status);
    const data = await res.json().catch(() => null);
    if (!res.ok) throw new Error(data?.error ?? `${res.status} ${res.statusText}`.trim());
    return data as Deployment;
  },
  metrics: (id: string, e: string, minutes = 60) => call<Metrics>("GET", `${env(id, e)}/metrics${qs({ minutes })}`),
  /** Errors and latency by route over the last `minutes` (5–60). */
  routes: (id: string, e: string, minutes = 60) => call<RouteStats>("GET", `${env(id, e)}/routes${qs({ minutes })}`),
  analytics: (id: string, e: string, minutes = 1440) => call<Analytics>("GET", `${env(id, e)}/analytics${qs({ minutes })}`),
  vitals: (id: string, e: string, minutes = 1440) => call<Vitals>("GET", `${env(id, e)}/vitals${qs({ minutes })}`),
  setErrorState: (id: string, e: string, fingerprint: string, state: "open" | "resolved" | "muted") => call<{ fingerprint: string; state: string }>("PATCH", `${env(id, e)}/errors/${encodeURIComponent(fingerprint)}`, { state }),
  errors: (id: string, e: string, minutes = 1440) => call<{ minutes: number; groups: ErrorGroup[]; scanned: number; cut: boolean }>("GET", `${env(id, e)}/errors${qs({ minutes })}`),
  appMetrics: (id: string, e: string, minutes = 60) => call<AppMetrics>("GET", `${env(id, e)}/app-metrics${qs({ minutes })}`),
  traces: (id: string, e: string, minutes = 60) => call<{ minutes: number; traces: TraceSummary[]; cut: boolean }>("GET", `${env(id, e)}/traces${qs({ minutes })}`),
  trace: (id: string, e: string, trace: string) => call<{ trace: string; spans: Span[] }>("GET", `${env(id, e)}/traces/${enc(trace)}`),
  usage: (id: string, e: string, days = 30) => call<Usage>("GET", `${env(id, e)}/usage${qs({ days })}`),

  runs: (id: string, e: string) => call<CronRun[]>("GET", `${env(id, e)}/runs`),
  runNow: (id: string, e: string) => call<CronRun>("POST", `${env(id, e)}/runs`),
  run: (id: string, e: string, r: string) => call<CronRun>("GET", `${env(id, e)}/runs/${encodeURIComponent(r)}`),
  cancelRun: (id: string, e: string, r: string) => call<CronRun>("POST", `${env(id, e)}/runs/${encodeURIComponent(r)}/cancel`),
  jobs: (id: string, e: string) => call<Job[]>("GET", `${env(id, e)}/jobs`),
  startJob: (id: string, e: string, body: { command: string; timeout_s?: number; tasks?: number; retries?: number; parallelism?: number }) => call<Job>("POST", `${env(id, e)}/jobs`, body),
  job: (id: string, e: string, j: string) => call<Job>("GET", `${env(id, e)}/jobs/${encodeURIComponent(j)}`),
  cancelJob: (id: string, e: string, j: string) => call<Job>("POST", `${env(id, e)}/jobs/${encodeURIComponent(j)}/cancel`),

  /** A shell in a running instance over HTTP (a browser can't send the token on a WebSocket). */
  /** `instance`: that one (default: one that's running, or a woken one). */
  openShell: (id: string, e: string, body: { cols: number; rows: number; instance?: string; console?: boolean; command?: string; cpu?: number; memory_mb?: number }) => call<{ id: string }>("POST", `${env(id, e)}/shell/sessions`, body),
  /** The environment's last finished minute as Prometheus text, for a scraper with a token that may read it. */
  prometheusUrl: (id: string, e: string) => `${ROLL_API}${env(id, e)}/metrics?format=prometheus`,
  shellKeys: (id: string, e: string, sh: string, data: string) => call<null>("POST", `${env(id, e)}/shell/sessions/${encodeURIComponent(sh)}/keys`, { data }),
  resizeShell: (id: string, e: string, sh: string, size: { cols: number; rows: number }) => call<null>("POST", `${env(id, e)}/shell/sessions/${encodeURIComponent(sh)}/resize`, size),
  closeShell: (id: string, e: string, sh: string) => call<null>("DELETE", `${env(id, e)}/shell/sessions/${encodeURIComponent(sh)}`),

  domains: (id: string, e: string) => call<Domain[]>("GET", `${env(id, e)}/domains`),
  addDomain: (id: string, e: string, hostname: string) => call<Domain>("POST", `${env(id, e)}/domains`, { hostname }),
  verifyDomain: (id: string, e: string, host: string) => call<Domain>("POST", `${env(id, e)}/domains/${enc(host)}/verify`),
  removeDomain: (id: string, e: string, host: string) => call<null>("DELETE", `${env(id, e)}/domains/${enc(host)}`),
  redirectDomain: (id: string, e: string, host: string, to: string | null) => call<Domain>("PATCH", `${env(id, e)}/domains/${enc(host)}`, { redirect_to: to }),
  /** Send a custom domain to another of the app's ports (null: the service's own). */
  domainPort: (id: string, e: string, host: string, port: number | null) => call<Domain>("PATCH", `${env(id, e)}/domains/${enc(host)}`, { port }),
};

/**
 * Follows a log over server-sent events (`?follow=1`), with the operator token EventSource can't
 * send. `onReset` runs on every (re)connect: the stream starts again with the last `tail` lines,
 * so the caller drops what it had. Returns the function that stops it.
 */
/** A shell's screen over SSE: base64 chunks (the scrollback first, again after a reconnect: `onReset`), then `onClosed`. */
export function followShell(service: string, e: string, sh: string, opts: ScreenHandlers): () => void {
  return followScreen(`${ROLL_API}${env(service, e)}/shell/sessions/${encodeURIComponent(sh)}/screen`, operatorHeaders, opts);
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
  by?: "allow_list" | "firewall" | "rate_limit" | "private" | "maintenance" | "wake" | "capped" | "paused" | "not_deployed" | "moving" | "unavailable";
}

type ChangeFollower = { onChange: () => void; onOpen?: (open: boolean) => void };

/** One `…/changes` stream a service, shared by everything on the page that follows it. */
const changeStreams = new Map<string, { followers: Set<ChangeFollower>; open: boolean; stop: () => void }>();

/** Follows a service's changes (`…/changes`): `onChange` for each tick, `onOpen` as the stream comes and goes. Followers of one service share
 *  one stream, which closes when the last stops. */
export function followChanges(service: string, opts: ChangeFollower): () => void {
  let s = changeStreams.get(service);
  if (!s) {
    const followers = new Set<ChangeFollower>();
    const shared = { followers, open: false, stop: () => {} };
    shared.stop = streamSse(`${ROLL_API}/v1/apps/services/${encodeURIComponent(service)}/changes`, {
      headers: operatorHeaders,
      onOpen: (open) => {
        shared.open = open;
        for (const f of [...followers]) f.onOpen?.(open);
      },
      onEvent: (e) => {
        if (e.event === "changed") for (const f of [...followers]) f.onChange();
      },
    });
    changeStreams.set(service, shared);
    s = shared;
  }
  const follower = { ...opts };
  s.followers.add(follower);
  if (s.open) follower.onOpen?.(true);
  const mine = s;
  return () => {
    mine.followers.delete(follower);
    if (mine.followers.size === 0 && changeStreams.get(service) === mine) {
      changeStreams.delete(service);
      mine.stop();
    }
  };
}

export function followLog(
  kind:
    | { runtime: { service: string; env: string; http?: boolean } }
    | { build: { service: string; env: string; deployment: string } }
    | { job: { service: string; env: string; job: string } }
    | { run: { service: string; env: string; run: string } },
  opts: { tail?: number; onLine: (l: LogLine) => void; onReset: () => void; onOpen?: (open: boolean) => void; onEnd?: () => void },
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
  const stop = streamSse(`${ROLL_API}${path}${qs({ follow: true, tail: opts.tail ?? 500, http })}`, {
    headers: operatorHeaders,
    onOpen: (open) => {
      if (open) opts.onReset();
      opts.onOpen?.(open);
    },
    onEvent: (e) => {
      // A build's, a job's or a run's log ended: nothing more comes, so no reconnect (one would
      // read it all again).
      if (e.event === "end") {
        stop();
        opts.onEnd?.();
        return;
      }
      try {
        opts.onLine(JSON.parse(e.data) as LogLine);
      } catch {
        // Not a log line (a comment the parser kept): skip it.
      }
    },
  });
  return stop;
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
    case "git":
      return s.root ? `${s.url} · ${s.root}` : s.url;
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

/** Where each platform's functions answer, to route them to their service. */
export const FUNCTION_ROUTES: Record<string, string> = { "netlify/functions": "/.netlify/functions/*", api: "/api/*" };

/**
 * A static site's functions, run (Netlify Functions, Vercel's `api/`): a service of them from the
 * same source (the `functions` builder), and the site's environment routing their paths to it.
 * Its environment of the same name answers, so the site's `env` must be one the new service has
 * (a new service has production).
 */
export async function runFunctions(siteId: string, env: string, dirs: string[]): Promise<Service> {
  const site = await appsApi.service(siteId);
  const fns = await appsApi.create({ project: site.project, name: `${site.name}-fn`.slice(0, 40), source: site.source, build: { ...site.build, builder: "functions" }, run: { port: 8080 } });
  const current = await appsApi.environment(siteId, env);
  const wanted = dirs.map((d) => FUNCTION_ROUTES[d]).filter((p): p is string => !!p);
  const routes = [...(current.routes ?? []).filter((r) => !wanted.includes(r.path)), ...wanted.map((path) => ({ path, service: fns.name }))];
  await appsApi.putEnvironment(siteId, env, { routes });
  return fns;
}
