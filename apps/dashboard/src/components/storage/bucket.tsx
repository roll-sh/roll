"use client";

import { HorizonMark } from "@roll/brand";
import { ArrowLeft, GitBranch, RotateCcw } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useState } from "react";
import { formatCount } from "@/lib/redis-api";
import { formatBytes } from "@/lib/roll-api";
import {
  agoMs,
  bucketNameError,
  formatDate,
  inMs,
  storageApi,
  type Bucket,
  type StorageInfo,
  type TrashEntry,
} from "@/lib/storage-api";
import { AppShell } from "../app-shell";
import { iconBtn, LoadMore, Spinner } from "../tasks/ui";
import { btn, Connecting, CopyButton, EmptyPanel, ErrorNote, input, Offline, Panel, panel, primary, Stats, useCopy } from "../ui";
import { ObjectBrowser } from "./browser";
import { StorageIdentity } from "./console";
import { SdkSnippet } from "./keys";
import { SettingsTab } from "./settings";
import { PublicBadge, TabNav, useAction, useHashTab, useStorage, type Run } from "./shared";
import { BucketUsage } from "./usage";

const TABS = [
  { value: "objects", label: "Objects" },
  { value: "deleted", label: "Recently deleted" },
  { value: "branches", label: "Branches" },
  { value: "connect", label: "Connect" },
  { value: "usage", label: "Usage" },
  { value: "settings", label: "Settings" },
] as const;
type Tab = (typeof TABS)[number]["value"];
const TAB_VALUES = TABS.map((t) => t.value);

const href = (name: string) => `/dashboard/storage/${encodeURIComponent(name)}`;

/** Placeholder while the route's params resolve. */
export function StorageBucketLoading() {
  return (
    <AppShell crumb="storage">
      <Connecting />
    </AppShell>
  );
}

export function StorageBucket({ name: rawName }: { name: string }) {
  const name = decodeURIComponent(rawName);
  const router = useRouter();
  const { info, buckets, offline, denied, refresh } = useStorage();
  const { busy, error, setError, run } = useAction(refresh);
  const [tab, setTabRaw] = useHashTab<Tab>(TAB_VALUES, "objects");
  const setTab = (t: Tab) => {
    setTabRaw(t);
    setError(null);
  };

  const crumb = (
    <>
      <Link href="/dashboard/storage" className="text-muted hover:text-fg">
        storage
      </Link>{" "}
      <span className="text-subtle">/</span> <span className="font-mono">{name}</span>
    </>
  );

  if (offline) {
    return (
      <AppShell crumb={crumb}>
        <Offline />
      </AppShell>
    );
  }
  if (!info || !buckets) {
    return (
      <AppShell crumb={crumb}>
        {denied ? <ErrorNote error={denied} /> : <Connecting />}
      </AppShell>
    );
  }
  const bucket = buckets.find((b) => b.name === name || b.id === name);
  if (!bucket) {
    return (
      <AppShell crumb={crumb}>
        <StorageIdentity switcher={false} />
        <EmptyPanel>
          <HorizonMark state="sleep" size={40} />
          <h2 className="text-[15px] font-medium">No bucket {name}</h2>
          <p className="max-w-md text-[13px] text-muted">
            {denied ?? "It may have been deleted, or the link is wrong — or it belongs to another account."}
          </p>
          <Link href="/dashboard/storage" className={btn}>
            <ArrowLeft className="size-3.5" /> All buckets
          </Link>
        </EmptyPanel>
      </AppShell>
    );
  }

  return (
    <AppShell crumb={crumb}>
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="min-w-0">
          <h1 className="flex items-center gap-3 text-2xl font-semibold tracking-[-0.04em]">
            <span className="truncate font-mono">{bucket.name}</span>
            {bucket.public && <PublicBadge />}
          </h1>
          <p className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-[13px] text-muted">
            <span className="font-mono text-[12px] tabular-nums">
              {formatCount(bucket.objects)} objects · {formatBytes(bucket.bytes)}
            </span>
            {(bucket.versions > 0 || bucket.version_bytes > 0) && (
              <span
                className="font-mono text-[12px] text-subtle tabular-nums"
                title="Noncurrent versions and delete markers, billed as storage"
              >
                + {formatBytes(bucket.version_bytes)} in {formatCount(bucket.versions)} older version{bucket.versions === 1 ? "" : "s"}
              </span>
            )}
            {bucket.trash_bytes > 0 && (
              <span className="font-mono text-[12px] text-subtle tabular-nums" title="Recently deleted, billed as storage">
                + {formatBytes(bucket.trash_bytes)} recoverable
              </span>
            )}
            <span className="text-subtle">·</span>
            <span className="font-mono text-[12px] text-subtle">{bucket.id}</span>
            {bucket.parent && (
              <>
                <span className="text-subtle">·</span>
                <span className="flex items-center gap-1 text-[12px] text-subtle">
                  <GitBranch className="size-3" /> branch of{" "}
                  <Link href={href(bucket.parent.name)} className="text-accent hover:underline">
                    {bucket.parent.name}
                  </Link>{" "}
                  at {formatDate(bucket.parent.at)}
                </span>
              </>
            )}
          </p>
        </div>
        <div className="flex flex-col items-start gap-1 sm:items-end">
          <StorageIdentity switcher={false} />
          <p className="font-mono text-[11px] text-subtle">
            {bucket.region} · created {agoMs(bucket.created_at)}
          </p>
        </div>
      </div>

      <TabNav
        tabs={TABS}
        tab={tab}
        onChange={setTab}
        label="Bucket sections"
        counts={{ branches: bucket.branches.length }}
      />
      <ErrorNote error={error} />

      <div className="mt-6">
        {tab === "objects" && <ObjectBrowser key={bucket.id} bucket={bucket} onChanged={refresh} />}
        {tab === "deleted" && <TrashTab key={bucket.id} bucket={bucket} onRestored={refresh} />}
        {tab === "branches" && <BranchesTab bucket={bucket} all={buckets} busy={busy} run={run} />}
        {tab === "connect" && <ConnectTab bucket={bucket} info={info} />}
        {tab === "usage" && <BucketUsage info={info} bucket={bucket.name} />}
        {tab === "settings" && (
          <SettingsTab
            key={bucket.id}
            bucket={bucket}
            info={info}
            buckets={buckets}
            busy={busy}
            run={run}
            onChanged={refresh}
            onDeleted={() => router.push("/dashboard/storage")}
          />
        )}
      </div>
    </AppShell>
  );
}

// --- Recently deleted --------------------------------------------------------------------------

function TrashTab({ bucket, onRestored }: { bucket: Bucket; onRestored: () => void }) {
  const [prefix, setPrefix] = useState("");
  const [entries, setEntries] = useState<TrashEntry[] | null>(null);
  const [cursor, setCursor] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [restoring, setRestoring] = useState<string | null>(null);
  const [restored, setRestored] = useState<string | null>(null);

  const load = useCallback(
    async (more: string | null) => {
      setLoading(true);
      try {
        const r = await storageApi.trash(bucket.name, { prefix, cursor: more, limit: 200 });
        setEntries((cur) => (more && cur ? [...cur, ...r.objects] : r.objects));
        setCursor(r.next_cursor);
        setError(null);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setLoading(false);
      }
    },
    [bucket.name, prefix],
  );

  useEffect(() => {
    const t = setTimeout(() => load(null), prefix ? 200 : 0);
    return () => clearTimeout(t);
  }, [load, prefix]);

  const restore = async (e: TrashEntry) => {
    const id = `${e.key}@${e.deleted_at}`;
    setRestoring(id);
    try {
      await storageApi.restore(bucket.name, e.key, e.deleted_at);
      setRestored(e.key);
      onRestored();
      await load(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setRestoring(null);
    }
  };

  return (
    <div className="space-y-4">
      <ErrorNote error={error} />
      {restored && (
        <p className="rise text-[13px] text-ok">
          Restored <span className="font-mono">{restored}</span>.
        </p>
      )}
      <section className={panel}>
        <div className="flex flex-wrap items-center gap-3 border-b border-line px-5 py-3">
          <div className="min-w-0 flex-1">
            <h2 className="text-[15px] font-medium">Recently deleted</h2>
            <p className="text-[12px] text-subtle">
              {bucket.recover_days > 0
                ? `${bucket.versioning ? "Versions deleted for good" : "Deleted and overwritten objects"} are kept for ${bucket.recover_days} day${bucket.recover_days === 1 ? "" : "s"} · ${formatBytes(bucket.trash_bytes)}`
                : "Recovery is off for this bucket (Settings)."}
            </p>
          </div>
          <input
            value={prefix}
            onChange={(e) => setPrefix(e.target.value)}
            placeholder="Filter by prefix…"
            spellCheck={false}
            className={`${input} w-48 py-1 font-mono text-[12px]`}
          />
          <button onClick={() => load(null)} className={iconBtn} aria-label="Refresh">
            {loading ? <Spinner /> : <RotateCcw className="size-3.5" />}
          </button>
        </div>
        {entries === null ? (
          <p className="px-5 py-6 text-[13px] text-subtle">
            <span className="shimmer">Loading…</span>
          </p>
        ) : entries.length === 0 ? (
          <p className="px-5 py-10 text-center text-[13px] text-muted">
            {prefix ? "Nothing deleted under that prefix." : "Nothing deleted recently."}
          </p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr className="border-b border-line text-left text-[11px] text-subtle">
                  <th className="px-5 py-2 font-normal">Key</th>
                  <th className="hidden px-3 py-2 text-right font-normal sm:table-cell">Size</th>
                  <th className="px-3 py-2 font-normal">Deleted</th>
                  <th className="hidden px-3 py-2 font-normal md:table-cell">Gone in</th>
                  <th className="px-5 py-2" />
                </tr>
              </thead>
              <tbody>
                {entries.map((e) => {
                  const id = `${e.key}@${e.deleted_at}`;
                  return (
                    <tr key={id} className="border-b border-line last:border-0 hover:bg-panel-2/60">
                      <td className="max-w-0 truncate px-5 py-2 font-mono text-[12px]" title={e.key}>
                        {e.key}
                      </td>
                      <td className="hidden px-3 py-2 text-right font-mono text-[12px] tabular-nums sm:table-cell">{formatBytes(e.size)}</td>
                      <td className="px-3 py-2 text-[12px] whitespace-nowrap text-subtle" title={formatDate(e.deleted_at)}>
                        {agoMs(e.deleted_at)}
                      </td>
                      <td className="hidden px-3 py-2 font-mono text-[12px] text-subtle md:table-cell" title={formatDate(e.expires_at)}>
                        {inMs(e.expires_at)}
                      </td>
                      <td className="px-5 py-2 text-right">
                        <button onClick={() => restore(e)} disabled={restoring !== null} className={btn}>
                          {restoring === id ? <span className="shimmer">Restoring…</span> : "Restore"}
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        <LoadMore cursor={cursor} onMore={() => load(cursor)} loading={loading} />
      </section>
      <p className="text-[12px] text-subtle">
        {bucket.versioning
          ? "Restoring puts the version back in its key's history: current again unless a newer version or delete marker is there."
          : "Restoring puts the object back under its key; whatever is there now moves to Recently deleted."}
      </p>
    </div>
  );
}

// --- Branches ----------------------------------------------------------------------------------

function BranchesTab({ bucket, all, busy, run }: { bucket: Bucket; all: Bucket[]; busy: string | null; run: Run }) {
  const router = useRouter();
  const children = all.filter((b) => b.parent?.id === bucket.id);
  const [name, setName] = useState(`${bucket.name}-preview`.slice(0, 63));
  const err = bucketNameError(name);
  return (
    <div className="grid gap-4 lg:grid-cols-5">
      <Panel title="Branches" aside={<span className="font-mono text-[12px] text-subtle">{children.length}</span>} className="lg:col-span-3">
        {children.length === 0 ? (
          <p className="px-5 py-6 text-[13px] text-muted">
            No branches yet. A branch is a new bucket that starts with this bucket&apos;s objects as of now — instant,
            nothing is copied. Writes on either side don&apos;t affect the other, so previews can upload and delete
            freely.
          </p>
        ) : (
          <ul>
            {children.map((b) => (
              <li key={b.id} className="rise border-b border-line last:border-0">
                <Link href={href(b.name)} className="flex items-center gap-3 px-5 py-3 transition hover:bg-panel-2/60">
                  <GitBranch className="size-4 shrink-0 text-subtle" />
                  <div className="min-w-0 flex-1">
                    <p className="truncate font-mono text-[13px]">{b.name}</p>
                    <p className="font-mono text-[11px] text-subtle">
                      objects as of {b.parent ? formatDate(b.parent.at) : "—"} · {formatBytes(b.bytes)} own
                    </p>
                  </div>
                  <span className="text-[12px] text-subtle">{agoMs(b.created_at)}</span>
                </Link>
              </li>
            ))}
          </ul>
        )}
        {bucket.parent && (
          <p className="mt-auto border-t border-line px-5 py-3 text-[12px] text-subtle">
            This bucket is itself a branch of{" "}
            <Link href={href(bucket.parent.name)} className="text-accent hover:underline">
              {bucket.parent.name}
            </Link>
            .
          </p>
        )}
      </Panel>
      <Panel title="New branch" className="lg:col-span-2">
        <form
          className="space-y-4 px-5 py-4"
          onSubmit={async (e) => {
            e.preventDefault();
            if (err) return;
            const b = await run("branch", () => storageApi.branch(bucket.name, name));
            if (b) router.push(href(b.name));
          }}
        >
          <label className="block space-y-1">
            <span className="block text-[11px] text-subtle">Bucket name</span>
            <input
              value={name}
              onChange={(e) => setName(e.target.value.toLowerCase())}
              spellCheck={false}
              className={`${input} w-full font-mono text-[12px]`}
            />
          </label>
          {name && err && <p className="text-[12px] text-danger">{err}</p>}
          <p className="text-[12px] text-subtle">
            Billed for what it adds or changes; objects it shares with {bucket.name} are billed once.
          </p>
          <button type="submit" disabled={busy !== null || !!err} className={primary}>
            {busy === "branch" ? (
              <span className="shimmer">Branching…</span>
            ) : (
              <>
                <GitBranch className="size-3.5" /> Create branch
              </>
            )}
          </button>
        </form>
      </Panel>
    </div>
  );
}

// --- Connect -----------------------------------------------------------------------------------

function ConnectTab({ bucket, info }: { bucket: Bucket; info: StorageInfo }) {
  const { copied, copy } = useCopy();
  const rows: { label: string; value: string; hint?: string }[] = [
    { label: "Endpoint", value: info.endpoint, hint: "AWS_ENDPOINT_URL; use path-style addressing" },
    { label: "Bucket URL (path style)", value: bucket.url },
    ...(bucket.virtual_host_url ? [{ label: "Bucket URL (virtual host)", value: bucket.virtual_host_url }] : []),
    { label: "Region", value: info.region, hint: "any region name is accepted when signing" },
  ];
  return (
    <div className="space-y-4">
      <Stats
        items={[
          {
            label: "Objects",
            value: formatCount(bucket.objects),
            hint: bucket.version_bytes ? `${formatBytes(bucket.bytes)} + ${formatBytes(bucket.version_bytes)} older versions` : formatBytes(bucket.bytes),
          },
          { label: "Access", value: bucket.public ? "public read" : "private", hint: bucket.public ? "GET without credentials" : "access key required" },
          { label: "Recovery", value: bucket.recover_days ? `${bucket.recover_days}d` : "off", hint: `${formatBytes(bucket.trash_bytes)} recoverable` },
          { label: "Storage class", value: info.storage_class.toLowerCase().replace("_", "-"), hint: "one S3 object per object" },
        ]}
      />
      <div className="grid gap-4 lg:grid-cols-5">
        <Panel title="Connection" className="lg:col-span-2">
          <div className="space-y-4 px-5 py-4">
            {rows.map((r) => (
              <div key={r.label} className="space-y-1">
                <p className="text-[12px] text-subtle">{r.label}</p>
                <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
                  <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{r.value}</code>
                  <CopyButton id={r.label} text={r.value} copied={copied} onCopy={copy} />
                </div>
                {r.hint && <p className="text-[11px] text-subtle">{r.hint}</p>}
              </div>
            ))}
            <p className="text-[12px] text-muted">
              Credentials are access keys —{" "}
              <Link href="/dashboard/storage#keys" className="text-accent hover:underline">
                create one
              </Link>{" "}
              (optionally scoped to this bucket).
            </p>
          </div>
        </Panel>
        <Panel title="Quickstart" className="lg:col-span-3">
          <div className="p-4">
            <SdkSnippet info={info} id="<access-key-id>" secret="<secret-access-key>" bucket={bucket.name} />
          </div>
          {bucket.public && (
            <p className="mt-auto border-t border-line px-5 py-3 text-[12px] text-subtle">
              Public objects load straight from{" "}
              <span className="font-mono text-muted">
                {bucket.url.replace(/\/$/, "")}/&lt;key&gt;
              </span>
              .
            </p>
          )}
        </Panel>
      </div>
    </div>
  );
}
