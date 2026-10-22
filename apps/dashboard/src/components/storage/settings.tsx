"use client";

import { Plus, Trash2, X } from "lucide-react";
import { useState } from "react";
import { formatBytes } from "@/lib/roll-api";
import { RECOVER_OPTIONS, storageApi, type Bucket, type CorsRule, type LifecycleRule } from "@/lib/storage-api";
import { iconBtn } from "../tasks/ui";
import { btn, Confirm, input, Panel, primary } from "../ui";
import { Field, Segmented, type Run } from "./shared";

const METHODS = ["GET", "PUT", "POST", "DELETE", "HEAD"];

const csv = (v: string[]) => v.join(", ");
const parseCsv = (v: string) =>
  v
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);

const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

function SaveBar({ dirty, busy, onReset, label = "Save" }: { dirty: boolean; busy: boolean; onReset: () => void; label?: string }) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line pt-4">
      <span className="text-[12px] text-subtle">{dirty ? "Unsaved changes" : "Up to date"}</span>
      <div className="flex gap-2">
        {dirty && (
          <button type="button" onClick={onReset} className={btn}>
            Reset
          </button>
        )}
        <button type="submit" disabled={busy || !dirty} className={primary}>
          {busy ? <span className="shimmer">Saving…</span> : label}
        </button>
      </div>
    </div>
  );
}

export function SettingsTab({
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
  return (
    <div className="space-y-4">
      <AccessPanel bucket={bucket} busy={busy} run={run} />
      <CorsPanel bucket={bucket} busy={busy} run={run} />
      <LifecyclePanel bucket={bucket} busy={busy} run={run} />
      <DeletePanel bucket={bucket} busy={busy} run={run} onDeleted={onDeleted} />
    </div>
  );
}

function AccessPanel({ bucket, busy, run }: { bucket: Bucket; busy: string | null; run: Run }) {
  const [isPublic, setPublic] = useState(bucket.public);
  const [recover, setRecover] = useState(bucket.recover_days);
  const dirty = isPublic !== bucket.public || recover !== bucket.recover_days;
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
            hint={isPublic ? "anyone with a URL can GET and HEAD objects; listing still needs a key" : "every request needs an access key"}
          >
            <Segmented
              value={isPublic ? "public" : "private"}
              options={[
                { label: "private", value: "private" },
                { label: "public read", value: "public" },
              ]}
              onChange={(v) => setPublic(v === "public")}
            />
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
  const dirty = !same(rules, bucket.lifecycle);
  const set = (i: number, patch: Partial<LifecycleRule>) => setRules((r) => r.map((x, j) => (j === i ? { ...x, ...patch } : x)));
  const days = (v: string) => (v === "" ? null : Math.max(1, Math.floor(Number(v))));
  const valid = rules.every((r) => r.id.trim() && (r.expire_days != null || r.abort_multipart_days != null));
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
          {bucket.recover_days > 0 && ` Expired objects go to Recently deleted for ${bucket.recover_days} days first.`}
        </p>
        {rules.map((r, i) => (
          <div key={r.id + i} className="rise grid items-end gap-3 rounded-lg border border-line bg-bg p-4 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_8rem_8rem_auto_auto]">
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
            <label className="flex items-center gap-2 pb-2 text-[12px] text-muted">
              <input type="checkbox" checked={r.enabled} onChange={(e) => set(i, { enabled: e.target.checked })} className="accent-accent" />
              enabled
            </label>
            <button type="button" onClick={() => setRules((x) => x.filter((_, j) => j !== i))} className={`${iconBtn} mb-1 hover:text-danger`} aria-label={`Remove ${r.id}`}>
              <X className="size-3.5" />
            </button>
          </div>
        ))}
        <button type="button" onClick={() => setRules((r) => [...r, emptyLifecycle()])} className={btn}>
          <Plus className="size-3.5" /> Add rule
        </button>
        {!valid && <p className="text-[12px] text-danger">Every rule needs an id and an expiry or an upload abort.</p>}
        <SaveBar dirty={dirty} busy={busy === "lifecycle"} onReset={() => setRules(bucket.lifecycle)} />
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
  const nonEmpty = bucket.objects > 0;
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
              ? `Deletes the bucket and its ${bucket.objects.toLocaleString("en-US")} object${bucket.objects === 1 ? "" : "s"} (${formatBytes(bucket.bytes)}). Unlike single deletes, objects aren't kept in Recently deleted.`
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
