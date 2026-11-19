/**
 * Snapshot and restore, as Kibana's "Snapshot and Restore" does it: repositories (`_snapshot`),
 * snapshots, restores (with their `_recovery` progress) and snapshot lifecycle policies
 * (`_slm/policy`). Pure logic: what the panel sends and how it reads the answers
 * (`components/search/snapshots.tsx`).
 */

// --- Repositories ------------------------------------------------------------------------------

export type RepoType = "s3" | "fs" | "url" | "gcs" | "azure";

/** One setting of a repository type, as the register form asks for it. */
export interface RepoSetting {
  key: string;
  label: string;
  /** `bool` settings are checkboxes; the rest are text. */
  kind: "text" | "bool";
  required?: boolean;
  placeholder?: string;
  /** A boolean's default: it is sent only when it differs. */
  default?: boolean;
}

const COMMON: RepoSetting[] = [
  { key: "base_path", label: "Base path", kind: "text", placeholder: "snapshots/prod" },
  { key: "client", label: "Client", kind: "text", placeholder: "default" },
  { key: "compress", label: "Compress metadata", kind: "bool", default: true },
  { key: "readonly", label: "Read-only", kind: "bool", default: false },
];

/** The repository types and their settings (Elasticsearch's repository plugins). */
export const REPO_TYPES: { type: RepoType; label: string; settings: RepoSetting[] }[] = [
  { type: "s3", label: "AWS S3", settings: [{ key: "bucket", label: "Bucket", kind: "text", required: true, placeholder: "my-snapshots" }, ...COMMON] },
  {
    type: "fs",
    label: "Shared file system",
    settings: [
      { key: "location", label: "Location", kind: "text", required: true, placeholder: "backups (under path.repo)" },
      { key: "compress", label: "Compress metadata", kind: "bool", default: true },
      { key: "readonly", label: "Read-only", kind: "bool", default: false },
    ],
  },
  { type: "url", label: "Read-only URL", settings: [{ key: "url", label: "URL", kind: "text", required: true, placeholder: "https://example.com/snapshots/" }] },
  { type: "gcs", label: "Google Cloud Storage", settings: [{ key: "bucket", label: "Bucket", kind: "text", required: true, placeholder: "my-snapshots" }, ...COMMON] },
  { type: "azure", label: "Azure Blob Storage", settings: [{ key: "container", label: "Container", kind: "text", required: true, placeholder: "snapshots" }, ...COMMON] },
];

export type RepoValues = Record<string, string | boolean>;

/** `PUT _snapshot/<name>`'s body: blank settings left out, booleans only when not the default. */
export function repositoryBody(type: RepoType, values: RepoValues): { type: RepoType; settings: Record<string, string | boolean> } {
  const settings: Record<string, string | boolean> = {};
  for (const s of REPO_TYPES.find((t) => t.type === type)?.settings ?? []) {
    const v = values[s.key];
    if (s.kind === "bool") {
      if (typeof v === "boolean" && v !== s.default) settings[s.key] = v;
    } else if (typeof v === "string" && v.trim()) {
      settings[s.key] = v.trim();
    }
  }
  return { type, settings };
}

/** Why the register form can't be sent yet, or null. */
export function repositoryProblem(name: string, type: RepoType, values: RepoValues, existing: string[]): string | null {
  const n = name.trim();
  if (!n) return "Name the repository.";
  if (n !== n.toLowerCase() || /[\s"*\\<|,>/?#]/.test(n) || n.startsWith("_")) return "Use lowercase letters, digits, - and _ (not first).";
  if (existing.includes(n)) return `${n} is registered already.`;
  const missing = REPO_TYPES.find((t) => t.type === type)?.settings.find((s) => s.required && !String(values[s.key] ?? "").trim());
  return missing ? `${missing.label} is required.` : null;
}

/** A repository as `GET _snapshot` lists it. */
export interface Repository {
  type: string;
  settings?: Record<string, unknown>;
}

/** Where a repository keeps its snapshots, in a few words (`roll` for roll's own). */
export function repositoryWhere(r: Repository): string {
  const s = r.settings ?? {};
  const str = (k: string) => (typeof s[k] === "string" ? (s[k] as string) : "");
  const path = (root: string) => [root, str("base_path")].filter(Boolean).join("/");
  switch (r.type) {
    case "s3":
      return `s3://${path(str("bucket"))}`;
    case "gcs":
      return `gs://${path(str("bucket"))}`;
    case "azure":
      return `azure://${path(str("container"))}`;
    case "fs":
      return str("location");
    case "url":
      return str("url");
    default:
      return "";
  }
}

// --- Snapshots ---------------------------------------------------------------------------------

/** A snapshot as `GET _snapshot/<repository>/_all` lists it (the fields the panel reads). */
export interface SnapshotInfo {
  snapshot: string;
  uuid: string;
  repository?: string;
  indices?: string[];
  data_streams?: string[];
  index_details?: Record<string, { size_in_bytes?: number; shard_count?: number }>;
  include_global_state?: boolean;
  state: string;
  start_time_in_millis?: number;
  end_time_in_millis?: number;
  duration_in_millis?: number;
  failures?: { index?: string; shard_id?: number; reason?: string; status?: string }[];
  shards?: { total: number; failed: number; successful: number };
  metadata?: { policy?: string } & Record<string, unknown>;
}

export interface SnapshotRow {
  repository: string;
  name: string;
  uuid: string;
  state: string;
  indices: string[];
  dataStreams: string[];
  /** Summed from `index_details` (null when the cluster doesn't give them: OpenSearch). */
  sizeBytes: number | null;
  startMs: number | null;
  durationMs: number | null;
  shards: { total: number; failed: number; successful: number };
  failures: string[];
  globalState: boolean;
  /** The SLM policy that took it. */
  policy: string | null;
}

/** Each repository's snapshots as rows, newest first. */
export function snapshotRows(byRepo: Record<string, SnapshotInfo[]>): SnapshotRow[] {
  const rows: SnapshotRow[] = [];
  for (const [repository, list] of Object.entries(byRepo)) {
    for (const s of list) {
      const details = s.index_details ? Object.values(s.index_details) : null;
      rows.push({
        repository,
        name: s.snapshot,
        uuid: s.uuid,
        state: s.state,
        indices: s.indices ?? [],
        dataStreams: s.data_streams ?? [],
        sizeBytes: details ? details.reduce((n, d) => n + (d.size_in_bytes ?? 0), 0) : null,
        startMs: s.start_time_in_millis ?? null,
        durationMs: s.state === "IN_PROGRESS" ? null : (s.duration_in_millis ?? null),
        shards: s.shards ?? { total: 0, failed: 0, successful: 0 },
        failures: (s.failures ?? []).map((f) => [f.index && `${f.index}${f.shard_id !== undefined ? `[${f.shard_id}]` : ""}`, f.reason].filter(Boolean).join(": ")),
        globalState: !!s.include_global_state,
        policy: typeof s.metadata?.policy === "string" ? s.metadata.policy : null,
      });
    }
  }
  return rows.sort((a, b) => (b.startMs ?? 0) - (a.startMs ?? 0) || a.name.localeCompare(b.name));
}

/** A name for a snapshot taken by hand (`snapshot-2026.11.18-1630`, UTC). */
export function snapshotName(now: Date): string {
  const p = (n: number) => String(n).padStart(2, "0");
  return `snapshot-${now.getUTCFullYear()}.${p(now.getUTCMonth() + 1)}.${p(now.getUTCDate())}-${p(now.getUTCHours())}${p(now.getUTCMinutes())}`;
}

/** Comma- or space-separated index patterns as a list ([] for none, which means all). */
export function patternList(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

// --- Restore -----------------------------------------------------------------------------------

export interface RestoreForm {
  /** null: every index (and data stream) in the snapshot. */
  indices: string[] | null;
  rename: { pattern: string; replacement: string } | null;
  includeAliases: boolean;
  includeGlobalState: boolean;
  partial: boolean;
}

export const RESTORE_DEFAULTS: RestoreForm = { indices: null, rename: null, includeAliases: true, includeGlobalState: false, partial: false };

/** `POST _snapshot/<repo>/<snapshot>/_restore`: only what differs from Elasticsearch's defaults. */
export function restoreRequest(repo: string, snapshot: string, f: RestoreForm): { path: string; body: Record<string, unknown> } {
  const body: Record<string, unknown> = {};
  if (f.indices) body.indices = f.indices.join(",");
  if (f.rename && f.rename.pattern) {
    body.rename_pattern = f.rename.pattern;
    body.rename_replacement = f.rename.replacement;
  }
  if (!f.includeAliases) body.include_aliases = false;
  if (f.includeGlobalState) body.include_global_state = true;
  if (f.partial) body.partial = true;
  return { path: `/_snapshot/${encodeURIComponent(repo)}/${encodeURIComponent(snapshot)}/_restore`, body };
}

/** The names indices restore under: Java's `replaceAll` with `$1`-style groups, as JS does it. */
export function restoredNames(indices: string[], rename: RestoreForm["rename"]): string[] {
  if (!rename?.pattern) return indices;
  let re: RegExp;
  try {
    re = new RegExp(rename.pattern, "g");
  } catch {
    return indices;
  }
  return indices.map((i) => i.replace(re, rename.replacement));
}

/** Why a restore would be refused before it's sent: a bad pattern, or names already open. */
export function restoreProblem(f: RestoreForm, snapshotIndices: string[], openIndices: string[]): string | null {
  if (f.indices && f.indices.length === 0) return "Pick at least one index.";
  if (f.rename?.pattern) {
    try {
      new RegExp(f.rename.pattern);
    } catch {
      return "The rename pattern isn't a valid regular expression.";
    }
  }
  const open = new Set(openIndices);
  const taken = restoredNames(f.indices ?? snapshotIndices.filter((i) => !i.startsWith(".")), f.rename).filter((n) => open.has(n));
  if (taken.length === 0) return null;
  const list = taken.slice(0, 3).join(", ") + (taken.length > 3 ? ` and ${taken.length - 3} more` : "");
  return `${list} ${taken.length === 1 ? "is" : "are"} open: rename on restore, or close or delete ${taken.length === 1 ? "it" : "them"} first.`;
}

/** One restored index's recovery, from `GET <indices>/_recovery`. */
export interface RecoveryProgress {
  index: string;
  /** Recovered bytes over total, 0–100. */
  percent: number;
  done: boolean;
}

interface RecoveryShard {
  stage?: string;
  index?: { size?: { total_in_bytes?: number; recovered_in_bytes?: number; percent?: string } };
}

/** Each index's progress: bytes recovered over all its shards, done when every shard is. */
export function recoveryProgress(r: Record<string, { shards?: RecoveryShard[] }>): RecoveryProgress[] {
  return Object.entries(r).map(([index, { shards = [] }]) => {
    const total = shards.reduce((n, s) => n + (s.index?.size?.total_in_bytes ?? 0), 0);
    const got = shards.reduce((n, s) => n + (s.index?.size?.recovered_in_bytes ?? 0), 0);
    const done = shards.length > 0 && shards.every((s) => s.stage === "DONE");
    const percent = done ? 100 : total > 0 ? Math.min(100, (got / total) * 100) : 0;
    return { index, percent, done };
  });
}

// --- Snapshot lifecycle policies ---------------------------------------------------------------

/** A policy as `GET _slm/policy` gives it. */
export interface SlmPolicyInfo {
  version?: number;
  modified_date_millis?: number;
  policy: {
    name: string;
    schedule: string;
    repository: string;
    config?: { indices?: string | string[]; include_global_state?: boolean; partial?: boolean; ignore_unavailable?: boolean };
    retention?: { expire_after?: string; min_count?: number; max_count?: number };
  };
  last_success?: { snapshot_name: string; time: number; start_time?: number };
  last_failure?: { snapshot_name: string; time: number; details?: string };
  next_execution_millis?: number;
  in_progress?: { name: string; state: string };
  stats?: { snapshots_taken?: number; snapshots_failed?: number; snapshots_deleted?: number };
}

export interface SlmRow {
  id: string;
  info: SlmPolicyInfo;
  /** The last run failed (its failure is newer than its last success). */
  failing: boolean;
}

export function slmRows(r: Record<string, SlmPolicyInfo>): SlmRow[] {
  return Object.entries(r)
    .map(([id, info]) => ({ id, info, failing: !!info.last_failure && (info.last_failure.time ?? 0) > (info.last_success?.time ?? 0) }))
    .sort((a, b) => a.id.localeCompare(b.id));
}

/** The innermost reason of a failure's `details` (Elasticsearch stores the exception as JSON). */
export function failureReason(details: string | undefined): string {
  if (!details) return "";
  try {
    type Cause = { reason?: string; caused_by?: Cause };
    let c = JSON.parse(details) as Cause;
    while (c.caused_by?.reason) c = c.caused_by;
    return c.reason ?? details;
  } catch {
    return details;
  }
}

const DAYS: Record<string, string> = { SUN: "Sundays", MON: "Mondays", TUE: "Tuesdays", WED: "Wednesdays", THU: "Thursdays", FRI: "Fridays", SAT: "Saturdays", "1": "Sundays", "2": "Mondays", "3": "Tuesdays", "4": "Wednesdays", "5": "Thursdays", "6": "Fridays", "7": "Saturdays" };
const UNITS: Record<string, string> = { s: "second", m: "minute", h: "hour", d: "day" };

/**
 * A policy's schedule in words: an interval (`30m`) or the common shapes of a Quartz cron
 * expression (seconds first; times are UTC). Anything else is shown as written.
 */
export function describeSchedule(schedule: string): string {
  const s = schedule.trim();
  const interval = /^(\d+)([smhd])$/.exec(s);
  if (interval) {
    const n = Number(interval[1]);
    return n === 1 ? `Every ${UNITS[interval[2]]}` : `Every ${n} ${UNITS[interval[2]]}s`;
  }
  const f = s.split(/\s+/);
  if (f.length < 6 || f.length > 7 || (f[6] !== undefined && f[6] !== "*")) return s;
  const [sec, min, hour, dom, month, dow] = f;
  const num = (v: string) => /^\d+$/.test(v);
  const any = (v: string) => v === "*" || v === "?";
  if (sec !== "0" || month !== "*") return s;
  const step = /^(?:\*|0)\/(\d+)$/.exec(min);
  if (step && hour === "*" && any(dom) && any(dow)) return `Every ${step[1]} minutes`;
  if (num(min) && hour === "*" && any(dom) && any(dow)) return `Hourly at :${min.padStart(2, "0")}`;
  if (!num(min) || !num(hour)) return s;
  const at = `at ${hour.padStart(2, "0")}:${min.padStart(2, "0")} UTC`;
  if (any(dom) && any(dow)) return `Daily ${at}`;
  if (any(dom) && DAYS[dow.toUpperCase()]) return `${DAYS[dow.toUpperCase()]} ${at}`;
  if (num(dom) && any(dow)) return `Monthly on day ${dom} ${at}`;
  return s;
}

/** A policy's retention in words, or "" for none. */
export function describeRetention(r: SlmPolicyInfo["policy"]["retention"]): string {
  if (!r) return "";
  const parts: string[] = [];
  if (r.expire_after) parts.push(`expire after ${r.expire_after}`);
  if (r.min_count !== undefined && r.max_count !== undefined) parts.push(`keep ${r.min_count}–${r.max_count}`);
  else if (r.min_count !== undefined) parts.push(`keep at least ${r.min_count}`);
  else if (r.max_count !== undefined) parts.push(`keep at most ${r.max_count}`);
  return parts.join(" · ");
}

/** The schedules the policy form offers (Quartz cron, UTC), and a custom one. */
export const SCHEDULE_PRESETS = [
  { label: "Daily at 01:30 UTC", value: "0 30 1 * * ?" },
  { label: "Every hour", value: "0 0 * * * ?" },
  { label: "Every 30 minutes", value: "0 0/30 * * * ?" },
  { label: "Sundays at 02:00 UTC", value: "0 0 2 ? * SUN" },
];

export interface SlmForm {
  id: string;
  name: string;
  schedule: string;
  repository: string;
  indices: string;
  includeGlobalState: boolean;
  expireAfter: string;
  minCount: string;
  maxCount: string;
}

/** `PUT _slm/policy/<id>`'s body (retention only when one of its fields is set). */
export function slmPolicyBody(f: SlmForm): SlmPolicyInfo["policy"] {
  const indices = patternList(f.indices);
  const retention: NonNullable<SlmPolicyInfo["policy"]["retention"]> = {};
  if (f.expireAfter.trim()) retention.expire_after = f.expireAfter.trim();
  if (f.minCount.trim()) retention.min_count = Number(f.minCount);
  if (f.maxCount.trim()) retention.max_count = Number(f.maxCount);
  return {
    schedule: f.schedule.trim(),
    name: f.name.trim(),
    repository: f.repository,
    config: { indices: indices.length ? indices : ["*"], include_global_state: f.includeGlobalState },
    ...(Object.keys(retention).length ? { retention } : {}),
  };
}

/** A duration such as a snapshot's, in the cluster's own units (`1.2s`, `3m`). */
export function formatMillis(ms: number | null): string {
  if (ms === null) return "—";
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)}s`;
  const s = Math.round(ms / 1000);
  if (s < 3600) return s % 60 ? `${Math.floor(s / 60)}m ${s % 60}s` : `${s / 60}m`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}
