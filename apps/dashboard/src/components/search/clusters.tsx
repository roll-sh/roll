"use client";

import { HorizonMark } from "@roll/brand";
import { ChevronRight, GitBranch, Moon, Plus } from "lucide-react";
import Link from "next/link";
import { useState } from "react";
import { formatBytes } from "@/lib/roll-api";
import { formatCount, formatDuration, NAME_HINT, NAME_RE, searchApi, type SearchCluster, type SearchInfo, type SearchSettings } from "@/lib/search-api";
import { AppShell } from "../app-shell";
import { btn, Connecting, EmptyPanel, ErrorNote, input, Offline, panel, primary, Stats } from "../ui";
import { CredentialsOnce, SearchStateDot, SettingsFields, stateLabel, useAction, useSearchClusters } from "./shared";

export function SearchClusters() {
  const { info, clusters, offline, refresh } = useSearchClusters();
  const { busy, error, run } = useAction(refresh);
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<SearchCluster | null>(null);

  return (
    <AppShell
      crumb="search"
      actions={
        <button onClick={() => setCreating((c) => !c)} disabled={offline || !info} className={primary}>
          <Plus className="size-3.5" /> New cluster
        </button>
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-[-0.04em]">Search</h1>
          <p className="mt-1 text-[13px] text-muted">
            Elasticsearch-compatible clusters that sleep when idle, wake in milliseconds and branch instantly.
          </p>
        </div>
        {info && (
          <p className="font-mono text-[11px] text-subtle">
            elasticsearch {info.version} · opensearch {info.opensearch_version} · {info.region} · {info.storage}
          </p>
        )}
      </div>

      <ErrorNote error={error} />

      {created && (
        <CredentialsOnce
          cluster={created}
          title={`${created.name} is ready — save its password`}
          onDone={() => setCreated(null)}
        />
      )}

      {creating && info && (
        <CreateCluster
          info={info}
          busy={busy === "create"}
          onCancel={() => setCreating(false)}
          onCreate={async (name, settings) => {
            const c = await run("create", () => searchApi.create(name, settings));
            if (c) {
              setCreating(false);
              setCreated(c);
            }
          }}
        />
      )}

      {offline ? (
        <Offline />
      ) : clusters === null || info === null ? (
        <Connecting />
      ) : clusters.length === 0 ? (
        !creating && (
          <EmptyPanel>
            <h2 className="text-[15px] font-medium">No search clusters yet</h2>
            <p className="max-w-md text-[13px] text-muted">
              Each cluster speaks the Elasticsearch REST API, so the official clients, searchkick, Laravel Scout and
              Spring Data work unchanged. It loads on the first request and sleeps when idle; its indices stay in object
              storage, so an unused cluster costs only storage.
            </p>
            <button onClick={() => setCreating(true)} className={primary}>
              <Plus className="size-3.5" /> New cluster
            </button>
          </EmptyPanel>
        )
      ) : (
        <>
          <Overview info={info} clusters={clusters} />
          <ClusterList clusters={clusters} info={info} busy={busy} onUnload={(c) => run(`unload-${c.id}`, () => searchApi.unload(c.id))} />
        </>
      )}
    </AppShell>
  );
}

function Overview({ info, clusters }: { info: SearchInfo; clusters: SearchCluster[] }) {
  const loaded = clusters.filter((c) => c.status.state === "loaded");
  const stored = clusters.reduce((s, c) => s + c.stored_bytes, 0);
  const docs = clusters.reduce((s, c) => s + c.docs, 0);
  const indices = clusters.reduce((s, c) => s + (c.status.indices ?? c.indices), 0);
  return (
    <div className="mt-6">
      <Stats
        items={[
          { label: "Clusters", value: clusters.length, hint: `${loaded.length} loaded · ${clusters.length - loaded.length} asleep` },
          { label: "Indices", value: formatCount(indices), hint: `${formatCount(docs)} documents` },
          { label: "Stored", value: formatBytes(stored), hint: `cache ${formatBytes(info.cache.used_bytes)} of ${formatBytes(info.cache.budget_bytes)}` },
          { label: "Endpoint", value: info.domain ? `*.${info.domain}` : info.http, hint: info.tls ? "HTTPS" : "HTTP" },
        ]}
      />
    </div>
  );
}

function ClusterList({
  clusters,
  info,
  busy,
  onUnload,
}: {
  clusters: SearchCluster[];
  info: SearchInfo;
  busy: string | null;
  onUnload: (c: SearchCluster) => void;
}) {
  return (
    <section className={`${panel} mt-4`}>
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h2 className="text-[15px] font-medium">Clusters</h2>
        <span className="font-mono text-[12px] text-subtle">{clusters.length}</span>
      </div>
      <ul>
        {clusters.map((c) => {
          const on = c.status.state === "loaded";
          const parent = c.parent_id ? (c.parent_name ?? "deleted cluster") : null;
          return (
            <li key={c.id} className="rise group relative border-b border-line last:border-0">
              <Link
                href={`/dashboard/search/${c.id}`}
                className="grid grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-x-4 gap-y-2 px-5 py-3.5 transition hover:bg-panel-2/60 md:grid-cols-[auto_minmax(0,1.4fr)_minmax(0,1fr)_minmax(0,0.8fr)_auto]"
              >
                <SearchStateDot state={c.status.state} />
                <div className="min-w-0">
                  <p className="flex items-center gap-1.5 truncate text-[13px]">
                    {c.name}
                    {parent && (
                      <span className="flex items-center gap-1 truncate text-[11px] text-subtle">
                        <GitBranch className="size-3" /> {parent}
                      </span>
                    )}
                  </p>
                  <p className="truncate font-mono text-[11px] text-subtle">
                    {c.id} · {c.flavor} · {c.url}
                  </p>
                </div>
                <div className="hidden min-w-0 font-mono text-[11px] text-subtle md:block">
                  <p className="truncate">
                    <span className={on ? "text-fg" : ""}>{formatCount(c.status.indices ?? c.indices)}</span>{" "}
                    {(c.status.indices ?? c.indices) === 1 ? "index" : "indices"} · {formatCount(c.docs)} {c.docs === 1 ? "doc" : "docs"}
                  </p>
                  <p className="truncate">{formatBytes(c.stored_bytes)} stored</p>
                </div>
                <div className="hidden min-w-0 font-mono text-[11px] text-subtle md:block">
                  <p className="truncate">
                    {on ? `${c.status.in_flight ?? 0} in flight` : c.idle_timeout_s === 0 ? "always loaded" : `sleeps after ${formatDuration(c.idle_timeout_s)}`}
                  </p>
                  <p className="truncate">{c.branches.length} branch{c.branches.length === 1 ? "" : "es"}</p>
                </div>
                <span className="flex items-center gap-2 text-[12px] text-subtle">
                  <span className="hidden w-16 text-right sm:inline">{stateLabel(c.status.state)}</span>
                  <ChevronRight className="size-3.5" />
                </span>
              </Link>
              {on && (
                <button
                  title="Put to sleep now"
                  aria-label={`Put ${c.name} to sleep now`}
                  disabled={busy !== null}
                  onClick={() => onUnload(c)}
                  className="absolute top-1/2 right-12 hidden -translate-y-1/2 rounded-md p-1.5 text-subtle transition group-hover:block hover:bg-panel hover:text-fg focus-visible:block disabled:opacity-40"
                >
                  {busy === `unload-${c.id}` ? <HorizonMark state="thinking" size={14} interactive={false} /> : <Moon className="size-3.5" />}
                </button>
              )}
            </li>
          );
        })}
      </ul>
      <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
        Point any Elasticsearch client at{" "}
        <span className="font-mono text-muted">{info.domain ? `https://<cluster>.${info.domain}` : `${info.tls ? "https" : "http"}://${info.http}`}</span>{" "}
        with the cluster name as the user.
      </p>
    </section>
  );
}

function CreateCluster({
  info,
  busy,
  onCreate,
  onCancel,
}: {
  info: SearchInfo;
  busy: boolean;
  onCreate: (name: string, settings: SearchSettings) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [settings, setSettings] = useState<SearchSettings>({ ...info.defaults });
  const valid = NAME_RE.test(name);
  return (
    <form
      className={`${panel} rise mt-6 space-y-5 p-5`}
      onSubmit={(e) => {
        e.preventDefault();
        if (valid) onCreate(name, settings);
      }}
    >
      <div className="flex flex-wrap items-center gap-2">
        <input
          autoFocus
          value={name}
          onChange={(e) => setName(e.target.value.toLowerCase())}
          placeholder="cluster-name"
          spellCheck={false}
          className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
        />
        <button type="submit" disabled={busy || !valid} className={primary}>
          {busy ? <span className="shimmer">Creating…</span> : "Create"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
      {name && !valid && <p className="text-[12px] text-danger">{NAME_HINT}</p>}
      <SettingsFields value={settings} onChange={setSettings} />
      <p className="text-[12px] text-subtle">
        The name is the cluster&apos;s user name and host label, so it can&apos;t change later. Billed only while loaded:
        memory at {info.pricing.usd_per_gib_hour} USD per GiB-hour and {info.pricing.usd_per_million_requests} USD per
        million requests, plus S3 storage at {info.pricing.usd_per_gb_month_s3} USD per GB-month.
      </p>
    </form>
  );
}
