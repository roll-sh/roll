"use client";

import { HorizonMark } from "@roll/brand";
import { ArrowLeft, GitBranch, Moon } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useRef, useState } from "react";
import { nosqlApi, type NosqlDb, type NosqlInfo, type NosqlSchema } from "@/lib/nosql-api";
import { AppShell } from "../app-shell";
import { ago, btn, Connecting, EmptyPanel, ErrorNote, Offline } from "../ui";
import { ActivityTab, UsageTab } from "./usage";
import { BranchesTab } from "./branches";
import { CqlConsole } from "./console";
import { DataBrowser } from "./data";
import { ImportsTab } from "./imports";
import { OverviewTab } from "./overview";
import { SchemaTab } from "./schema";
import { DialectBadge, Expiry, formatTime, NosqlStateDot, POLL_MS, ReadOnlyBadge, stateLabel, useAction } from "./shared";

const TABS = ["overview", "branches", "schema", "data", "console", "imports", "usage", "activity"] as const;
type Tab = (typeof TABS)[number];

function useDatabase(id: string) {
  const [info, setInfo] = useState<NosqlInfo | null>(null);
  const [all, setAll] = useState<NosqlDb[] | null>(null);
  const [offline, setOffline] = useState(false);
  const polls = useRef(0);

  const refresh = useCallback(async () => {
    try {
      // The list carries the last measured storage; the detail call re-measures it (it reads
      // the manifest, never loads the database), so only now and then.
      const detail = polls.current++ % 12 === 0 ? nosqlApi.get(id).catch(() => null) : null;
      const [i, d] = await Promise.all([nosqlApi.info(), nosqlApi.list(), detail]);
      setInfo(i);
      setAll(d);
      setOffline(false);
    } catch {
      setOffline(true);
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

  const db = all?.find((d) => d.id === id || d.name === id) ?? null;
  return { info, all, db, offline, refresh };
}

/** The schema, fetched on demand: reading it loads (wakes) the database, so it is never polled. */
export function useSchema(id: string, wanted: boolean) {
  const [schema, setSchema] = useState<NosqlSchema | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const asked = useRef(false);

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      setSchema(await nosqlApi.schema(id));
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    if (wanted && !asked.current) {
      asked.current = true;
      reload();
    }
  }, [wanted, reload]);

  return { schema, error, loading, reload };
}

export type SchemaState = ReturnType<typeof useSchema>;

/** Placeholder while the route's params resolve. */
export function NosqlDatabaseLoading() {
  return (
    <AppShell crumb="nosql">
      <Connecting />
    </AppShell>
  );
}

export function NosqlDatabase({ id }: { id: string }) {
  const router = useRouter();
  const { info, all, db, offline, refresh } = useDatabase(id);
  const { busy, error, setError, run } = useAction(refresh);
  const [tab, setTabRaw] = useState<Tab>("overview");
  const [dataTable, setDataTable] = useState<{ ks: string; table: string } | null>(null);
  const [prefill, setPrefill] = useState<{ text: string; n: number } | null>(null);
  const schema = useSchema(db?.id ?? id, tab === "schema" || tab === "data" || tab === "console");

  useEffect(() => {
    const h = window.location.hash.slice(1) as Tab;
    if (TABS.includes(h)) setTabRaw(h);
  }, []);
  const setTab = (t: Tab) => {
    setTabRaw(t);
    setError(null);
    window.history.replaceState(null, "", t === "overview" ? window.location.pathname : `#${t}`);
  };

  const crumb = (
    <>
      <Link href="/dashboard/nosql" className="text-muted hover:text-fg">
        nosql
      </Link>{" "}
      <span className="text-subtle">/</span> {db?.name ?? id}
    </>
  );

  if (offline) {
    return (
      <AppShell crumb={crumb}>
        <Offline />
      </AppShell>
    );
  }
  if (!all || !info) {
    return (
      <AppShell crumb={crumb}>
        <Connecting />
      </AppShell>
    );
  }
  if (!db) {
    return (
      <AppShell crumb={crumb}>
        <EmptyPanel>
          <HorizonMark state="sleep" size={40} />
          <h2 className="text-[15px] font-medium">No NoSQL database {id}</h2>
          <p className="max-w-md text-[13px] text-muted">It may have been deleted or expired, or the link is wrong.</p>
          <Link href="/dashboard/nosql" className={btn}>
            <ArrowLeft className="size-3.5" /> All NoSQL databases
          </Link>
        </EmptyPanel>
      </AppShell>
    );
  }

  const parent = db.parent_id ? all.find((d) => d.id === db.parent_id) : null;
  const children = all.filter((d) => d.parent_id === db.id);
  const loaded = db.state === "running";

  return (
    <AppShell
      crumb={crumb}
      actions={
        loaded && (
          <button
            onClick={() => run("unload", () => nosqlApi.unload(db.id))}
            disabled={busy !== null}
            className={`${btn} shrink-0`}
            title="Disconnect clients and unload now (data stays in object storage)"
          >
            {busy === "unload" ? <HorizonMark state="thinking" size={14} interactive={false} /> : <Moon className="size-3.5" />}
            Scale to zero
          </button>
        )
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="min-w-0">
          <h1 className="flex items-center gap-3 text-2xl font-semibold tracking-[-0.04em]">
            <span className="truncate">{db.name}</span>
            <DialectBadge dialect={db.dialect} />
          </h1>
          <p className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-[13px] text-muted">
            <NosqlStateDot state={db.state} />
            <span>{stateLabel(db.state)}</span>
            <span className="text-subtle">·</span>
            <span className="font-mono text-[12px] text-subtle">{db.id}</span>
            {db.parent_id && (
              <>
                <span className="text-subtle">·</span>
                <span className="flex items-center gap-1 text-[12px] text-subtle">
                  <GitBranch className="size-3" /> branch of{" "}
                  {parent ? (
                    <Link href={`/dashboard/nosql/${parent.id}`} className="text-accent hover:underline">
                      {parent.name}
                    </Link>
                  ) : (
                    <span className="font-mono">{db.parent_id} (deleted)</span>
                  )}
                  {db.parent_at != null && <> at {formatTime(db.parent_at)}</>}
                  {db.schema_only && <> · schema only</>}
                </span>
              </>
            )}
            {db.read_only && (
              <>
                <span className="text-subtle">·</span>
                <ReadOnlyBadge />
              </>
            )}
            {db.expires_at != null && (
              <>
                <span className="text-subtle">·</span>
                <Expiry at={db.expires_at} />
              </>
            )}
          </p>
        </div>
        <p className="font-mono text-[11px] text-subtle">
          {db.dialect === "scylla" ? `scylla ${info.version.scylla}` : `cassandra ${info.version.cassandra}`} · {db.datacenter}/
          {db.rack} · created {ago(db.created_at)}
        </p>
      </div>

      <nav className="mt-6 flex gap-1 overflow-x-auto border-b border-line" aria-label="Database sections">
        {TABS.map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            aria-current={t === tab ? "page" : undefined}
            className={`-mb-px border-b px-3 py-2 text-[13px] capitalize transition ${
              t === tab ? "border-fg text-fg" : "border-transparent text-muted hover:text-fg"
            }`}
          >
            {t}
            {t === "branches" && children.length > 0 && (
              <span className="ml-1.5 font-mono text-[11px] text-subtle">{children.length}</span>
            )}
          </button>
        ))}
      </nav>

      <ErrorNote error={error} />

      <div className="mt-6">
        {tab === "overview" && (
          <OverviewTab
            key={db.id}
            db={db}
            info={info}
            busy={busy}
            run={run}
            onDeleted={() => router.push("/dashboard/nosql")}
          />
        )}
        {tab === "branches" && <BranchesTab key={db.id} db={db} all={all} busy={busy} run={run} />}
        {tab === "schema" && (
          <SchemaTab
            db={db}
            schema={schema}
            onBrowse={(ks, table) => {
              setDataTable({ ks, table });
              setTab("data");
            }}
            onQuery={(text) => {
              setPrefill((p) => ({ text, n: (p?.n ?? 0) + 1 }));
              setTab("console");
            }}
          />
        )}
        {tab === "data" && (
          <DataBrowser key={db.id} db={db} schema={schema} target={dataTable} onTarget={setDataTable} />
        )}
        {tab === "console" && <CqlConsole key={db.id} db={db} schema={schema} prefill={prefill} />}
        {tab === "imports" && <ImportsTab key={db.id} db={db} />}
        {tab === "usage" && <UsageTab key={db.id} db={db} info={info} />}
        {tab === "activity" && <ActivityTab key={db.id} db={db} />}
      </div>
    </AppShell>
  );
}
