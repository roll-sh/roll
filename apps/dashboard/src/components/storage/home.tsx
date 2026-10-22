"use client";

import { ChevronRight, GitBranch, Plus } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import { formatCount } from "@/lib/redis-api";
import { formatBytes } from "@/lib/roll-api";
import {
  agoMs,
  bucketNameError,
  RECOVER_OPTIONS,
  storageApi,
  usd,
  type Bucket,
  type StorageInfo,
  type StorageUsageAll,
} from "@/lib/storage-api";
import { AppShell } from "../app-shell";
import { btn, Connecting, EmptyPanel, ErrorNote, input, Offline, panel, primary, Stats } from "../ui";
import { KeysTab } from "./keys";
import { RolesTab } from "./roles";
import { Field, PublicBadge, Segmented, TabNav, useAction, useHashTab, useStorage } from "./shared";
import { UsageTab } from "./usage";

const TABS = [
  { value: "buckets", label: "Buckets" },
  { value: "keys", label: "Access keys" },
  { value: "roles", label: "Roles" },
  { value: "usage", label: "Usage" },
] as const;
type Tab = (typeof TABS)[number]["value"];
const TAB_VALUES = TABS.map((t) => t.value);

/** Days of the current UTC month so far, for month-to-date usage. */
function monthDays() {
  return Math.max(1, new Date().getUTCDate());
}

function useMonthToDate() {
  const [usage, setUsage] = useState<StorageUsageAll | null>(null);
  useEffect(() => {
    let alive = true;
    const load = () => storageApi.usageAll(monthDays()).then((u) => alive && setUsage(u), () => {});
    load();
    const t = setInterval(load, 60_000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);
  return usage;
}

export function StorageHome() {
  const router = useRouter();
  const { info, buckets, offline, refresh } = useStorage();
  const { busy, error, setError, run } = useAction(refresh);
  const [tab, setTabRaw] = useHashTab<Tab>(TAB_VALUES, "buckets");
  const [creating, setCreating] = useState(false);
  const mtd = useMonthToDate();
  const setTab = (t: Tab) => {
    setTabRaw(t);
    setError(null);
  };

  return (
    <AppShell
      crumb="storage"
      actions={
        <button
          onClick={() => {
            setTab("buckets");
            setCreating((c) => !c);
          }}
          disabled={offline || !info}
          className={primary}
        >
          <Plus className="size-3.5" /> New bucket
        </button>
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-[-0.04em]">Storage</h1>
          <p className="mt-1 text-[13px] text-muted">
            S3-compatible object storage — any S3 SDK or tool, instant branches, deletes recoverable for days.
          </p>
        </div>
        {info && (
          <p className="font-mono text-[11px] text-subtle">
            {info.region} · {info.storage_class.toLowerCase()} · {info.storage}
          </p>
        )}
      </div>

      {offline ? (
        <Offline />
      ) : !info || !buckets ? (
        <Connecting />
      ) : (
        <>
          <TabNav tabs={TABS} tab={tab} onChange={setTab} label="Storage sections" counts={{ buckets: buckets.length }} />
          <ErrorNote error={error} />
          <div className="mt-6">
            {tab === "buckets" && (
              <>
                {creating && (
                  <CreateBucket
                    info={info}
                    busy={busy === "create"}
                    onCancel={() => setCreating(false)}
                    onCreate={async (name, isPublic, recover) => {
                      const b = await run("create", () =>
                        storageApi.createBucket(name, { public: isPublic, recover_days: recover }),
                      );
                      if (b) {
                        setCreating(false);
                        router.push(`/dashboard/storage/${encodeURIComponent(b.name)}`);
                      }
                    }}
                  />
                )}
                <Overview info={info} buckets={buckets} mtd={mtd} />
                {buckets.length === 0 ? (
                  !creating && (
                    <EmptyPanel>
                      <h2 className="text-[15px] font-medium">No buckets yet</h2>
                      <p className="max-w-md text-[13px] text-muted">
                        A bucket speaks the S3 API at <span className="font-mono text-fg">{info.endpoint}</span>. Each
                        object is stored as its own S3 object, so it gets S3&apos;s durability; deleted and overwritten
                        objects stay recoverable for {info.recover_days_default} days.
                      </p>
                      <button onClick={() => setCreating(true)} className={primary}>
                        <Plus className="size-3.5" /> New bucket
                      </button>
                    </EmptyPanel>
                  )
                ) : (
                  <BucketList buckets={buckets} info={info} mtd={mtd} />
                )}
              </>
            )}
            {tab === "keys" && <KeysTab info={info} buckets={buckets} />}
            {tab === "roles" && <RolesTab buckets={buckets} />}
            {tab === "usage" && <UsageTab info={info} />}
          </div>
        </>
      )}
    </AppShell>
  );
}

function Overview({ info, buckets, mtd }: { info: StorageInfo; buckets: Bucket[]; mtd: StorageUsageAll | null }) {
  const trash = buckets.reduce((s, b) => s + b.trash_bytes, 0);
  const versionBytes = buckets.reduce((s, b) => s + b.version_bytes, 0);
  const branches = buckets.filter((b) => b.parent).length;
  const scrubBad = info.scrub.missing + info.scrub.mismatched;
  return (
    <div className="mb-4">
      <Stats
        items={[
          {
            label: "Buckets",
            value: buckets.length,
            hint: branches ? `${branches} branch${branches === 1 ? "" : "es"}` : `${buckets.filter((b) => b.public).length} public`,
          },
          {
            label: "Objects",
            value: formatCount(info.objects),
            hint: versionBytes ? `${formatBytes(versionBytes)} older versions · ${formatBytes(trash)} recoverable` : `${formatBytes(trash)} recoverable`,
          },
          {
            label: "Stored",
            value: formatBytes(info.bytes),
            hint: scrubBad
              ? `${scrubBad} integrity issue${scrubBad === 1 ? "" : "s"}`
              : info.scrub.last_at
                ? `verified ${agoMs(info.scrub.last_at)}`
                : "S3 durability per object",
            tone: scrubBad ? undefined : info.scrub.last_at ? "ok" : "muted",
          },
          {
            label: "Month to date",
            value: mtd ? usd(mtd.cost.total_usd) : "—",
            hint: mtd ? `${usd(mtd.cost.projected_month_usd)} projected` : "S3 list prices",
          },
        ]}
      />
    </div>
  );
}

function BucketList({ buckets, info, mtd }: { buckets: Bucket[]; info: StorageInfo; mtd: StorageUsageAll | null }) {
  const cost = new Map(mtd?.buckets.map((u) => [u.bucket, u.cost.total_usd]) ?? []);
  return (
    <section className={panel}>
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h2 className="text-[15px] font-medium">Buckets</h2>
        <span className="font-mono text-[12px] text-subtle">{buckets.length}</span>
      </div>
      <ul>
        {buckets.map((b) => (
          <li key={b.id} className="rise border-b border-line last:border-0">
            <Link
              href={`/dashboard/storage/${encodeURIComponent(b.name)}`}
              className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-4 gap-y-1 px-5 py-3.5 transition hover:bg-panel-2/60 md:grid-cols-[minmax(0,1.6fr)_minmax(0,0.7fr)_minmax(0,0.7fr)_minmax(0,0.6fr)_auto]"
            >
              <div className="min-w-0">
                <p className="flex items-center gap-2 truncate text-[13px]">
                  <span className="truncate font-mono">{b.name}</span>
                  {b.public && <PublicBadge />}
                  {b.parent && (
                    <span className="flex items-center gap-1 truncate text-[11px] text-subtle">
                      <GitBranch className="size-3" /> {b.parent.name}
                    </span>
                  )}
                </p>
                <p className="truncate font-mono text-[11px] text-subtle">
                  {b.id} · {b.region} · created {agoMs(b.created_at)}
                </p>
              </div>
              <p className="hidden font-mono text-[12px] tabular-nums md:block">
                {formatCount(b.objects)} <span className="text-subtle">objects</span>
              </p>
              <p className="hidden font-mono text-[12px] tabular-nums md:block">{formatBytes(b.bytes)}</p>
              <p className="hidden font-mono text-[12px] text-muted tabular-nums md:block" title="Month to date">
                {cost.has(b.name) ? usd(cost.get(b.name) ?? 0) : "—"}
              </p>
              <ChevronRight className="size-3.5 text-subtle" />
            </Link>
          </li>
        ))}
      </ul>
      <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
        Point any S3 client at <span className="font-mono text-muted">{info.endpoint}</span> with path-style addressing
        {info.virtual_host_domain && (
          <>
            , or use <span className="font-mono text-muted">&lt;bucket&gt;.{info.virtual_host_domain}</span>
          </>
        )}
        .
      </p>
    </section>
  );
}

function CreateBucket({
  info,
  busy,
  onCreate,
  onCancel,
}: {
  info: StorageInfo;
  busy: boolean;
  onCreate: (name: string, isPublic: boolean, recoverDays: number) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [isPublic, setPublic] = useState(false);
  const [recover, setRecover] = useState(info.recover_days_default);
  const err = bucketNameError(name);
  const recoverOptions = RECOVER_OPTIONS.includes(recover) ? RECOVER_OPTIONS : [...RECOVER_OPTIONS, recover].sort((a, b) => a - b);
  return (
    <form
      className={`${panel} rise mb-4 space-y-5 p-5`}
      onSubmit={(e) => {
        e.preventDefault();
        if (!err) onCreate(name, isPublic, recover);
      }}
    >
      <div className="flex flex-wrap items-center gap-2">
        <input
          autoFocus
          value={name}
          onChange={(e) => setName(e.target.value.toLowerCase())}
          placeholder="bucket-name"
          spellCheck={false}
          className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
        />
        <button type="submit" disabled={busy || !!err} className={primary}>
          {busy ? <span className="shimmer">Creating…</span> : "Create"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
      {name && err && <p className="text-[12px] text-danger">{err}</p>}
      <div className="grid gap-x-3 gap-y-4 sm:grid-cols-2">
        <Field label="Access" hint={isPublic ? "anyone can GET objects by URL; listing still needs a key" : "every request needs an access key"}>
          <Segmented
            value={isPublic ? "public" : "private"}
            options={[
              { label: "private", value: "private" },
              { label: "public read", value: "public" },
            ]}
            onChange={(v) => setPublic(v === "public")}
          />
        </Field>
        <Field label="Recover deleted objects for" hint={recover === 0 ? "deletes are final for you" : "deleted and overwritten objects, billed as storage"}>
          <select value={recover} onChange={(e) => setRecover(Number(e.target.value))} className={`${input} w-full`}>
            {recoverOptions.map((d) => (
              <option key={d} value={d}>
                {d === 0 ? "off" : `${d} day${d === 1 ? "" : "s"}`}
              </option>
            ))}
          </select>
        </Field>
      </div>
      <p className="text-[12px] text-subtle">
        S3 list prices: {usd(info.pricing.usd_per_gb_month)} per GB-month, {usd(info.pricing.usd_per_million_class_a)} per
        million writes, {usd(info.pricing.usd_per_million_class_b)} per million reads. Traffic inside roll is free.
      </p>
    </form>
  );
}
