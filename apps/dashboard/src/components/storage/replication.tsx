"use client";

import { ArrowRight, Plus, Trash2 } from "lucide-react";
import Link from "next/link";
import { useState } from "react";
import { replicationInput, replicationStatus, storageApi, type Bucket, type ReplicationRule } from "@/lib/storage-api";
import { iconBtn } from "../tasks/ui";
import { btn, ErrorNote, input, Panel, primary } from "../ui";
import { Check, Field, InlineConfirm, useAction } from "./shared";

const href = (name: string) => `/dashboard/storage/${encodeURIComponent(name)}`;

/** Replication rules to other roll buckets, with how each is doing. */
export function ReplicationPanel({
  bucket,
  buckets,
  onChanged,
}: {
  bucket: Bucket;
  buckets: Bucket[];
  onChanged: () => Promise<unknown>;
}) {
  const { busy, error, run } = useAction(onChanged);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<string | null>(null);
  const rules = bucket.replication?.rules ?? [];
  const versioned = bucket.versioning === "Enabled";
  const targets = buckets.filter((b) => b.name !== bucket.name);
  const save = (key: string, next: ReplicationRule[]) => run(key, () => storageApi.setReplication(bucket.name, next.map(replicationInput)));
  const enableVersioning = (name: string) => run(`ver-${name}`, () => storageApi.updateBucket(name, { versioning: "Enabled" }));

  return (
    <Panel title="Replication" aside={<span className="text-[12px] text-subtle">to other roll buckets</span>}>
      <div className="space-y-4 px-5 py-4">
        <p className="text-[13px] text-muted">
          Copy every new version to another bucket. Inside roll, a replica shares the original&apos;s bytes: nothing is copied or
          transferred, so replication is <span className="text-fg">instant and free</span>, with no egress or request charges.
        </p>
        <ErrorNote error={error} />
        {!versioned && (
          <div className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-line bg-bg px-3 py-2.5 text-[12px]">
            <p className="text-muted">Replication needs versioning enabled on this bucket{bucket.versioning === "Suspended" ? " (it's suspended)" : ""}.</p>
            <button type="button" disabled={busy !== null} onClick={() => enableVersioning(bucket.name)} className={btn}>
              {busy === `ver-${bucket.name}` ? <span className="shimmer">Enabling…</span> : "Enable versioning"}
            </button>
          </div>
        )}
        {rules.length === 0 ? (
          <p className="text-[13px] text-subtle">No rules.</p>
        ) : (
          <ul className="overflow-hidden rounded-lg border border-line">
            {rules.map((r) => {
              const status = replicationStatus(r);
              const active = r.enabled && (r.pending > 0 || r.backfilling) && !r.last_error;
              return (
                <li key={r.id} className="rise flex items-start gap-3 border-b border-line px-4 py-3 last:border-0">
                  <div className="min-w-0 flex-1 space-y-0.5">
                    <p className="flex min-w-0 items-center gap-2 text-[13px]">
                      <span className="truncate font-mono text-[12px] text-subtle">{r.prefix || "every key"}</span>
                      <ArrowRight className="size-3.5 shrink-0 text-subtle" />
                      {buckets.some((b) => b.name === r.destination) ? (
                        <Link href={href(r.destination)} className="truncate font-mono text-[12px] text-accent hover:underline">
                          {r.destination}
                        </Link>
                      ) : (
                        <span className="truncate font-mono text-[12px]">{r.destination}</span>
                      )}
                    </p>
                    <p className="truncate font-mono text-[11px] text-subtle">
                      {r.id}
                      {r.delete_markers && " · delete markers"}
                      {r.existing_objects && " · existing objects"}
                      {Object.entries(r.tags).map(([k, v]) => ` · tag ${k}=${v}`)}
                    </p>
                    <p className="flex items-center gap-1.5 text-[12px] text-muted">
                      <span className={`size-1.5 rounded-full ${active ? "bg-accent" : r.last_error ? "bg-danger" : r.enabled ? "bg-ok" : "bg-line-2"}`} aria-hidden />
                      {status}
                      {r.last_error && <span className="truncate text-danger"> · {r.last_error}</span>}
                    </p>
                  </div>
                  {removing === r.id ? (
                    <InlineConfirm
                      label="Remove"
                      busy={busy === `rm-${r.id}`}
                      onConfirm={async () => {
                        await save(`rm-${r.id}`, rules.filter((x) => x.id !== r.id));
                        setRemoving(null);
                      }}
                      onCancel={() => setRemoving(null)}
                    />
                  ) : (
                    <span className="flex shrink-0 items-center gap-1">
                      <button
                        type="button"
                        disabled={busy !== null}
                        onClick={() => save(`tg-${r.id}`, rules.map((x) => (x.id === r.id ? { ...x, enabled: !x.enabled } : x)))}
                        className={btn}
                      >
                        {busy === `tg-${r.id}` ? <span className="shimmer">Saving…</span> : r.enabled ? "Pause" : "Resume"}
                      </button>
                      <button type="button" onClick={() => setRemoving(r.id)} className={`${iconBtn} hover:text-danger`} aria-label={`Remove rule ${r.id}`} title="Remove">
                        <Trash2 className="size-3.5" />
                      </button>
                    </span>
                  )}
                </li>
              );
            })}
          </ul>
        )}
        {adding ? (
          <AddRule
            targets={targets}
            sourceVersioned={versioned}
            busy={busy}
            onEnableVersioning={enableVersioning}
            onCancel={() => setAdding(false)}
            onAdd={async (rule) => {
              const r = await run("add", () => storageApi.setReplication(bucket.name, [...rules.map(replicationInput), rule]));
              if (r) setAdding(false);
            }}
          />
        ) : (
          <button type="button" onClick={() => setAdding(true)} disabled={targets.length === 0} className={btn}>
            <Plus className="size-3.5" /> Add rule
          </button>
        )}
        {targets.length === 0 && <p className="text-[12px] text-subtle">Create another bucket to replicate to.</p>}
      </div>
    </Panel>
  );
}

function AddRule({
  targets,
  sourceVersioned,
  busy,
  onEnableVersioning,
  onAdd,
  onCancel,
}: {
  targets: Bucket[];
  sourceVersioned: boolean;
  busy: string | null;
  onEnableVersioning: (name: string) => void;
  onAdd: (r: { destination: string; prefix: string; delete_markers: boolean; existing_objects: boolean }) => void;
  onCancel: () => void;
}) {
  const [destination, setDestination] = useState(targets[0]?.name ?? "");
  const [prefix, setPrefix] = useState("");
  const [markers, setMarkers] = useState(true);
  const [existing, setExisting] = useState(false);
  const dest = targets.find((b) => b.name === destination);
  const destVersioned = dest?.versioning === "Enabled";
  const ready = !!dest && destVersioned && sourceVersioned;
  return (
    <form
      className="rise space-y-4 rounded-lg border border-line bg-bg p-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (ready) onAdd({ destination, prefix, delete_markers: markers, existing_objects: existing });
      }}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Destination bucket">
          <select value={destination} onChange={(e) => setDestination(e.target.value)} className={`${input} w-full font-mono text-[12px]`}>
            {targets.map((b) => (
              <option key={b.id} value={b.name}>
                {b.name}
              </option>
            ))}
          </select>
        </Field>
        <Field label="Key prefix" hint={prefix ? undefined : "every key"}>
          <input
            value={prefix}
            onChange={(e) => setPrefix(e.target.value)}
            placeholder="images/"
            spellCheck={false}
            className={`${input} w-full font-mono text-[12px]`}
          />
        </Field>
      </div>
      {dest && !destVersioned && (
        <div className="flex flex-wrap items-center justify-between gap-3 text-[12px]">
          <p className="text-muted">
            <span className="font-mono">{dest.name}</span> needs versioning enabled to receive replicas.
          </p>
          <button type="button" disabled={busy !== null} onClick={() => onEnableVersioning(dest.name)} className={btn}>
            {busy === `ver-${dest.name}` ? <span className="shimmer">Enabling…</span> : `Enable on ${dest.name}`}
          </button>
        </div>
      )}
      <div className="grid gap-3 sm:grid-cols-2">
        <Check checked={markers} onChange={setMarkers} label="Replicate deletes" hint="delete markers follow to the replica" />
        <Check checked={existing} onChange={setExisting} label="Existing objects too" hint="not just versions written from now on" />
      </div>
      <div className="flex gap-2">
        <button type="submit" disabled={busy === "add" || !ready} className={primary}>
          {busy === "add" ? <span className="shimmer">Adding…</span> : "Add rule"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </form>
  );
}
