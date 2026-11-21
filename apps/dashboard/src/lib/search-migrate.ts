/**
 * roll search: migrating from Elasticsearch or OpenSearch (`POST /v1/search/clusters/{id}/migrate/
 * inspect` and `…/migrate`, crates/search-control/src/migrate.rs). roll's server talks to the
 * remote, never the browser; the remote's key or password goes to roll with those two calls,
 * isn't kept and is never sent back. The copies run as reindex tasks the dashboard follows
 * through `_tasks`.
 */

import { call } from "./roll-api";
import { indexNameProblem } from "./search-import";

/** The remote and how to log in to it. */
export interface MigrateSource {
  host: string;
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
  aliases: string[];
  /** Its aliases that roll has already (as an alias or an index): not copied, so the copy never
   * joins what they point at. */
  aliases_on_roll: string[];
  /** An index of this name exists on the roll cluster already. */
  exists: boolean;
}

export interface MigrateInspect {
  remote: { host: string; version: string; distribution: string; cluster_name: string | null };
  indices: RemoteIndex[];
  /** Indices past the first 10,000 roll lists (they aren't shown). */
  more_indices?: number;
  /** Data streams on the remote, which this flow doesn't copy yet. */
  data_streams: string[];
}

/** A setting of the remote's index that wasn't copied, and why. */
export interface DroppedSetting {
  setting: string;
  value: unknown;
  reason: string;
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
}

export interface MigrateRequest {
  source: MigrateSource;
  indices: { index: string; dest?: string }[];
  requests_per_second?: number;
  size?: number;
}

const cl = (id: string) => `/v1/search/clusters/${encodeURIComponent(id)}`;

export const migrateApi = {
  inspect: (id: string, source: MigrateSource) => call<MigrateInspect>("POST", `${cl(id)}/migrate/inspect`, { source }),
  start: (id: string, req: MigrateRequest) => call<{ indices: MigrateStarted[] }>("POST", `${cl(id)}/migrate`, req),
};

export type AuthKind = "api_key" | "basic" | "none";

export interface SourceForm {
  host: string;
  auth: AuthKind;
  apiKey: string;
  username: string;
  password: string;
}

/** What the form sends: only the credentials of the chosen kind. */
export function sourceFrom(f: SourceForm): MigrateSource {
  const s: MigrateSource = { host: f.host.trim() };
  if (f.auth === "api_key" && f.apiKey.trim()) s.api_key = f.apiKey.trim();
  if (f.auth === "basic" && f.username.trim()) {
    s.username = f.username.trim();
    s.password = f.password;
  }
  return s;
}

/** Why the form can't be sent yet, or `null`. */
export function sourceProblem(f: SourceForm): string | null {
  const host = f.host.trim();
  if (!host) return "Enter the cluster's URL.";
  const scheme = /^([a-z][a-z0-9+.-]*):\/\//i.exec(host)?.[1]?.toLowerCase();
  if (scheme && scheme !== "https" && scheme !== "http") return "The URL starts with https:// (or http://).";
  // Credentials typed into the URL would end up in places a password shouldn't.
  if (/^([a-z][a-z0-9+.-]*:\/\/)?[^/]*@/i.test(host)) return "Put the username and password in their own fields, not in the URL.";
  if (/\s/.test(host)) return "The URL can't hold spaces.";
  if (f.auth === "api_key" && !f.apiKey.trim()) return "Paste an API key, or pick another way to sign in.";
  if (f.auth === "basic" && !f.username.trim()) return "Enter a username.";
  return null;
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

/** How long to wait before asking again to start waiting copies, after `failures` attempts in a
 * row that failed or started nothing: 5s, doubling, at most a minute. */
export function queueRetryDelay(failures: number): number {
  return Math.min(60_000, 5_000 * 2 ** Math.max(0, Math.min(failures, 10)));
}

/** `rows` with each waiting copy replaced by what a later `migrate` call made of it (matched by
 * index and copy name); the others as they were. */
export function mergeStarted(rows: MigrateStarted[], results: MigrateStarted[]): MigrateStarted[] {
  return rows.map((r) => (r.queued ? (results.find((x) => x.index === r.index && x.dest === r.dest) ?? r) : r));
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
