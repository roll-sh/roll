"use client";

import { Lock, Plus, Trash2, X } from "lucide-react";
import { useState } from "react";
import { formatCount } from "@/lib/redis-api";
import { formatBytes } from "@/lib/roll-api";
import {
  bucketTagsError,
  lifecycleExtras,
  lifecycleHasAction,
  mimeTypeError,
  normalizeLifecycleRule,
  normalizeMimeType,
  formatSizeLimit,
  RECOVER_OPTIONS,
  retentionLabel,
  SIZE_UNITS,
  sizeLimitBytes,
  splitSize,
  storageApi,
  effectiveBlock,
  type Bucket,
  type CorsRule,
  type LifecycleRule,
  type SizeUnit,
  type StorageInfo,
} from "@/lib/storage-api";
import { iconBtn } from "../tasks/ui";
import { btn, Confirm, input, Panel } from "../ui";
import { PolicyPanel, PublicAccessBlockPanel } from "./access";
import { EventsPanel } from "./events";
import { ReplicationPanel } from "./replication";
import { Field, SaveBar, Segmented, type Run } from "./shared";
import { WebsitePanel } from "./website";

const METHODS = ["GET", "PUT", "POST", "DELETE", "HEAD"];

const csv = (v: string[]) => v.join(", ");
const parseCsv = (v: string) =>
  v
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);

const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

export function SettingsTab({
  bucket,
  info,
  buckets,
  busy,
  run,
  onChanged,
  onDeleted,
}: {
  bucket: Bucket;
  info: StorageInfo;
  buckets: Bucket[];
  busy: string | null;
  run: Run;
  /** Reload the buckets after a panel saved on its own. */
  onChanged: () => Promise<unknown>;
  onDeleted: () => void;
}) {
  return (
    <div className="space-y-4">
      <AccessPanel bucket={bucket} info={info} busy={busy} run={run} />
      <LimitsPanel bucket={bucket} busy={busy} run={run} />
      <PublicAccessBlockPanel bucket={bucket} info={info} onChanged={onChanged} />
      <PolicyPanel bucket={bucket} info={info} onChanged={onChanged} />
      <VersioningPanel bucket={bucket} busy={busy} run={run} />
      <EventsPanel bucket={bucket} onChanged={onChanged} />
      <ReplicationPanel bucket={bucket} buckets={buckets} onChanged={onChanged} />
      <WebsitePanel bucket={bucket} info={info} onChanged={onChanged} />
      <CorsPanel bucket={bucket} busy={busy} run={run} />
      <LifecyclePanel bucket={bucket} busy={busy} run={run} />
      <TagsPanel bucket={bucket} busy={busy} run={run} />
      <DeletePanel bucket={bucket} busy={busy} run={run} onDeleted={onDeleted} />
    </div>
  );
}

function AccessPanel({ bucket, info, busy, run }: { bucket: Bucket; info: StorageInfo; busy: string | null; run: Run }) {
  const [isPublic, setPublic] = useState(bucket.public);
  const [recover, setRecover] = useState(bucket.recover_days);
  const dirty = isPublic !== bucket.public || recover !== bucket.recover_days;
  // Making a bucket public counts as a public policy: BlockPublicPolicy refuses it.
  const blocked = effectiveBlock(bucket.public_access_block, info.block_public_access).block_public_policy;
  const options = RECOVER_OPTIONS.includes(recover) ? RECOVER_OPTIONS : [...RECOVER_OPTIONS, recover].sort((a, b) => a - b);
  return (
    <Panel title="Access and recovery">
      <form
        className="space-y-5 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          run("access", () => storageApi.updateBucket(bucket.name, { public: isPublic, recover_days: recover }));
        }}
      >
        <div className="grid gap-x-3 gap-y-4 sm:grid-cols-2">
          <Field
            label="Access"
            hint={
              isPublic
                ? "anyone with a URL can GET and HEAD objects; listing still needs a key"
                : blocked
                  ? "public access is blocked (below)"
                  : "every request needs an access key"
            }
          >
            {blocked && !bucket.public ? (
              <p className="py-1 font-mono text-[12px] text-muted">private</p>
            ) : (
              <Segmented
                value={isPublic ? "public" : "private"}
                options={[
                  { label: "private", value: "private" },
                  { label: "public read", value: "public" },
                ]}
                onChange={(v) => setPublic(v === "public")}
              />
            )}
          </Field>
          <Field
            label="Recover deleted objects for"
            hint={
              recover === 0
                ? "deletes and overwrites are final"
                : `${formatBytes(bucket.trash_bytes)} recoverable now, billed as storage`
            }
          >
            <select value={recover} onChange={(e) => setRecover(Number(e.target.value))} className={`${input} w-full`}>
              {options.map((d) => (
                <option key={d} value={d}>
                  {d === 0 ? "off" : `${d} day${d === 1 ? "" : "s"}`}
                </option>
              ))}
            </select>
          </Field>
        </div>
        <SaveBar
          dirty={dirty}
          busy={busy === "access"}
          onReset={() => {
            setPublic(bucket.public);
            setRecover(bucket.recover_days);
          }}
        />
      </form>
    </Panel>
  );
}

const COMMON_TYPES = ["image/*", "video/*", "audio/*", "application/pdf", "text/plain"];

/** The developer API's upload limits (Supabase's bucket settings), enforced on every write path. */
function LimitsPanel({ bucket, busy, run }: { bucket: Bucket; busy: string | null; run: Run }) {
  const savedSize = (): { value: string; unit: SizeUnit } =>
    bucket.file_size_limit == null ? { value: "", unit: "MB" } : splitSize(bucket.file_size_limit);
  const savedTypes = bucket.allowed_mime_types ?? [];
  const [size, setSize] = useState(savedSize);
  const [types, setTypes] = useState<string[]>(savedTypes);
  const [draft, setDraft] = useState("");
  const parsed = sizeLimitBytes(size.value, size.unit);
  const draftErr = draft.trim() ? mimeTypeError(draft) : null;
  const dirty = (!parsed.error && parsed.bytes !== bucket.file_size_limit) || !same(types, savedTypes) || !!draft.trim();
  const add = (raw: string) => {
    const items = raw
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
    if (items.some((s) => mimeTypeError(s))) return false;
    setTypes((t) => [...t, ...items.map(normalizeMimeType).filter((m, i, all) => !t.includes(m) && all.indexOf(m) === i)]);
    return true;
  };
  const current =
    bucket.file_size_limit == null && !bucket.allowed_mime_types?.length
      ? "no limits"
      : [
          bucket.file_size_limit != null && `up to ${formatSizeLimit(bucket.file_size_limit)}`,
          bucket.allowed_mime_types?.length && bucket.allowed_mime_types.join(", "),
        ]
          .filter(Boolean)
          .join(" · ");
  return (
    <Panel
      title="Upload limits"
      aside={
        <span className="max-w-[60%] truncate font-mono text-[12px] text-subtle" title={current}>
          {current}
        </span>
      }
    >
      <form
        className="space-y-5 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (parsed.error || draftErr) return;
          // A type still in the box counts: Save shouldn't drop what was typed.
          const pending = draft.trim()
            ? draft
                .split(",")
                .map((s) => normalizeMimeType(s))
                .filter((m) => m && !types.includes(m))
            : [];
          const all = [...types, ...pending];
          setTypes(all);
          setDraft("");
          run("limits", () =>
            storageApi.setUploadLimits(bucket.name, { file_size_limit: parsed.bytes, allowed_mime_types: all.length ? all : null }),
          );
        }}
      >
        <p className="text-[13px] text-muted">
          Refuse objects that are too large or of the wrong type, on every write: the developer API, the S3 API and uploads here.
        </p>
        <div className="grid gap-x-3 gap-y-4 sm:grid-cols-2">
          <Field label="Max file size" hint={parsed.error ?? (parsed.bytes == null ? "empty: no limit" : `${parsed.bytes.toLocaleString("en-US")} bytes · decimal units`)}>
            <span className="flex gap-2">
              <input
                value={size.value}
                onChange={(e) => setSize((s) => ({ ...s, value: e.target.value }))}
                inputMode="decimal"
                placeholder="no limit"
                aria-invalid={!!parsed.error}
                className={`${input} min-w-0 flex-1 font-mono text-[12px] ${parsed.error ? "border-danger/50" : ""}`}
              />
              <select
                value={size.unit}
                onChange={(e) => setSize((s) => ({ ...s, unit: e.target.value as SizeUnit }))}
                aria-label="Unit"
                className={`${input} w-20 font-mono text-[12px]`}
              >
                {SIZE_UNITS.map((u) => (
                  <option key={u.unit} value={u.unit}>
                    {u.unit}
                  </option>
                ))}
              </select>
            </span>
          </Field>
          <div className="min-w-0 space-y-1">
            <span className="block text-[11px] text-subtle">Allowed MIME types</span>
            <div className="flex min-h-9 flex-wrap items-center gap-1 rounded-lg border border-line bg-bg px-1.5 py-1 focus-within:border-line-2">
              {types.map((t) => (
                <span key={t} className="rise inline-flex items-center gap-1 rounded-md border border-line bg-panel-2 py-0.5 pr-0.5 pl-1.5 font-mono text-[11px]">
                  {t}
                  <button
                    type="button"
                    onClick={() => setTypes((x) => x.filter((y) => y !== t))}
                    className="rounded p-0.5 text-subtle transition hover:text-fg"
                    aria-label={`Remove ${t}`}
                  >
                    <X className="size-3" strokeWidth={1.5} />
                  </button>
                </span>
              ))}
              <input
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                onKeyDown={(e) => {
                  if ((e.key === "Enter" || e.key === ",") && draft.trim()) {
                    e.preventDefault();
                    if (add(draft)) setDraft("");
                  } else if (e.key === "Backspace" && !draft && types.length) {
                    setTypes((x) => x.slice(0, -1));
                  }
                }}
                onBlur={() => draft.trim() && add(draft) && setDraft("")}
                placeholder={types.length ? "" : "any type"}
                spellCheck={false}
                aria-label="Add an allowed MIME type"
                aria-invalid={!!draftErr}
                className="min-w-24 flex-1 bg-transparent px-1 py-0.5 font-mono text-[12px] outline-none placeholder:text-subtle"
              />
            </div>
            <span className={`block text-[11px] ${draftErr ? "text-danger" : "text-subtle"}`}>
              {draftErr ?? "Enter or comma to add; wildcards like image/* work; empty: any type"}
            </span>
            <span className="flex flex-wrap gap-1 pt-1">
              {COMMON_TYPES.filter((t) => !types.includes(t)).map((t) => (
                <button
                  key={t}
                  type="button"
                  onClick={() => setTypes((x) => [...x, t])}
                  className="inline-flex items-center gap-1 rounded-md border border-line px-1.5 py-0.5 font-mono text-[11px] text-subtle transition hover:text-fg"
                >
                  <Plus className="size-3" strokeWidth={1.5} /> {t}
                </button>
              ))}
            </span>
          </div>
        </div>
        <SaveBar
          dirty={dirty}
          busy={busy === "limits"}
          onReset={() => {
            setSize(savedSize());
            setTypes(savedTypes);
            setDraft("");
          }}
        />
      </form>
    </Panel>
  );
}

type VersioningChoice = "off" | "Enabled" | "Suspended";

function VersioningPanel({ bucket, busy, run }: { bucket: Bucket; busy: string | null; run: Run }) {
  const current: VersioningChoice = bucket.versioning ?? "off";
  const [choice, setChoice] = useState<VersioningChoice>(current);
  const dirty = choice !== current;
  const locked = bucket.object_lock !== null;
  const retention = retentionLabel(bucket.object_lock);
  // Never versioned: off or on. Once versioned: on or suspended, never back to off.
  const options: { label: string; value: VersioningChoice }[] =
    bucket.versioning === null
      ? [
          { label: "off", value: "off" },
          { label: "enabled", value: "Enabled" },
        ]
      : [
          { label: "enabled", value: "Enabled" },
          { label: "suspended", value: "Suspended" },
        ];
  const hint =
    choice === "Enabled"
      ? "overwrites and deletes keep the old version"
      : choice === "Suspended"
        ? "new writes replace the null version; old ones stay"
        : "overwrites and deletes go to Recently deleted";
  return (
    <Panel title="Versioning">
      <form
        className="space-y-5 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (dirty && choice !== "off") run("versioning", () => storageApi.updateBucket(bucket.name, { versioning: choice }));
        }}
      >
        <div className="grid gap-x-3 gap-y-4 sm:grid-cols-2">
          <Field label="State" hint={hint}>
            {locked ? (
              <p className="py-1 font-mono text-[12px] text-muted">enabled</p>
            ) : (
              <Segmented value={choice} options={options} onChange={setChoice} />
            )}
          </Field>
          <Field
            label="Older versions"
            hint={bucket.versioning === null ? "none: the bucket was never versioned" : "billed as storage, like S3"}
          >
            <p className="py-1 font-mono text-[12px] tabular-nums">
              {formatCount(bucket.versions)} · {formatBytes(bucket.version_bytes)}
            </p>
          </Field>
        </div>
        <div className="flex items-start gap-2 rounded-lg border border-line bg-bg px-3 py-2.5 text-[12px]">
          <Lock className="mt-0.5 size-3.5 shrink-0 text-subtle" strokeWidth={1.5} />
          <p className="text-muted">
            <span className="text-fg">Object Lock {locked ? "on" : "off"}</span>
            {locked
              ? ` · ${retention ? `new versions are retained ${retention}` : "no default retention"}. Versioning stays on, and locked versions can't be deleted. Retention and legal holds are set through the S3 API.`
              : " · it can only be turned on when a bucket is created."}
          </p>
        </div>
        {bucket.versioning === null && choice === "Enabled" && (
          <p className="rise text-[12px] text-muted">
            Once enabled, versioning can be suspended but not turned off: the bucket can&apos;t go back to unversioned.
          </p>
        )}
        {!locked && (
          <SaveBar
            dirty={dirty}
            busy={busy === "versioning"}
            onReset={() => setChoice(current)}
            label={
              !dirty
                ? "Save"
                : choice === "Enabled"
                  ? bucket.versioning === null
                    ? "Enable versioning"
                    : "Resume versioning"
                  : "Suspend versioning"
            }
          />
        )}
      </form>
    </Panel>
  );
}

/** A CORS rule as edited: list fields stay comma-separated text until saved. */
interface CorsDraft {
  origins: string;
  methods: string[];
  headers: string;
  expose: string;
  max_age: string;
}

const toDraft = (r: CorsRule): CorsDraft => ({
  origins: csv(r.allowed_origins),
  methods: r.allowed_methods,
  headers: csv(r.allowed_headers),
  expose: csv(r.expose_headers),
  max_age: r.max_age_s == null ? "" : String(r.max_age_s),
});

const fromDraft = (d: CorsDraft): CorsRule => ({
  allowed_origins: parseCsv(d.origins),
  allowed_methods: d.methods,
  allowed_headers: parseCsv(d.headers),
  expose_headers: parseCsv(d.expose),
  max_age_s: d.max_age === "" ? null : Math.max(0, Math.floor(Number(d.max_age)) || 0),
});

const emptyCors = (): CorsDraft => ({ origins: "*", methods: ["GET", "HEAD"], headers: "*", expose: "ETag", max_age: "3600" });

function CorsPanel({ bucket, busy, run }: { bucket: Bucket; busy: string | null; run: Run }) {
  const [drafts, setDrafts] = useState<CorsDraft[]>(() => bucket.cors.map(toDraft));
  const rules = drafts.map(fromDraft);
  const dirty = !same(rules, bucket.cors);
  const set = (i: number, patch: Partial<CorsDraft>) => setDrafts((r) => r.map((x, j) => (j === i ? { ...x, ...patch } : x)));
  const valid = rules.every((r) => r.allowed_origins.length > 0 && r.allowed_methods.length > 0);
  const text = (i: number, field: "origins" | "headers" | "expose", placeholder?: string) => (
    <input
      value={drafts[i][field]}
      onChange={(e) => set(i, { [field]: e.target.value })}
      placeholder={placeholder}
      spellCheck={false}
      className={`${input} w-full font-mono text-[12px]`}
    />
  );
  return (
    <Panel
      title="CORS"
      aside={<span className="text-[12px] text-subtle">for browsers calling the S3 API directly (presigned uploads)</span>}
    >
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (valid) run("cors", () => storageApi.updateBucket(bucket.name, { cors: rules }));
        }}
      >
        {drafts.length === 0 && <p className="text-[13px] text-muted">No CORS rules: browsers on other origins can&apos;t call this bucket.</p>}
        {drafts.map((r, i) => (
          <div key={i} className="rise space-y-3 rounded-lg border border-line bg-bg p-4">
            <div className="flex items-center justify-between">
              <span className="font-mono text-[11px] text-subtle">rule {i + 1}</span>
              <button
                type="button"
                onClick={() => setDrafts((x) => x.filter((_, j) => j !== i))}
                className={`${iconBtn} hover:text-danger`}
                aria-label={`Remove rule ${i + 1}`}
              >
                <Trash2 className="size-3.5" />
              </button>
            </div>
            <div className="grid gap-x-3 gap-y-3 sm:grid-cols-2">
              <Field label="Allowed origins" hint="comma separated; * for any">
                {text(i, "origins", "https://app.example.com")}
              </Field>
              <Field label="Allowed methods">
                <div className="flex flex-wrap gap-1">
                  {METHODS.map((m) => {
                    const on = r.methods.includes(m);
                    return (
                      <button
                        key={m}
                        type="button"
                        aria-pressed={on}
                        onClick={() => set(i, { methods: on ? r.methods.filter((x) => x !== m) : [...r.methods, m] })}
                        className={`rounded-md border px-2 py-1 font-mono text-[11px] transition ${
                          on ? "border-line-2 bg-panel-2 text-fg" : "border-line text-subtle hover:text-fg"
                        }`}
                      >
                        {m}
                      </button>
                    );
                  })}
                </div>
              </Field>
              <Field label="Allowed headers" hint="request headers; * for any">
                {text(i, "headers")}
              </Field>
              <Field label="Expose headers" hint="readable by the page, e.g. ETag">
                {text(i, "expose")}
              </Field>
              <Field label="Max age (seconds)" hint="how long browsers cache the preflight">
                <input
                  type="number"
                  min={0}
                  value={r.max_age}
                  onChange={(e) => set(i, { max_age: e.target.value })}
                  className={`${input} w-full font-mono text-[12px]`}
                />
              </Field>
            </div>
          </div>
        ))}
        <button type="button" onClick={() => setDrafts((r) => [...r, emptyCors()])} className={btn}>
          <Plus className="size-3.5" /> Add rule
        </button>
        {!valid && <p className="text-[12px] text-danger">Every rule needs at least one origin and one method.</p>}
        <SaveBar dirty={dirty} busy={busy === "cors"} onReset={() => setDrafts(bucket.cors.map(toDraft))} />
      </form>
    </Panel>
  );
}

let ruleSeq = 0;
const emptyLifecycle = (): LifecycleRule => ({
  id: `rule-${Date.now().toString(36)}${ruleSeq++}`,
  prefix: "",
  enabled: true,
  expire_days: 30,
  abort_multipart_days: 7,
});

function LifecyclePanel({ bucket, busy, run }: { bucket: Bucket; busy: string | null; run: Run }) {
  const [rules, setRules] = useState<LifecycleRule[]>(bucket.lifecycle);
  // The server fills in the optional fields: compare with them filled in on both sides.
  const dirty = !same(rules.map(normalizeLifecycleRule), bucket.lifecycle.map(normalizeLifecycleRule));
  const set = (i: number, patch: Partial<LifecycleRule>) => setRules((r) => r.map((x, j) => (j === i ? { ...x, ...patch } : x)));
  const days = (v: string) => (v === "" ? null : Math.max(1, Math.floor(Number(v))));
  const valid = rules.every((r) => r.id.trim() && lifecycleHasAction(r));
  const versioned = bucket.versioning !== null;
  return (
    <Panel title="Lifecycle" aside={<span className="text-[12px] text-subtle">runs daily</span>}>
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (valid) run("lifecycle", () => storageApi.updateBucket(bucket.name, { lifecycle: rules }));
        }}
      >
        <p className="text-[13px] text-muted">
          Expire objects by age under a prefix, and abort multipart uploads that were never completed.
          {versioned
            ? " In a versioned bucket, expiring adds a delete marker; older versions expire by how long they've been noncurrent."
            : bucket.recover_days > 0 && ` Expired objects go to Recently deleted for ${bucket.recover_days} days first.`}
        </p>
        {rules.map((r, i) => (
          <div
            key={r.id + i}
            className={`rise grid items-end gap-3 rounded-lg border border-line bg-bg p-4 ${
              versioned
                ? "sm:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_7rem_7rem_7rem_auto_auto]"
                : "sm:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_8rem_8rem_auto_auto]"
            }`}
          >
            <Field label="Rule id">
              <input value={r.id} onChange={(e) => set(i, { id: e.target.value })} spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
            </Field>
            <Field label="Prefix" hint={r.prefix ? undefined : "whole bucket"}>
              <input
                value={r.prefix}
                onChange={(e) => set(i, { prefix: e.target.value })}
                placeholder="logs/"
                spellCheck={false}
                className={`${input} w-full font-mono text-[12px]`}
              />
            </Field>
            <Field label="Expire after (days)">
              <input
                type="number"
                min={1}
                value={r.expire_days ?? ""}
                placeholder="never"
                onChange={(e) => set(i, { expire_days: days(e.target.value) })}
                className={`${input} w-full font-mono text-[12px]`}
              />
            </Field>
            <Field label="Abort uploads (days)">
              <input
                type="number"
                min={1}
                value={r.abort_multipart_days ?? ""}
                placeholder="7"
                onChange={(e) => set(i, { abort_multipart_days: days(e.target.value) })}
                className={`${input} w-full font-mono text-[12px]`}
              />
            </Field>
            {versioned && (
              <Field label="Older versions (days)">
                <input
                  type="number"
                  min={1}
                  value={r.noncurrent_days ?? ""}
                  placeholder="keep"
                  onChange={(e) => set(i, { noncurrent_days: days(e.target.value) })}
                  className={`${input} w-full font-mono text-[12px]`}
                />
              </Field>
            )}
            <label className="flex items-center gap-2 pb-2 text-[12px] text-muted">
              <input type="checkbox" checked={r.enabled} onChange={(e) => set(i, { enabled: e.target.checked })} className="accent-accent" />
              enabled
            </label>
            <button type="button" onClick={() => setRules((x) => x.filter((_, j) => j !== i))} className={`${iconBtn} mb-1 hover:text-danger`} aria-label={`Remove ${r.id}`}>
              <X className="size-3.5" />
            </button>
            {lifecycleExtras(r).length > 0 && (
              <p className="col-span-full font-mono text-[11px] text-subtle" title="Set through the S3 API; kept as is">
                also: {lifecycleExtras(r).join(" · ")}
              </p>
            )}
          </div>
        ))}
        <button type="button" onClick={() => setRules((r) => [...r, emptyLifecycle()])} className={btn}>
          <Plus className="size-3.5" /> Add rule
        </button>
        {!valid && <p className="text-[12px] text-danger">Every rule needs an id and something to do: an expiry or an upload abort.</p>}
        <SaveBar dirty={dirty} busy={busy === "lifecycle"} onReset={() => setRules(bucket.lifecycle)} />
      </form>
    </Panel>
  );
}

function TagsPanel({ bucket, busy, run }: { bucket: Bucket; busy: string | null; run: Run }) {
  const initial = () => Object.entries(bucket.tags ?? {});
  const [tags, setTags] = useState<[string, string][]>(initial);
  const err = bucketTagsError(tags);
  const dirty = !same(Object.fromEntries(tags), bucket.tags ?? {}) || tags.length !== Object.keys(bucket.tags ?? {}).length;
  const set = (i: number, at: 0 | 1, v: string) => setTags((t) => t.map((x, j) => (j === i ? ((at === 0 ? [v, x[1]] : [x[0], v]) as [string, string]) : x)));
  return (
    <Panel title="Tags" aside={<span className="text-[12px] text-subtle">bucket tags, for cost allocation and policies</span>}>
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (!err) run("tags", () => storageApi.updateBucket(bucket.name, { tags: Object.fromEntries(tags) }));
        }}
      >
        {tags.length === 0 && <p className="text-[13px] text-muted">No tags.</p>}
        {tags.length > 0 && (
          <div className="space-y-2">
            {tags.map(([k, v], i) => (
              <div key={i} className="rise grid grid-cols-[minmax(0,1fr)_minmax(0,1.5fr)_auto] items-center gap-2">
                <input
                  value={k}
                  onChange={(e) => set(i, 0, e.target.value)}
                  placeholder="key"
                  aria-label={`Tag ${i + 1} key`}
                  spellCheck={false}
                  className={`${input} w-full font-mono text-[12px]`}
                />
                <input
                  value={v}
                  onChange={(e) => set(i, 1, e.target.value)}
                  placeholder="value"
                  aria-label={`Tag ${i + 1} value`}
                  spellCheck={false}
                  className={`${input} w-full font-mono text-[12px]`}
                />
                <button
                  type="button"
                  onClick={() => setTags((t) => t.filter((_, j) => j !== i))}
                  className={`${iconBtn} hover:text-danger`}
                  aria-label={`Remove tag ${k || i + 1}`}
                >
                  <X className="size-3.5" />
                </button>
              </div>
            ))}
          </div>
        )}
        <button type="button" onClick={() => setTags((t) => [...t, ["", ""]])} disabled={tags.length >= 50} className={btn}>
          <Plus className="size-3.5" /> Add tag
        </button>
        {err && tags.length > 0 && <p className="text-[12px] text-danger">{err}</p>}
        <SaveBar dirty={dirty} busy={busy === "tags"} onReset={() => setTags(initial())} />
      </form>
    </Panel>
  );
}

function DeletePanel({
  bucket,
  busy,
  run,
  onDeleted,
}: {
  bucket: Bucket;
  busy: string | null;
  run: Run;
  onDeleted: () => void;
}) {
  const [deleting, setDeleting] = useState(false);
  const hasBranches = bucket.branches.length > 0;
  // Older versions and delete markers count too: S3 won't delete a bucket that still has any.
  const nonEmpty = bucket.objects > 0 || bucket.versions > 0;
  const versions = bucket.versions > 0 ? ` and ${bucket.versions.toLocaleString("en-US")} older version${bucket.versions === 1 ? "" : "s"}` : "";
  return (
    <Panel title="Delete bucket">
      <div className="space-y-3 px-5 py-4">
        {hasBranches ? (
          <p className="text-[13px] text-muted">
            This bucket has {bucket.branches.length} branch{bucket.branches.length === 1 ? "" : "es"} (
            <span className="font-mono">{bucket.branches.join(", ")}</span>) that read its objects. Delete them first.
          </p>
        ) : (
          <p className="text-[13px] text-muted">
            {nonEmpty
              ? `Deletes the bucket and its ${bucket.objects.toLocaleString("en-US")} object${bucket.objects === 1 ? "" : "s"}${versions} (${formatBytes(bucket.bytes + bucket.version_bytes)}). Unlike single deletes, objects aren't kept in Recently deleted.`
              : "The bucket is empty."}
          </p>
        )}
        {deleting ? (
          <Confirm
            word={bucket.name}
            action={nonEmpty ? "Delete bucket and objects" : "Delete bucket"}
            busy={busy === "delete"}
            onCancel={() => setDeleting(false)}
            onConfirm={async () => {
              const r = await run("delete", () => storageApi.deleteBucket(bucket.name, nonEmpty));
              if (r) onDeleted();
            }}
          >
            This can&apos;t be undone.
          </Confirm>
        ) : (
          <button
            onClick={() => setDeleting(true)}
            disabled={hasBranches}
            className={`${btn} hover:border-danger/50 hover:text-danger`}
          >
            <Trash2 className="size-3.5" /> Delete {bucket.name}
          </button>
        )}
      </div>
    </Panel>
  );
}
