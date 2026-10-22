"use client";

import { HorizonMark } from "@roll/brand";
import { CloudDownload } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { formatCount } from "@/lib/redis-api";
import { EVERY_OPTIONS, formatSpan, importSourceKind, nosqlApi, type NosqlDb, type NosqlImport } from "@/lib/nosql-api";
import { ago, ErrorNote, input, Panel, primary } from "../ui";
import { Field, formatTime, POLL_MS, useAction } from "./shared";

function useImports(id: string) {
  const [list, setList] = useState<NosqlImport[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const refresh = useCallback(async () => {
    try {
      setList(await nosqlApi.imports(id));
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [id]);
  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      await refresh();
      if (alive) timer = setTimeout(loop, POLL_MS);
    };
    loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [refresh]);
  return { list, error, refresh };
}

export function ImportsTab({ db }: { db: NosqlDb }) {
  const { list, error, refresh } = useImports(db.id);
  const { busy, error: actionError, setError, run } = useAction(refresh);
  const [from, setFrom] = useState("cql://");
  const [keyspaces, setKeyspaces] = useState("");
  const [every, setEvery] = useState(0);
  const kind = importSourceKind(from);
  const valid = kind !== null;

  return (
    <div className="space-y-4">
      <div className="grid gap-4 lg:grid-cols-5">
        <Panel title="Import" className="lg:col-span-3">
          <form
            className="space-y-4 px-5 py-4"
            onSubmit={async (e) => {
              e.preventDefault();
              if (!valid) return;
              const ks = keyspaces
                .split(/[\s,]+/)
                .map((k) => k.trim())
                .filter(Boolean);
              const req = { from: from.trim(), ...(ks.length ? { keyspaces: ks } : {}), ...(every ? { every } : {}) };
              await run("import", () => nosqlApi.startImport(db.id, req));
            }}
          >
            <p className="text-[13px] text-muted">
              {kind === "backup"
                ? "Restore SSTable backups — nodetool snapshot, ScyllaDB Manager or Medusa, Cassandra nb/oa/da and ScyllaDB me/ms/mt — with no source cluster. Write times, TTLs and tombstones are kept; every node's SSTables are merged. Schema comes along."
                : "Mirror a live ScyllaDB or Cassandra cluster: a parallel token-range scan over CQL, loaded as SSTables with writetimes and TTLs kept. It needs only a read-only role on the source. Schema comes along."}
            </p>
            <Field
              label="Source"
              hint={
                kind === "backup"
                  ? "s3://bucket/path — ?tag= picks a snapshot (default: the newest), ?region= / ?endpoint= for the bucket"
                  : "cql://user:password@host:9042 — add ?ssl=true for TLS; s3://bucket/path for backups"
              }
            >
              <input
                value={from}
                onChange={(e) => setFrom(e.target.value)}
                placeholder="cql://reader:secret@prod-host:9042"
                spellCheck={false}
                autoComplete="off"
                className={`${input} w-full font-mono text-[12px]`}
              />
            </Field>
            <div className="grid gap-3 sm:grid-cols-2">
              <Field label="Keyspaces" hint="comma-separated; empty imports every non-system keyspace">
                <input
                  value={keyspaces}
                  onChange={(e) => setKeyspaces(e.target.value)}
                  placeholder="app, events"
                  spellCheck={false}
                  className={`${input} w-full font-mono text-[12px]`}
                />
              </Field>
              <Field label="Refresh" hint={every ? "each refresh is a new version to branch from" : "a one-off copy"}>
                <select value={every} onChange={(e) => setEvery(Number(e.target.value))} className={`${input} w-full`}>
                  {EVERY_OPTIONS.map((o) => (
                    <option key={o.value} value={o.value}>
                      {o.label}
                    </option>
                  ))}
                </select>
              </Field>
            </div>
            <ErrorNote error={actionError} />
            <button type="submit" disabled={busy !== null || !valid || db.read_only} className={primary}>
              {busy === "import" ? (
                <span className="shimmer">Starting…</span>
              ) : (
                <>
                  <CloudDownload className="size-3.5" /> Start import
                </>
              )}
            </button>
            {db.read_only && <p className="text-[12px] text-subtle">This database is read-only: make it writable to import.</p>}
          </form>
        </Panel>
        <Panel title="Other ways in" className="lg:col-span-2">
          <ul className="space-y-3 px-5 py-4 text-[13px] text-muted">
            <li>
              <span className="text-fg">Run on roll.</span> When production is a roll database, branch from it directly —
              nothing to import.
            </li>
            <li>
              <span className="text-fg">From the CLI.</span>{" "}
              <code className="font-mono text-[12px]">roll nosql import {db.name} --from cql://… --keyspaces app --every 24h</code>
            </li>
            <li>
              <span className="text-fg">Backups on S3.</span>{" "}
              <code className="font-mono text-[12px]">roll nosql import {db.name} --from s3://bucket/backups --every 24h</code>{" "}
              re-reads the newest snapshot on every refresh.
            </li>
          </ul>
        </Panel>
      </div>

      <Panel title="Imports" aside={list && <span className="font-mono text-[12px] text-subtle">{list.length}</span>}>
        {error ? (
          <p className="px-5 py-4 text-[13px] text-danger">{error}</p>
        ) : !list ? (
          <p className="px-5 py-6 text-[13px] text-subtle">
            <span className="shimmer">Loading…</span>
          </p>
        ) : list.length === 0 ? (
          <p className="px-5 py-6 text-[13px] text-muted">No imports into {db.name} yet.</p>
        ) : (
          <ul>
            {list.map((i) => (
              <ImportRow key={i.id} i={i} />
            ))}
          </ul>
        )}
      </Panel>
    </div>
  );
}

const STATE_TONE: Record<NosqlImport["state"], string> = {
  queued: "text-subtle",
  running: "text-accent",
  done: "text-muted",
  failed: "text-danger",
};

function ImportRow({ i }: { i: NosqlImport }) {
  const busy = i.state === "running" || i.state === "queued";
  return (
    <li className="rise space-y-2 border-b border-line px-5 py-3.5 last:border-0">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <span className={`flex items-center gap-1.5 font-mono text-[12px] ${STATE_TONE[i.state]}`}>
          {i.state === "running" && <HorizonMark state="deploying" size={14} interactive={false} />}
          {i.state}
        </span>
        <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{i.from}</code>
        <span className="font-mono text-[11px] text-subtle">{i.id}</span>
      </div>
      {(busy || i.tables_total > 0) && (
        <div className="flex items-center gap-3">
          <div className="flex-1">
            <div className="h-1 overflow-hidden rounded-full bg-line" title={`${i.tables_done} of ${i.tables_total} tables`}>
              <div
                className={`h-full rounded-full transition-[width] duration-500 motion-reduce:transition-none ${busy ? "bg-accent" : "bg-fg/30"}`}
                style={{ width: `${i.tables_total ? Math.min(100, (i.tables_done / i.tables_total) * 100) : 0}%` }}
              />
            </div>
          </div>
          <span className="font-mono text-[11px] text-subtle tabular-nums">
            {i.tables_done}/{i.tables_total} tables · {formatCount(i.rows)} rows
          </span>
        </div>
      )}
      <p className="flex flex-wrap gap-x-3 font-mono text-[11px] text-subtle">
        <span>{i.keyspaces.length ? i.keyspaces.join(", ") : "every keyspace"}</span>
        <span>{i.every ? `every ${formatSpan(i.every)}` : "once"}</span>
        <span>
          {i.runs} run{i.runs === 1 ? "" : "s"}
        </span>
        {i.started_at && <span>started {ago(i.started_at)}</span>}
        {i.finished_at && <span>finished {formatTime(i.finished_at)}</span>}
        {i.next_run_at && <span>next {formatTime(i.next_run_at)}</span>}
      </p>
      {i.error && <p className="font-mono text-[12px] break-words text-danger">{i.error}</p>}
    </li>
  );
}
