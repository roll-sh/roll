/**
 * Client for roll previews (`crates/roll-previews`, docs/previews.md "HTTP contract"): a full copy
 * of a workspace project per change, with a branch of every data resource. Change both together.
 */

import { call } from "./roll-api";

export const PREVIEW_STATES = ["queued", "awaiting_approval", "creating", "ready", "sleeping", "updating", "failed", "deleting"] as const;
export type PreviewState = (typeof PREVIEW_STATES)[number];

export type PreviewKind = "postgres" | "redis" | "bucket" | "nosql" | "stream" | "queue" | "tasks" | "search";

export interface PreviewService {
  name: string;
  repo: string;
  url: string;
  /** `url` with the preview's access token (link access): the link to open and hand out. */
  share_url: string | null;
  /** The changeset it should run. */
  revision: string | null;
  /** The changeset it was last deployed at. */
  deployed: string | null;
  state: "pending" | "deploying" | "running" | "sleeping" | "failed";
  error: string | null;
}

export interface PreviewResource {
  kind: PreviewKind;
  /** The resource in the project it branches. */
  parent: string;
  /** The branch, once it exists. */
  ref: string | null;
  mode: "branch" | "schema";
  state: "pending" | "ready" | "resetting" | "failed" | "deleting";
  error: string | null;
  /** Bytes the branch owns (not its parent's). */
  bytes: number;
}

export interface PreviewCost {
  sleeping_usd_month: number;
  active_usd_hour: number;
  month_to_date_usd: number;
  bytes: number;
}

/** Times are unix milliseconds. */
export interface Preview {
  id: string;
  project: string;
  slug: string;
  /** `owner/name`. */
  repo: string;
  /** `chg_…`, for a change's preview. */
  change: string | null;
  number: number | null;
  github_pr: number | null;
  branch: string | null;
  title: string;
  head: string;
  trusted: boolean;
  desired: "active" | "deleted";
  state: PreviewState;
  error: string | null;
  /** The first service's URL. */
  url: string | null;
  /** `url` with the access token, when the project's previews need a link. */
  share_url: string | null;
  services: PreviewService[];
  resources: PreviewResource[];
  cost: PreviewCost;
  /** What the change did to the Postgres schema (`+ …`, `- …`), once read at `head`. */
  schema_changes?: string[] | null;
  last_reset_at: number | null;
  last_active_at: number;
  expires_at: number | null;
  created_by: string;
  created_at: number;
  updated_at: number;
}

export interface PreviewActivity {
  at: number;
  message: string;
}

export type ResourceRule = "branch" | "schema" | "skip";

/** A command on the change's tree: the seed, or the release. */
export interface PreviewJob {
  image: string;
  run: string;
  timeout_s?: number;
  memory_mb?: number;
}

export interface PreviewSettings {
  enabled: boolean;
  /** Globs on the change's branch. */
  branches: string[];
  drafts: boolean;
  /** `"<kind>:<ref>"` → mode; resources not listed get `data`. A patch replaces the whole map. */
  resources: Record<string, ResourceRule>;
  data: "branch" | "schema";
  /** 0: never. */
  reset_every_hours: number;
  sleep_after_s: number;
  ttl_hours: number;
  max_previews: number;
  max_bytes: number;
  /** The month's budget for the project's previews, USD (0: no cap). */
  max_usd_month: number;
  secrets: { base: string; allow_production: boolean };
  /** A patch replaces the whole map. */
  env: Record<string, string>;
  github: { comment: boolean; status: boolean; deployment: boolean };
  /** "link": a preview's URLs need its link (its access token once, then a cookie). */
  access: "link" | "public";
  /** Run once on fresh data (after branching or a reset) before the services deploy. */
  seed: PreviewJob | null;
  /** Run at each new head, after the seed and before services deploy (migrations). */
  release?: PreviewJob | null;
  /** NoSQL column masks: `"nosql:<ref>"` → `{"keyspace.table.column": "mask_default()"}`. */
  masking: Record<string, Record<string, string>>;
}

/** A partial update: objects merge (except `resources` and `env`, which are replaced whole). */
export type SettingsPatch = Partial<Omit<PreviewSettings, "secrets" | "github">> & {
  secrets?: Partial<PreviewSettings["secrets"]>;
  github?: Partial<PreviewSettings["github"]>;
};

export interface PreviewUsage {
  previews: number;
  sleeping_usd_month: number;
  active_usd_hour: number;
  month_to_date_usd: number;
  bytes: number;
  /** This month, deleted previews included (what the budget counts). */
  spent_usd_month?: number;
  /** The project's monthly budget for previews, USD (0: no cap). */
  max_usd_month?: number;
}

/** A project's secret for automation: CI sends it in `header` to open private previews. */
export interface PreviewBypass {
  enabled: boolean;
  created_at: number | null;
  header: string;
  /** Only in the answer that made it. */
  secret?: string;
}

export type PreviewSource = { branch: string; change?: never } | { change: string; branch?: never };

const one = (id: string) => `/v1/previews/${encodeURIComponent(id)}`;
const project = (id: string) => `/v1/previews/projects/${encodeURIComponent(id)}`;

function query(params: Record<string, string | number | undefined>) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== "") q.set(k, String(v));
  const s = q.toString();
  return s ? `?${s}` : "";
}

export const previewsApi = {
  /** Newest first. */
  list: (filter: { project?: string; state?: PreviewState } = {}) => call<Preview[]>("GET", `/v1/previews${query(filter)}`),
  get: (id: string) => call<Preview>("GET", one(id)),
  /** A preview of a branch (no change) or of a change; the existing one if there is one. */
  create: (projectId: string, repo: string, source: PreviewSource) =>
    call<Preview>("POST", "/v1/previews", { project: projectId, repo, ...source }),
  /** Marks it for deletion (`desired: "deleted"`); the reconciler removes it. */
  remove: (id: string) => call<Preview>("DELETE", one(id)),
  refresh: (id: string) => call<Preview>("POST", `${one(id)}/refresh`),
  sleep: (id: string) => call<Preview>("POST", `${one(id)}/sleep`),
  wake: (id: string) => call<Preview>("POST", `${one(id)}/wake`),
  /** Re-branches every resource, or the ones named (`"<kind>:<parent>"`). */
  reset: (id: string, resources?: string[]) => call<Preview>("POST", `${one(id)}/reset`, resources?.length ? { resources } : {}),
  logs: (id: string, opts: { service?: string; lines?: number } = {}) =>
    call<{ service: string; lines: string[] }>("GET", `${one(id)}/logs${query(opts)}`),
  /** Newest first. */
  activity: (id: string) => call<PreviewActivity[]>("GET", `${one(id)}/activity`),
  settings: (projectId: string) => call<PreviewSettings>("GET", `${project(projectId)}/settings`),
  putSettings: (projectId: string, patch: SettingsPatch) => call<PreviewSettings>("PUT", `${project(projectId)}/settings`, patch),
  usage: (projectId: string) => call<PreviewUsage>("GET", `${project(projectId)}/usage`),
  bypass: (projectId: string) => call<PreviewBypass>("GET", `${project(projectId)}/bypass`),
  /** A new secret (the old one stops working), shown only in this answer. */
  rotateBypass: (projectId: string) => call<PreviewBypass>("POST", `${project(projectId)}/bypass`),
  deleteBypass: (projectId: string) => call<void>("DELETE", `${project(projectId)}/bypass`),
};

/** A resource as the reset body and the settings name it. */
export const resourceId = (r: Pick<PreviewResource, "kind" | "parent">) => `${r.kind}:${r.parent}`;

/** Work in progress: the list polls quickly while any preview is in one of these. */
export const BUSY: ReadonlySet<PreviewState> = new Set(["creating", "updating", "deleting"]);

/** Production's environment names (crates/roll-previews `is_production`). */
export const isProduction = (env: string) => ["production", "prod", "live"].includes(env.toLowerCase());

/** The URL to open: with the access token when the preview is private. */
export const openUrl = (x: { url: string | null; share_url?: string | null }) => x.share_url ?? x.url ?? undefined;
