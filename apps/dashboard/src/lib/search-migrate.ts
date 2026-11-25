/**
 * roll search: migrating from Elasticsearch, OpenSearch, Algolia, Typesense or Meilisearch
 * (`POST /v1/search/clusters/{id}/migrate/inspect` and `…/migrate`,
 * crates/search-control/src/migrate.rs and src/saas/). roll's server talks to the source, never
 * the browser; the source's key or password goes to roll with those two calls, isn't kept and is
 * never sent back. The copies run as reindex tasks the dashboard follows through `_tasks`.
 */

import { call } from "./roll-api";
import { indexNameProblem } from "./search-import";

/** The product a migration reads from (`elasticsearch` when absent). */
export type SourceKind = "elasticsearch" | "opensearch" | "algolia" | "typesense" | "meilisearch";

/** Algolia, Typesense and Meilisearch: their definitions are mapped, not copied as they are. */
export function isSaas(kind: SourceKind | undefined): boolean {
  return kind === "algolia" || kind === "typesense" || kind === "meilisearch";
}

/** What the source picker offers, and what each source's form asks for. */
export const SOURCES: { value: SourceKind; label: string; placeholder: string; keyHint: string }[] = [
  {
    value: "elasticsearch",
    label: "Elasticsearch",
    placeholder: "https://my-deployment.es.us-east-1.aws.elastic.cloud",
    keyHint: "Its encoded value. It needs the monitor cluster privilege, and read and view_index_metadata on the indices to copy.",
  },
  { value: "opensearch", label: "OpenSearch", placeholder: "https://search-my-domain.us-east-1.es.amazonaws.com", keyHint: "Its encoded value, if the cluster takes API keys." },
  { value: "algolia", label: "Algolia", placeholder: "", keyHint: "The Admin API key, or a key with the listIndexes, browse and settings ACLs (Settings → API Keys)." },
  { value: "typesense", label: "Typesense", placeholder: "https://xyz.a1.typesense.net", keyHint: "An admin key, or one allowed collections:get, documents:export, synonyms:get and overrides:get." },
  { value: "meilisearch", label: "Meilisearch", placeholder: "https://ms-1234.meilisearch.io", keyHint: "The master key, or a key with indexes.get, documents.get, settings.get and stats.get. Leave it empty for a server without one." },
];

/** The remote and how to log in to it. */
export interface MigrateSource {
  kind?: SourceKind;
  host?: string;
  /** Algolia's application id (its host comes from it). */
  app_id?: string;
  api_key?: string;
  username?: string;
  password?: string;
}

export interface RemoteIndex {
  index: string;
  health: string | null;
  status: string | null;
  docs: number | null;
  size_bytes: number | null;
  primary_size_bytes: number | null;
  primaries: number | null;
  replicas: number | null;
  /** Its first aliases (`aliases_more` counts the rest). */
  aliases: string[];
  aliases_more?: number;
  /** Its aliases that roll has already (as an alias or an index): not copied, so the copy never
   * joins what they point at. */
  aliases_on_roll: string[];
  /** An index of this name exists on the roll cluster already. */
  exists: boolean;
  /** An Algolia replica: the index it's a replica of (copying the primary brings its sort). */
  replica_of?: string;
}

export interface MigrateInspect {
  remote: { host: string; version: string; distribution: string; cluster_name: string | null };
  indices: RemoteIndex[];
  /** Indices past the first 10,000 roll lists (they aren't shown). */
  more_indices?: number;
  /** Data streams past the first 100 named. */
  more_data_streams?: number;
  /** Data streams on the remote, which this flow doesn't copy yet. */
  data_streams: string[];
}

/** A setting of the remote's index that wasn't copied, and why. */
export interface DroppedSetting {
  setting: string;
  value: unknown;
  reason: string;
}

/** Something of an Algolia, Typesense or Meilisearch index that didn't map, and why. */
export interface NotMapped {
  kind: "setting" | "synonym" | "rule" | "field" | "replica";
  name: string;
  reason: string;
}

/** What an Algolia, Typesense or Meilisearch index becomes on roll. */
export interface SaasPlan {
  id_field: string;
  docs: number | null;
  mappings: number;
  default_fields: string[];
  synonyms: number;
  rules: number;
  synonyms_set?: string;
  rulesets?: string[];
  search_template: string;
  /** Algolia replicas, each a search template with its sort. */
  sort_templates?: Record<string, string>;
  /** Rulesets of Algolia rules with `filterPromotes` (their pins match the search's `filters`). */
  filtered_rulesets?: string[];
  /** Algolia rules that filter or rewrite the query: search with `template` when the query
   * meets the rule's condition. */
  rule_templates?: { rule: string; anchoring: string; pattern: string; template: string }[];
  not_mapped: NotMapped[];
  not_mapped_more: number;
  /** What mapped with a difference worth knowing. */
  notes: string[];
}

/** One picked index after `migrate`: its task, why it didn't start, or that it waits for a
 * free task slot (`queued`: nothing was created for it yet). */
export interface MigrateStarted {
  index: string;
  dest: string;
  task?: string;
  error?: string;
  dropped?: DroppedSetting[];
  aliases?: string[];
  queued?: boolean;
  reason?: string;
  /** The copy was created (also when the call's deadline cut the rest short). */
  created?: boolean;
  /** Algolia, Typesense, Meilisearch: what the index became (or would become, `dry_run`). */
  plan?: SaasPlan;
  /** Copied again into the index an earlier copy of it made. */
  restarted?: boolean;
  /** `dry_run`: the index exists from an earlier copy, which a start copies into again. */
  restart?: boolean;
  note?: string;
  /** A stopped copy resumed: from where (`dry_run`: `resume`). */
  resumed?: string;
  resume?: string;
  /** `catch_up` or `cutover` (a copy when absent). */
  step?: "catch_up" | "cutover";
  /** A catch-up: how it reads (`since_attribute`, or `compare`: every record). */
  method?: "since_attribute" | "compare";
  /** A cutover: the alias moved, and the indices it left. */
  alias?: string;
  previous?: string[];
}

export interface MigrateRequest {
  source: MigrateSource;
  indices: { index: string; dest?: string; alias?: string; full?: boolean }[];
  /** `copy` (the default: resumes a stopped copy), `catch_up` or `cutover`. */
  step?: "copy" | "catch_up" | "cutover";
  /** `catch_up`: a numeric attribute the records set when they change. */
  since_attribute?: string;
  since_unit?: "s" | "ms";
  requests_per_second?: number;
  size?: number;
  /** Algolia, Typesense, Meilisearch: answer each index's plan, making nothing. */
  dry_run?: boolean;
}

const cl = (id: string) => `/v1/search/clusters/${encodeURIComponent(id)}`;

export const migrateApi = {
  inspect: (id: string, source: MigrateSource) => call<MigrateInspect>("POST", `${cl(id)}/migrate/inspect`, { source }),
  start: (id: string, req: MigrateRequest) => call<{ indices: MigrateStarted[] }>("POST", `${cl(id)}/migrate`, req),
};

export type AuthKind = "api_key" | "basic" | "none";

export interface SourceForm {
  /** Elasticsearch unless set. */
  kind?: SourceKind;
  host: string;
  /** Algolia's application id. */
  appId?: string;
  auth: AuthKind;
  apiKey: string;
  username: string;
  password: string;
}

/** What the form sends: only the credentials of the chosen kind. */
export function sourceFrom(f: SourceForm): MigrateSource {
  const kind = f.kind ?? "elasticsearch";
  if (isSaas(kind)) {
    const s: MigrateSource = kind === "algolia" ? { kind, app_id: (f.appId ?? "").trim() } : { kind, host: f.host.trim() };
    if (f.apiKey.trim()) s.api_key = f.apiKey.trim();
    return s;
  }
  const s: MigrateSource = { host: f.host.trim() };
  if (kind === "opensearch") s.kind = kind;
  if (f.auth === "api_key" && f.apiKey.trim()) s.api_key = f.apiKey.trim();
  if (f.auth === "basic" && f.username.trim()) {
    s.username = f.username.trim();
    s.password = f.password;
  }
  return s;
}

/** Why the form can't be sent yet, or `null`. */
export function sourceProblem(f: SourceForm): string | null {
  const kind = f.kind ?? "elasticsearch";
  if (kind === "algolia") {
    const app = (f.appId ?? "").trim();
    if (!app) return "Enter the application id.";
    if (!/^[A-Za-z0-9]{1,32}$/.test(app)) return "The application id is letters and digits, as Algolia's dashboard shows it.";
    if (!f.apiKey.trim()) return "Paste an API key.";
    return null;
  }
  const host = f.host.trim();
  if (!host) return "Enter the cluster's URL.";
  const scheme = /^([a-z][a-z0-9+.-]*):\/\//i.exec(host)?.[1]?.toLowerCase();
  if (scheme && scheme !== "https" && scheme !== "http") return "The URL starts with https:// (or http://).";
  // Credentials typed into the URL would end up in places a password shouldn't.
  if (/^([a-z][a-z0-9+.-]*:\/\/)?[^/]*@/i.test(host)) return "Put the username and password in their own fields, not in the URL.";
  if (/\s/.test(host)) return "The URL can't hold spaces.";
  if (kind === "typesense" && !f.apiKey.trim()) return "Paste an API key.";
  if (kind === "meilisearch") return null;
  if (f.auth === "api_key" && !f.apiKey.trim()) return "Paste an API key, or pick another way to sign in.";
  if (f.auth === "basic" && !f.username.trim()) return "Enter a username.";
  return null;
}

/** Why a key would cross the network readable, or `null`: a Typesense or Meilisearch URL over
 * plain `http://`. */
export function plainHttpWarning(f: SourceForm): string | null {
  const kind = f.kind ?? "elasticsearch";
  if (kind !== "typesense" && kind !== "meilisearch") return null;
  return /^http:\/\//i.test(f.host.trim()) ? "This URL is plain http: the API key crosses the network unencrypted. Use https:// if the server has it." : null;
}

/** What the panel says when roll refuses a review or a start: both need `admin` on the
 * cluster (they add the source to the cluster and start copies as its own user). */
export function migrateRefusal(status: number | undefined, message: string): string {
  if (status === 403) return "Reviewing and starting a migration need admin on this cluster: they create indices and start copies as the cluster's own user. Ask someone with admin, or sign in with a token that has it.";
  return message;
}

/** The URL roll will call, as the server normalizes it (a scheme and a port), for display. */
export function displayHost(host: string): string {
  const h = host.trim().replace(/\/+$/, "");
  if (!h) return "";
  const withScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(h) ? h : `https://${h}`;
  try {
    const u = new URL(withScheme);
    const port = u.port || (u.protocol === "http:" ? "80" : "443");
    return `${u.protocol}//${u.hostname}:${port}${u.pathname.replace(/\/+$/, "")}`;
  } catch {
    return withScheme;
  }
}

/** The default name of an index's copy: its own, unless roll has one by that name. */
export function defaultDest(index: string, taken: Set<string>): string {
  if (!taken.has(index)) return index;
  for (let i = 1; ; i++) {
    const name = i === 1 ? `${index}-migrated` : `${index}-migrated-${i}`;
    if (!taken.has(name)) return name;
  }
}

/** Why `dest` can't be the copy's name: an invalid index name, one roll has, or one picked twice. */
export function destProblem(dest: string, existing: Set<string>, others: string[]): string | null {
  const p = indexNameProblem(dest);
  if (p) return p;
  if (existing.has(dest)) return "An index by this name exists on roll.";
  if (others.includes(dest)) return "Two copies can't have the same name.";
  return null;
}

/** A reindex task as `GET _tasks/<id>` answers (the parts the flow reads). */
export interface TaskBody {
  completed?: boolean;
  task?: { status?: TaskCounts; description?: string; cancelled?: boolean };
  response?: TaskCounts & { failures?: { cause?: { reason?: string; type?: string } }[]; canceled?: string };
  error?: { type?: string; reason?: string; caused_by?: { reason?: string } };
}

interface TaskCounts {
  total?: number;
  created?: number;
  updated?: number;
  deleted?: number;
  noops?: number;
  version_conflicts?: number;
  batches?: number;
  canceled?: string;
}

/** `lost`: `_tasks` doesn't know the task any more (the cluster restarted or moved while it
 * ran), so the copy stopped part way. */
export type TaskState = "running" | "done" | "failed" | "cancelled" | "lost";

export interface TaskProgress {
  state: TaskState;
  /** Documents written (created, updated or found unchanged). */
  copied: number;
  /** The remote's count of documents to copy (0 until the first page came). */
  total: number;
  /** 0–1, or `null` while the total isn't known. */
  fraction: number | null;
  /** Why it failed: the task's error, or its first failed document's reason. */
  error: string | null;
  failures: number;
}

/** Where a migration's reindex task stands. */
export function taskProgress(t: TaskBody): TaskProgress {
  const c: TaskCounts = (t.completed ? t.response : t.task?.status) ?? {};
  const copied = (c.created ?? 0) + (c.updated ?? 0) + (c.noops ?? 0) + (c.version_conflicts ?? 0);
  const total = c.total ?? 0;
  const failures = t.response?.failures?.length ?? 0;
  let state: TaskState = "running";
  let error: string | null = null;
  if (t.completed) {
    if (t.error) {
      state = t.error.type === "task_cancelled_exception" ? "cancelled" : "failed";
      const cause = t.error.caused_by?.reason;
      error = state === "failed" ? `${t.error.type ?? "error"}: ${t.error.reason ?? ""}${cause && !t.error.reason?.includes(cause) ? ` (${cause})` : ""}` : null;
    } else if (c.canceled) state = "cancelled";
    else if (failures > 0) {
      state = "failed";
      const f = t.response?.failures?.[0]?.cause;
      error = f ? `${f.type ?? "error"}: ${f.reason ?? ""}` : `${failures} documents failed`;
    } else state = "done";
  } else if (t.task?.cancelled) state = "cancelled";
  const fraction = state === "done" ? 1 : total > 0 ? Math.min(1, copied / total) : null;
  return { state, copied, total, fraction, error, failures };
}

/** What one poll of `GET _tasks/<id>` says: the task's progress; that it's gone (404: the
 * cluster restarted or moved, and the task with it); or nothing to go on (try again). */
export type TaskPoll = { kind: "task"; body: TaskBody } | { kind: "lost" } | { kind: "retry" };

export function taskPoll(status: number, body: string): TaskPoll {
  if (status === 404) return { kind: "lost" };
  if (status !== 200) return { kind: "retry" };
  try {
    return { kind: "task", body: JSON.parse(body) as TaskBody };
  } catch {
    return { kind: "retry" };
  }
}

/** A task `_tasks` lost: its copy holds what was written before. */
export const LOST: TaskProgress = { state: "lost", copied: 0, total: 0, fraction: null, error: "Stopped: the cluster restarted or moved while the copy ran, so the copy is partial.", failures: 0 };

/** Whether a copy in `state` holds a partial index someone may want to delete. */
export function partial(state: TaskState): boolean {
  return state === "failed" || state === "cancelled" || state === "lost";
}

/**
 * The alias the flow offers once a copy is done, so clients that used the remote's index name keep
 * working against roll: the source index's name, when the copy got another name and nothing on
 * roll is called that yet. `null` when there's nothing to offer.
 */
export function aliasOffer(index: string, dest: string, rollNames: Set<string>): string | null {
  if (index === dest || rollNames.has(index)) return null;
  return indexNameProblem(index) ? null : index;
}

/** The `_aliases` body that points `alias` at `dest`. */
export function aliasBody(alias: string, dest: string) {
  return { actions: [{ add: { index: dest, alias } }] };
}

/** How many copies one cluster runs at once (roll's `roll.reindex.remote.max_per_cluster`). */
export const COPIES_AT_ONCE = 2;

/** The copies waiting for a free task slot, as `migrate` takes them again. */
export function queuedPicks(rows: MigrateStarted[]): { index: string; dest: string }[] {
  return rows.filter((r) => r.queued).map((r) => ({ index: r.index, dest: r.dest }));
}

/** Whether to ask roll to start waiting copies now: some wait, and fewer than
 * [`COPIES_AT_ONCE`] run (a copy is running until its task ends; one without a known state yet
 * counts as running). */
export function shouldStartQueued(rows: MigrateStarted[], states: Record<string, TaskState | undefined>, atOnce = COPIES_AT_ONCE): boolean {
  if (!rows.some((r) => r.queued)) return false;
  const running = rows.filter((r) => r.task && (states[r.task] ?? "running") === "running").length;
  return running < atOnce;
}

/** Whether a failed attempt to start waiting copies is worth repeating: roll was busy (429) or
 * answered a deadline or server error (5xx). A refusal (4xx) isn't, and neither is a failure
 * without an answer from roll (`undefined`): the panel shows it and stops. */
export function retryableStart(status: number | undefined): boolean {
  return status !== undefined && (status === 429 || status >= 500);
}

/** How soon to ask again while any waiting copy came back `queued`: roll holds the node's copy
 * slots for each cluster's owner in turn, and a turn that isn't taken within a few seconds goes
 * to the next one in line, so a queued copy asks again often. */
export const QUEUED_RETRY_MS = 1_500;

/** How long to wait before asking again to start waiting copies, after `failures` attempts in a
 * row that failed: 5s, doubling, at most a minute. */
export function queueRetryDelay(failures: number): number {
  return Math.min(60_000, 5_000 * 2 ** Math.max(0, Math.min(failures, 10)));
}

/** `rows` with each waiting copy replaced by what a later `migrate` call made of it (matched by
 * index and copy name); the others as they were. */
export function mergeStarted(rows: MigrateStarted[], results: MigrateStarted[]): MigrateStarted[] {
  return rows.map((r) => (r.queued ? (results.find((x) => x.index === r.index && x.dest === r.dest) ?? r) : r));
}

/** What didn't map, grouped by kind in a fixed order (settings, fields, replicas, synonyms,
 * rules), for the summary shown before a copy starts and on its row. */
export function notMappedGroups(plan: SaasPlan): { kind: NotMapped["kind"]; items: NotMapped[] }[] {
  const order: NotMapped["kind"][] = ["setting", "field", "replica", "synonym", "rule"];
  return order.map((kind) => ({ kind, items: plan.not_mapped.filter((n) => n.kind === kind) })).filter((g) => g.items.length > 0);
}

/** How many things didn't map, the ones past the listed ones included. */
export function notMappedCount(plan: SaasPlan): number {
  return plan.not_mapped.length + plan.not_mapped_more;
}

/** What an Algolia, Typesense or Meilisearch copy's row offers next: resume a copy that
 * stopped, or, once it's done, catch up and cut over; nothing while it runs. */
export function nextSteps(row: MigrateStarted, state: TaskState | undefined): "resume" | "after" | null {
  if (!row.plan && !row.step) return null;
  if (row.step === "cutover") return null;
  if (state === "done") return "after";
  if (state === "failed" || state === "cancelled" || state === "lost") return row.step === "catch_up" ? "after" : "resume";
  return null;
}

/** `rows` with the one for `result`'s index and copy replaced by it (a resume, a catch-up, a
 * cutover), the plan kept when the new answer has none. */
export function replaceRow(rows: MigrateStarted[], result: MigrateStarted): MigrateStarted[] {
  return rows.map((r) => (r.index === result.index && r.dest === result.dest ? { ...result, plan: result.plan ?? r.plan } : r));
}

/** Why a catch-up's timestamp attribute can't be sent, or `null` (empty: every record is read). */
export function sinceProblem(attr: string): string | null {
  const a = attr.trim();
  if (!a) return null;
  return /^[\p{L}\p{N}_.@-]+$/u.test(a) ? null : "An attribute name: letters, digits, _ . - @.";
}

/** Dropped settings grouped by reason, in the order they first appear. */
export function droppedByReason(dropped: DroppedSetting[]): { reason: string; settings: string[] }[] {
  const out: { reason: string; settings: string[] }[] = [];
  for (const d of dropped) {
    const g = out.find((x) => x.reason === d.reason);
    if (g) g.settings.push(d.setting);
    else out.push({ reason: d.reason, settings: [d.setting] });
  }
  return out;
}
