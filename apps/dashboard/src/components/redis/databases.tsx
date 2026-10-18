"use client";

import { HorizonMark } from "@roll/brand";
import { ChevronRight, GitBranch, Moon, Plus } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useState } from "react";
import { formatBytes } from "@/lib/roll-api";
import {
  formatCount,
  formatDuration,
  formatMb,
  redisApi,
  type RedisDb,
  type RedisInfo,
  type RedisSettings,
} from "@/lib/redis-api";
import { AppShell } from "../app-shell";
import { btn, Connecting, EmptyPanel, ErrorNote, input, Meter, Offline, panel, primary, Stats } from "../ui";
import { POLL_MS, RedisStateDot, SettingsFields, stateLabel, useAction } from "./shared";

/** Poll the control plane: Redis info and every database. */
function useRedisList() {
  const [info, setInfo] = useState<RedisInfo | null>(null);
  const [dbs, setDbs] = useState<RedisDb[] | null>(null);
  const [offline, setOffline] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const [i, d] = await Promise.all([redisApi.info(), redisApi.list()]);
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

export function RedisDatabases() {
  const router = useRouter();
  const { info, dbs, offline, refresh } = useRedisList();
  const { busy, error, run } = useAction(refresh);
  const [creating, setCreating] = useState(false);

  return (
    <AppShell
      crumb="redis"
      actions={
        <button onClick={() => setCreating((c) => !c)} disabled={offline || !info} className={primary}>
          <Plus className="size-3.5" /> New database
        </button>
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-[-0.04em]">Redis</h1>
          <p className="mt-1 text-[13px] text-muted">
            Serverless Redis — scales to zero, settings apply live, branches and restores to any second.
          </p>
        </div>
        {info && (
          <p className="font-mono text-[11px] text-subtle">
            redis {info.redis_version} · {info.region} · {info.storage}
          </p>
        )}
      </div>

      <ErrorNote error={error} />

      {creating && info && (
        <CreateRedis
          info={info}
          busy={busy === "create"}
          onCancel={() => setCreating(false)}
          onCreate={async (name, settings) => {
            const db = await run("create", () => redisApi.create(name, settings));
            if (db) {
              setCreating(false);
              router.push(`/dashboard/redis/${db.id}`);
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
            <h2 className="text-[15px] font-medium">No Redis databases yet</h2>
            <p className="max-w-md text-[13px] text-muted">
              Each database speaks the Redis protocol and an Upstash-compatible REST API. It loads on the first
              connection and scales to zero when idle — its data stays in object storage, so an unused database costs
              only storage.
            </p>
            <button onClick={() => setCreating(true)} className={primary}>
              <Plus className="size-3.5" /> New database
            </button>
          </EmptyPanel>
        )
      ) : (
        <>
          <Overview info={info} dbs={dbs} />
          <DatabaseList dbs={dbs} info={info} busy={busy} onUnload={(d) => run(`unload-${d.id}`, () => redisApi.unload(d.id))} />
        </>
      )}
    </AppShell>
  );
}

function Overview({ info, dbs }: { info: RedisInfo; dbs: RedisDb[] }) {
  const active = dbs.filter((d) => d.status.state === "active");
  const used = active.reduce((s, d) => s + (d.status.used_memory ?? 0), 0);
  const limit = active.reduce((s, d) => s + d.max_memory_mb * 1024 * 1024, 0);
  const keys = active.reduce((s, d) => s + (d.status.keys ?? 0), 0);
  const replicas = dbs.reduce((s, d) => s + d.replicas.length, 0);
  return (
    <div className="mt-6">
      <Stats
        items={[
          { label: "Databases", value: dbs.length, hint: `${active.length} active · ${dbs.length - active.length} scaled to zero` },
          { label: "Memory in use", value: formatBytes(used), hint: limit ? `of ${formatBytes(limit)} loaded limits` : "nothing loaded" },
          { label: "Keys in memory", value: formatCount(keys), hint: `${replicas} read replica${replicas === 1 ? "" : "s"}` },
          { label: "Endpoint", value: info.resp, hint: `${info.tls ? "rediss://" : "redis://"} · REST ${info.http}` },
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
  dbs: RedisDb[];
  info: RedisInfo;
  busy: string | null;
  onUnload: (d: RedisDb) => void;
}) {
  const names = new Map(dbs.map((d) => [d.id, d.name]));
  return (
    <section className={`${panel} mt-4`}>
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h2 className="text-[15px] font-medium">Databases</h2>
        <span className="font-mono text-[12px] text-subtle">{dbs.length}</span>
      </div>
      <ul>
        {dbs.map((d) => {
          const st = d.status;
          const on = st.state === "active";
          const limit = d.max_memory_mb * 1024 * 1024;
          const parent = d.parent_id ? (names.get(d.parent_id) ?? d.parent_id) : null;
          return (
            <li key={d.id} className="rise group relative border-b border-line last:border-0">
              <Link
                href={`/dashboard/redis/${d.id}`}
                className="grid grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-x-4 gap-y-2 px-5 py-3.5 transition hover:bg-panel-2/60 md:grid-cols-[auto_minmax(0,1.4fr)_minmax(0,1fr)_minmax(0,0.8fr)_auto]"
              >
                <RedisStateDot state={st.state} />
                <div className="min-w-0">
                  <p className="flex items-center gap-1.5 truncate text-[13px]">
                    {d.name}
                    {parent && (
                      <span className="flex items-center gap-1 truncate text-[11px] text-subtle">
                        <GitBranch className="size-3" /> {parent}
                      </span>
                    )}
                  </p>
                  <p className="truncate font-mono text-[11px] text-subtle">
                    {d.id} · {d.region} · {d.host}:{d.port}
                  </p>
                </div>
                <div className="hidden min-w-0 md:block" title={`maxmemory ${formatMb(d.max_memory_mb)}`}>
                  <div className="flex justify-between font-mono text-[10px] text-subtle">
                    <span className={on ? "text-fg" : ""}>{on ? formatBytes(st.used_memory ?? 0) : "0 B"}</span>
                    <span>{formatMb(d.max_memory_mb)}</span>
                  </div>
                  <div className="mt-1">
                    <Meter value={on ? (st.used_memory ?? 0) : 0} max={limit} />
                  </div>
                </div>
                <div className="hidden min-w-0 font-mono text-[11px] text-subtle md:block">
                  <p className="truncate">
                    {on ? `${formatCount(st.keys)} keys · ${st.connections ?? 0} conn` : `idle after ${formatDuration(d.idle_timeout_s)}`}
                  </p>
                  <p className="truncate">
                    {d.eviction_policy} · {d.durability} · {d.replicas.length} replica{d.replicas.length === 1 ? "" : "s"}
                  </p>
                </div>
                <span className="flex items-center gap-2 text-[12px] text-subtle">
                  <span className="hidden w-24 text-right sm:inline">{stateLabel(st.state)}</span>
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
        Connect with any Redis client at <span className="font-mono text-muted">{info.resp}</span>; serverless and edge
        functions use the REST API at <span className="font-mono text-muted">{info.http}</span>.
      </p>
    </section>
  );
}

function CreateRedis({
  info,
  busy,
  onCreate,
  onCancel,
}: {
  info: RedisInfo;
  busy: boolean;
  onCreate: (name: string, settings: RedisSettings) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [settings, setSettings] = useState<RedisSettings>({ ...info.defaults, durability: "async" });
  const valid = /^[A-Za-z0-9_-]{1,63}$/.test(name);
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
          onChange={(e) => setName(e.target.value)}
          placeholder="database-name"
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
      {name && !valid && <p className="text-[12px] text-danger">Use 1–63 letters, digits, - or _.</p>}
      <SettingsFields value={settings} onChange={setSettings} />
      <p className="text-[12px] text-subtle">
        Billed only while loaded: memory at {info.pricing.usd_per_gib_hour.toFixed(4)} USD per GiB-hour, nothing while
        scaled to zero. Everything here can be changed later without a restart.
      </p>
    </form>
  );
}
