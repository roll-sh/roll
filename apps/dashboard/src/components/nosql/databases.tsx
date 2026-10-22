"use client";

import { HorizonMark } from "@roll/brand";
import { ChevronRight, GitBranch, Moon, Plus } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useState } from "react";
import { formatBytes } from "@/lib/roll-api";
import {
  branchTree,
  DIALECT_LABEL,
  formatSpan,
  nosqlApi,
  validLabel,
  validName,
  type CreateNosqlReq,
  type Dialect,
  type NosqlDb,
  type NosqlInfo,
} from "@/lib/nosql-api";
import { AppShell } from "../app-shell";
import { btn, Connecting, EmptyPanel, ErrorNote, input, Offline, panel, primary, Stats } from "../ui";
import {
  DialectBadge,
  Expiry,
  Field,
  NosqlStateDot,
  POLL_MS,
  ReadOnlyBadge,
  Segmented,
  SettingsFields,
  stateLabel,
  useAction,
  type NosqlSettings,
} from "./shared";

/** Poll the control plane: nosql info and every database (neither wakes a database). */
function useNosqlList() {
  const [info, setInfo] = useState<NosqlInfo | null>(null);
  const [dbs, setDbs] = useState<NosqlDb[] | null>(null);
  const [offline, setOffline] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const [i, d] = await Promise.all([nosqlApi.info(), nosqlApi.list()]);
      setInfo(i);
      setDbs(d);
      setOffline(false);
    } catch {
      setOffline(true);
    }
  }, []);

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

  return { info, dbs, offline, refresh };
}

export function NosqlDatabases() {
  const router = useRouter();
  const { info, dbs, offline, refresh } = useNosqlList();
  const { busy, error, run } = useAction(refresh);
  const [creating, setCreating] = useState(false);

  return (
    <AppShell
      crumb="nosql"
      actions={
        <button onClick={() => setCreating((c) => !c)} disabled={offline || !info} className={primary}>
          <Plus className="size-3.5" /> New database
        </button>
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-[-0.04em]">NoSQL</h1>
          <p className="mt-1 text-[13px] text-muted">
            ScyllaDB- and Cassandra-compatible databases — scale to zero, branch per pull request, restore to any second.
          </p>
        </div>
        {info && (
          <p className="font-mono text-[11px] text-subtle">
            scylla {info.version.scylla} · cassandra {info.version.cassandra} · {info.storage}
          </p>
        )}
      </div>

      <ErrorNote error={error} />

      {creating && info && (
        <CreateNosql
          info={info}
          busy={busy === "create"}
          onCancel={() => setCreating(false)}
          onCreate={async (req) => {
            const d = await run("create", () => nosqlApi.create(req));
            if (d) {
              setCreating(false);
              router.push(`/dashboard/nosql/${d.id}`);
            }
          }}
        />
      )}

      {offline ? (
        <Offline />
      ) : dbs === null || info === null ? (
        <Connecting />
      ) : dbs.length === 0 ? (
        !creating && (
          <EmptyPanel>
            <h2 className="text-[15px] font-medium">No NoSQL databases yet</h2>
            <p className="max-w-md text-[13px] text-muted">
              Each database speaks CQL like ScyllaDB {info.version.scylla} or Cassandra {info.version.cassandra}, so
              existing drivers and cqlsh work unchanged. It loads on the first connection and scales to zero when idle;
              branches are instant and copy nothing.
            </p>
            <button onClick={() => setCreating(true)} className={primary}>
              <Plus className="size-3.5" /> New database
            </button>
          </EmptyPanel>
        )
      ) : (
        <>
          <Overview info={info} dbs={dbs} />
          <DatabaseList dbs={dbs} info={info} busy={busy} onUnload={(d) => run(`unload-${d.id}`, () => nosqlApi.unload(d.id))} />
        </>
      )}
    </AppShell>
  );
}

function Overview({ info, dbs }: { info: NosqlInfo; dbs: NosqlDb[] }) {
  const loaded = dbs.filter((d) => d.state === "running");
  const branches = dbs.filter((d) => d.parent_id);
  const expiring = branches.filter((d) => d.expires_at != null);
  const stored = dbs.reduce((s, d) => s + (d.storage_bytes ?? 0), 0);
  const conns = dbs.reduce((s, d) => s + (d.connections ?? 0), 0);
  return (
    <div className="mt-6">
      <Stats
        items={[
          { label: "Databases", value: dbs.length - branches.length, hint: `${loaded.length} of ${dbs.length} loaded, branches included` },
          { label: "Branches", value: branches.length, hint: expiring.length ? `${expiring.length} with an expiry` : "none expiring" },
          { label: "Stored", value: formatBytes(stored), hint: `${conns} client${conns === 1 ? "" : "s"} connected` },
          { label: "CQL endpoint", value: info.endpoints.cql, hint: info.tls ? "TLS with SNI" : "plain CQL, native protocol v3–v5" },
        ]}
      />
    </div>
  );
}

function DatabaseList({
  dbs,
  info,
  busy,
  onUnload,
}: {
  dbs: NosqlDb[];
  info: NosqlInfo;
  busy: string | null;
  onUnload: (d: NosqlDb) => void;
}) {
  const rows = branchTree(dbs);
  return (
    <section className={`${panel} mt-4`}>
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h2 className="text-[15px] font-medium">Databases</h2>
        <span className="font-mono text-[12px] text-subtle">{dbs.length}</span>
      </div>
      <ul>
        {rows.map(({ db: d, depth, orphanOf }) => {
          const on = d.state === "running";
          return (
            <li key={d.id} className="rise group relative border-b border-line last:border-0">
              <Link
                href={`/dashboard/nosql/${d.id}`}
                className="grid grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-x-4 gap-y-2 px-5 py-3.5 transition hover:bg-panel-2/60 md:grid-cols-[auto_minmax(0,1.6fr)_minmax(0,1fr)_auto]"
              >
                <NosqlStateDot state={d.state} />
                <div className="flex min-w-0 items-center gap-2" style={{ paddingLeft: `${Math.min(depth, 6) * 20}px` }}>
                  {depth > 0 && <GitBranch className="size-3.5 shrink-0 text-subtle" aria-label="branch of the row above" />}
                  <div className="min-w-0">
                    <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-[13px]">
                      <span className="truncate">{d.name}</span>
                      <DialectBadge dialect={d.dialect} />
                      {d.read_only && <ReadOnlyBadge />}
                      {d.schema_only && <span className="text-[11px] text-subtle">schema only</span>}
                      {d.expires_at != null && <Expiry at={d.expires_at} />}
                    </p>
                    <p className="truncate font-mono text-[11px] text-subtle">
                      {d.id} · {d.datacenter}/{d.rack}
                      {orphanOf && <> · parent {orphanOf} deleted</>}
                    </p>
                  </div>
                </div>
                <div className="hidden min-w-0 font-mono text-[11px] text-subtle md:block">
                  <p className="truncate">
                    {formatBytes(d.storage_bytes)} stored · {d.connections} conn
                  </p>
                  <p className="truncate">
                    {d.durability} · {d.idle_timeout === 0 ? "always on" : `idle after ${formatSpan(d.idle_timeout)}`}
                  </p>
                </div>
                <span className="flex items-center gap-2 text-[12px] text-subtle">
                  <span className="hidden w-24 text-right sm:inline">{stateLabel(d.state)}</span>
                  <ChevronRight className="size-3.5" />
                </span>
              </Link>
              {on && (
                <button
                  title="Scale to zero now"
                  aria-label={`Scale ${d.name} to zero now`}
                  disabled={busy !== null}
                  onClick={() => onUnload(d)}
                  className="absolute top-1/2 right-12 hidden -translate-y-1/2 rounded-md p-1.5 text-subtle transition group-hover:block hover:bg-panel hover:text-fg focus-visible:block disabled:opacity-40"
                >
                  {busy === `unload-${d.id}` ? (
                    <HorizonMark state="thinking" size={14} interactive={false} />
                  ) : (
                    <Moon className="size-3.5" />
                  )}
                </button>
              )}
            </li>
          );
        })}
      </ul>
      <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
        Connect any CQL driver or cqlsh to <span className="font-mono text-muted">{info.endpoints.cql}</span>
        {info.endpoints.cql_shard_aware && (
          <>
            {" "}
            (shard-aware <span className="font-mono text-muted">{info.endpoints.cql_shard_aware}</span>)
          </>
        )}
        ; the username (or, with TLS, the SNI name) picks the database.
      </p>
    </section>
  );
}

function CreateNosql({
  info,
  busy,
  onCreate,
  onCancel,
}: {
  info: NosqlInfo;
  busy: boolean;
  onCreate: (req: CreateNosqlReq) => void;
  onCancel: () => void;
}) {
  const df = info.defaults;
  const [name, setName] = useState("");
  const [dialect, setDialect] = useState<Dialect>((df.dialect as Dialect) === "cassandra" ? "cassandra" : "scylla");
  const [datacenter, setDatacenter] = useState(df.datacenter);
  const [rack, setRack] = useState(df.rack);
  const [cluster, setCluster] = useState("");
  const [settings, setSettings] = useState<NosqlSettings>({
    durability: df.durability === "sync" ? "sync" : "async",
    idle_timeout: df.idle_timeout,
    history_retention: df.history_retention,
  });
  const nameOk = validName(name);
  const labelsOk = validLabel(datacenter) && validLabel(rack) && validLabel(cluster, true);
  const valid = nameOk && labelsOk;
  return (
    <form
      className={`${panel} rise mt-6 space-y-5 p-5`}
      onSubmit={(e) => {
        e.preventDefault();
        if (!valid) return;
        onCreate({
          name,
          dialect,
          datacenter,
          rack,
          ...(cluster ? { cluster_name: cluster } : {}),
          ...settings,
        });
      }}
    >
      <div className="flex flex-wrap items-center gap-2">
        <input
          autoFocus
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="database-name"
          spellCheck={false}
          aria-label="Database name"
          className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
        />
        <button type="submit" disabled={busy || !valid} className={primary}>
          {busy ? <span className="shimmer">Creating…</span> : "Create"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
      {name && !nameOk && <p className="text-[12px] text-danger">Use 1–40 lowercase letters, digits or -.</p>}

      <div className="grid gap-x-3 gap-y-4 sm:grid-cols-2 lg:grid-cols-4">
        <Field label="Dialect" hint={dialect === "scylla" ? `ScyllaDB ${info.version.scylla}` : `Apache Cassandra ${info.version.cassandra}`}>
          <Segmented
            value={dialect}
            options={info.dialects.map((d) => ({ label: DIALECT_LABEL[d], value: d }))}
            onChange={setDialect}
          />
        </Field>
        <Field label="Datacenter" hint="drivers' local DC">
          <input value={datacenter} onChange={(e) => setDatacenter(e.target.value)} spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
        </Field>
        <Field label="Rack">
          <input value={rack} onChange={(e) => setRack(e.target.value)} spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
        </Field>
        <Field label="Cluster name" hint="defaults to the database name">
          <input
            value={cluster}
            onChange={(e) => setCluster(e.target.value)}
            placeholder={name || "database-name"}
            spellCheck={false}
            className={`${input} w-full font-mono text-[12px]`}
          />
        </Field>
      </div>
      {!labelsOk && <p className="text-[12px] text-danger">Datacenter, rack and cluster name: up to 64 characters, no quotes.</p>}
      <SettingsFields value={settings} onChange={setSettings} />
      <p className="text-[12px] text-subtle">
        Match production&apos;s datacenter and rack names and drivers configured with a local DC work unchanged. Billed
        only while loaded ({info.pricing.usd_per_active_hour} USD per hour) plus storage; nothing while scaled to zero.
      </p>
    </form>
  );
}
