"use client";

import { HorizonMark } from "@roll/brand";
import { ArrowLeft, GitBranch, KeyRound, Moon, Plus, RotateCcw, Trash2 } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useState } from "react";
import { formatBytes, formatUsd } from "@/lib/roll-api";

import {
  formatCount,
  formatDuration,
  formatElapsed,
  formatMb,
  maskRedisUrl,
  maskToken,
  redisApi,
  type RedisDb,
  type RedisInfo,
  type RedisReplica,
  type RedisSettings,
  type RedisUsage,
} from "@/lib/redis-api";
import { AppShell } from "../app-shell";
import {
  ago,
  btn,
  Confirm,
  Connecting,
  CopyButton,
  EmptyPanel,
  ErrorNote,
  input,
  Meter,
  Offline,
  Panel,
  primary,
  SecretField,
  Stats,
  useCopy,
} from "../ui";
import { DataBrowser } from "./data";
import {
  formatTime,
  fromLocalInput,
  now,
  POLL_MS,
  RedisStateDot,
  Segmented,
  SettingsFields,
  stateLabel,
  toLocalInput,
  useAction,
  type Run,
} from "./shared";

const TABS = ["overview", "data", "replicas", "branches", "restore", "usage", "settings"] as const;
type Tab = (typeof TABS)[number];

function useDatabase(id: string) {
  const [info, setInfo] = useState<RedisInfo | null>(null);
  const [all, setAll] = useState<RedisDb[] | null>(null);
  const [offline, setOffline] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const [i, d] = await Promise.all([redisApi.info(), redisApi.list()]);
      setInfo(i);
      setAll(d);
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

  const db = all?.find((d) => d.id === id || d.name === id) ?? null;
  return { info, all, db, offline, refresh };
}

/** Placeholder while the route's params resolve. */
export function RedisDatabaseLoading() {
  return (
    <AppShell crumb="redis">
      <Connecting />
    </AppShell>
  );
}

export function RedisDatabase({ id }: { id: string }) {
  const router = useRouter();
  const { info, all, db, offline, refresh } = useDatabase(id);
  const { busy, error, setError, run } = useAction(refresh);
  const [tab, setTabRaw] = useState<Tab>("overview");

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
      <Link href="/dashboard/redis" className="text-muted hover:text-fg">
        redis
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
          <h2 className="text-[15px] font-medium">No Redis database {id}</h2>
          <p className="max-w-md text-[13px] text-muted">It may have been deleted, or the link is wrong.</p>
          <Link href="/dashboard/redis" className={btn}>
            <ArrowLeft className="size-3.5" /> All Redis databases
          </Link>
        </EmptyPanel>
      </AppShell>
    );
  }

  const parent = db.parent_id ? all.find((d) => d.id === db.parent_id) : null;
  const active = db.status.state === "active";

  return (
    <AppShell
      crumb={crumb}
      actions={
        active && (
          <button
            onClick={() => run("unload", () => redisApi.unload(db.id))}
            disabled={busy !== null}
            className={`${btn} shrink-0`}
            title="Flush to object storage and unload from memory now"
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
          </h1>
          <p className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-[13px] text-muted">
            <RedisStateDot state={db.status.state} />
            <span>{stateLabel(db.status.state)}</span>
            <span className="text-subtle">·</span>
            <span className="font-mono text-[12px] text-subtle">{db.id}</span>
            {db.parent_id && (
              <>
                <span className="text-subtle">·</span>
                <span className="flex items-center gap-1 text-[12px] text-subtle">
                  <GitBranch className="size-3" /> branch of{" "}
                  {parent ? (
                    <Link href={`/dashboard/redis/${parent.id}`} className="text-accent hover:underline">
                      {parent.name}
                    </Link>
                  ) : (
                    <span className="font-mono">{db.parent_id}</span>
                  )}
                  {db.parent_at != null && <> at {formatTime(db.parent_at)}</>}
                </span>
              </>
            )}
          </p>
        </div>
        <p className="font-mono text-[11px] text-subtle">
          redis {info.redis_version} · {db.region} · created {ago(db.created_at)}
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
            {t === "data" ? "Data" : t}
            {t === "replicas" && db.replicas.length > 0 && (
              <span className="ml-1.5 font-mono text-[11px] text-subtle">{db.replicas.length}</span>
            )}
          </button>
        ))}
      </nav>

      <ErrorNote error={error} />

      <div className="mt-6">
        {tab === "overview" && <OverviewTab db={db} info={info} />}
        {tab === "data" && <DataBrowser key={db.id} db={db} />}
        {tab === "replicas" && <ReplicasTab db={db} busy={busy} run={run} />}
        {tab === "branches" && <BranchesTab db={db} all={all} busy={busy} run={run} />}
        {tab === "restore" && <RestoreTab key={db.id} db={db} busy={busy} run={run} />}
        {tab === "usage" && <UsageTab db={db} info={info} />}
        {tab === "settings" && (
          <SettingsTab
            key={db.id}
            db={db}
            busy={busy}
            run={run}
            onDeleted={() => router.push("/dashboard/redis")}
          />
        )}
      </div>
    </AppShell>
  );
}

// --- Overview ----------------------------------------------------------------------------------

function OverviewTab({ db, info }: { db: RedisDb; info: RedisInfo }) {
  const st = db.status;
  const on = st.state === "active";
  const limit = db.max_memory_mb * 1024 * 1024;
  const used = st.used_memory ?? 0;
  return (
    <div className="space-y-4">
      <Stats
        items={[
          {
            label: "State",
            value: on ? "active" : "scaled to zero",
            hint: on
              ? db.idle_timeout_s === 0
                ? "always on"
                : `idle ${formatElapsed(Math.floor((st.idle_ms ?? 0) / 1000))} · unloads after ${formatDuration(db.idle_timeout_s)}`
              : "wakes on the first connection",
          },
          {
            label: "Memory",
            value: on ? formatBytes(used) : "0 B",
            hint: (
              <span className="flex items-center gap-2">
                <span className="w-16">
                  <Meter value={on ? used : 0} max={limit} />
                </span>
                of {formatMb(db.max_memory_mb)} · {db.eviction_policy}
              </span>
            ),
          },
          { label: "Keys", value: on ? formatCount(st.keys) : "—", hint: on ? "in memory" : "loaded on demand" },
          {
            label: "Clients",
            value: on ? formatCount(st.connections) : "0",
            hint: on ? `${formatCount(st.commands)} commands since load` : `${db.durability} durability`,
          },
        ]}
      />
      <div className="grid gap-4 lg:grid-cols-5">
        <Connection db={db} info={info} />
        <Quickstart db={db} />
      </div>
    </div>
  );
}

function Connection({ db, info }: { db: RedisDb; info: RedisInfo }) {
  const { copied, copy } = useCopy();
  return (
    <Panel title="Connect" className="lg:col-span-2">
      <div className="space-y-4 px-5 py-4">
        <div className="space-y-2">
          <p className="text-[12px] text-subtle">{info.tls ? "Redis URL (TLS)" : "Redis URL"}</p>
          <SecretField id="url" value={db.redis_url} masked={maskRedisUrl(db.redis_url)} copied={copied} onCopy={copy} />
          <p className="text-[11px] text-subtle">
            Any Redis client: redis-cli, node-redis, ioredis, redis-py, go-redis. Host{" "}
            <span className="font-mono">
              {db.host}:{db.port}
            </span>
            ; the token below is the password.
          </p>
        </div>
        <div className="space-y-2">
          <p className="text-[12px] text-subtle">REST URL</p>
          <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
            <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{db.rest_url}</code>
            <CopyButton id="rest" text={db.rest_url} copied={copied} onCopy={copy} />
          </div>
          <p className="text-[12px] text-subtle">REST token</p>
          <SecretField id="token" value={db.rest_token} masked={maskToken(db.rest_token)} copied={copied} onCopy={copy} />
          <p className="text-[11px] text-subtle">
            Upstash-compatible: <span className="font-mono">@upstash/redis</span> and plain HTTP from serverless and
            edge functions.
          </p>
        </div>
        <button
          onClick={() =>
            copy("env", `REDIS_URL=${db.redis_url}\nUPSTASH_REDIS_REST_URL=${db.rest_url}\nUPSTASH_REDIS_REST_TOKEN=${db.rest_token}\n`)
          }
          className={btn}
        >
          {copied === "env" ? "Copied .env" : "Copy as .env"}
        </button>
      </div>
    </Panel>
  );
}

const CLIENTS = ["redis-cli", "node-redis", "ioredis", "@upstash/redis", "curl"] as const;

function snippet(client: (typeof CLIENTS)[number], url: string, rest: string, token: string) {
  const tls = url.startsWith("rediss://");
  switch (client) {
    case "redis-cli":
      return `redis-cli ${tls ? "--tls " : ""}-u ${url}\n\n> SET greeting "hello from roll"\nOK\n> GET greeting\n"hello from roll"`;
    case "node-redis":
      return `// npm i redis\nimport { createClient } from "redis";\n\nconst client = await createClient({\n  url: "${url}",\n}).connect();\n\nawait client.set("greeting", "hello from roll");\nconsole.log(await client.get("greeting"));`;
    case "ioredis":
      return `// npm i ioredis\nimport Redis from "ioredis";\n\nconst redis = new Redis("${url}");\n\nawait redis.set("greeting", "hello from roll", "EX", 60);\nconsole.log(await redis.get("greeting"));`;
    case "@upstash/redis":
      return `// npm i @upstash/redis — works in edge and serverless functions\nimport { Redis } from "@upstash/redis";\n\nconst redis = new Redis({\n  url: "${rest}",\n  token: "${token}",\n});\n\nawait redis.set("greeting", "hello from roll");\nconsole.log(await redis.get("greeting"));`;
    case "curl":
      return `curl ${rest} \\\n  -H "Authorization: Bearer ${token}" \\\n  -d '["SET", "greeting", "hello from roll"]'\n\n# {"result":"OK"}`;
  }
}

function Quickstart({ db }: { db: RedisDb }) {
  const [client, setClient] = useState<(typeof CLIENTS)[number]>("redis-cli");
  const [reveal, setReveal] = useState(false);
  const { copied, copy } = useCopy();
  const full = snippet(client, db.redis_url, db.rest_url, db.rest_token);
  const shown = reveal ? full : snippet(client, maskRedisUrl(db.redis_url), db.rest_url, maskToken(db.rest_token));
  return (
    <Panel
      title="Quickstart"
      className="lg:col-span-3"
      aside={
        <div className="flex items-center gap-1">
          <button onClick={() => setReveal((r) => !r)} className="px-1.5 text-[12px] text-subtle hover:text-fg">
            {reveal ? "Hide secrets" : "Show secrets"}
          </button>
          <CopyButton id="snippet" text={full} copied={copied} onCopy={copy} />
        </div>
      }
    >
      <div className="flex gap-1 overflow-x-auto border-b border-line px-4 py-2">
        {CLIENTS.map((c) => (
          <button
            key={c}
            onClick={() => setClient(c)}
            className={`shrink-0 rounded-md px-2 py-1 font-mono text-[11px] transition ${
              c === client ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"
            }`}
          >
            {c}
          </button>
        ))}
      </div>
      <pre className="min-h-56 flex-1 overflow-x-auto px-5 py-4 font-mono text-[12px] leading-relaxed text-muted">
        {shown}
      </pre>
    </Panel>
  );
}

// --- Replicas ----------------------------------------------------------------------------------

function ReplicasTab({ db, busy, run }: { db: RedisDb; busy: string | null; run: Run }) {
  const { copied, copy } = useCopy();
  const [confirming, setConfirming] = useState<string | null>(null);
  return (
    <Panel
      title="Read replicas"
      aside={
        <button onClick={() => run("replica", () => redisApi.addReplica(db.id))} disabled={busy !== null} className={btn}>
          {busy === "replica" ? <span className="shimmer">Adding…</span> : (
            <>
              <Plus className="size-3.5" /> Add replica
            </>
          )}
        </button>
      }
    >
      {db.replicas.length === 0 ? (
        <p className="px-5 py-6 text-[13px] text-muted">
          No replicas. A read replica follows this database through object storage with its own read-only endpoint and
          token, and scales to zero on its own when unused.
        </p>
      ) : (
        <ul>
          {db.replicas.map((r) => (
            <li key={r.id} className="rise space-y-2 border-b border-line px-5 py-3.5 last:border-0">
              <div className="flex flex-wrap items-center gap-3">
                <RedisStateDot state={r.status?.state} />
                <div className="min-w-0 flex-1">
                  <p className="font-mono text-[12px]">{r.id}</p>
                  <p className="text-[11px] text-subtle">
                    {r.region} · read-only · added {ago(r.created_at)}
                  </p>
                </div>
                <ReplicaLag r={r} primaryActive={db.status.state === "active"} />
                {confirming === r.id ? (
                  <span className="flex items-center gap-1.5">
                    <button
                      disabled={busy !== null}
                      onClick={async () => {
                        await run(`del-${r.id}`, () => redisApi.deleteReplica(r.id));
                        setConfirming(null);
                      }}
                      className="rounded-lg bg-danger px-2.5 py-1.5 text-[12px] font-medium text-white disabled:opacity-40"
                    >
                      {busy === `del-${r.id}` ? <span className="shimmer">Removing…</span> : "Remove"}
                    </button>
                    <button onClick={() => setConfirming(null)} className={btn}>
                      Cancel
                    </button>
                  </span>
                ) : (
                  <button
                    onClick={() => setConfirming(r.id)}
                    className="rounded-md p-1.5 text-subtle transition hover:bg-panel-2 hover:text-danger"
                    title="Remove replica"
                    aria-label={`Remove replica ${r.id}`}
                  >
                    <Trash2 className="size-3.5" />
                  </button>
                )}
              </div>
              <SecretField id={r.id} value={r.redis_url} masked={maskRedisUrl(r.redis_url)} copied={copied} onCopy={copy} />
            </li>
          ))}
        </ul>
      )}
      <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
        Settings changes apply to replicas too. A restore or delete disconnects them; they reload on the next connection.
      </p>
    </Panel>
  );
}

function ReplicaLag({ r, primaryActive }: { r: RedisReplica; primaryActive: boolean }) {
  const st = r.status;
  let text: string;
  if (!st) text = "—";
  else if (st.state !== "active") text = "scaled to zero";
  else if (st.lag_bytes == null || !primaryActive) text = `offset ${formatCount(st.offset)}`;
  else if (st.lag_bytes === 0) text = "in sync";
  else text = `${formatBytes(st.lag_bytes)} behind`;
  return (
    <div className="text-right">
      <p className={`font-mono text-[12px] ${st?.state === "active" && st.lag_bytes === 0 ? "text-fg" : "text-muted"}`}>
        {text}
      </p>
      {st?.state === "active" && (
        <p className="font-mono text-[11px] text-subtle">
          {formatBytes(st.used_memory ?? 0)} · {st.connections ?? 0} conn
        </p>
      )}
    </div>
  );
}

// --- Branches & restore --------------------------------------------------------------------------

/** Earliest restorable second: the restore window, but never before the database existed. */
function earliest(db: RedisDb) {
  return Math.max(db.created_at, now() - db.history_retention_s);
}

function PointInTime({ db, value, onChange }: { db: RedisDb; value: number; onChange: (t: number) => void }) {
  const min = earliest(db);
  const presets = [
    { label: "1 min ago", s: 60 },
    { label: "15 min ago", s: 900 },
    { label: "1 hour ago", s: 3600 },
    { label: "1 day ago", s: 86_400 },
  ].filter((p) => now() - p.s >= min);
  return (
    <div className="space-y-2">
      <input
        type="datetime-local"
        step={1}
        value={toLocalInput(value)}
        min={toLocalInput(min)}
        max={toLocalInput(now())}
        onChange={(e) => {
          const t = fromLocalInput(e.target.value);
          if (t != null) onChange(t);
        }}
        className={`${input} w-full font-mono text-[12px]`}
      />
      <div className="flex flex-wrap gap-1">
        {presets.map((p) => (
          <button
            key={p.s}
            type="button"
            onClick={() => onChange(now() - p.s)}
            className="rounded-full border border-line px-2.5 py-0.5 text-[11px] text-muted transition hover:border-line-2 hover:text-fg"
          >
            {p.label}
          </button>
        ))}
      </div>
      <p className="text-[11px] text-subtle">
        Window: {formatTime(min)} → now ({formatDuration(db.history_retention_s)} retention)
      </p>
    </div>
  );
}

function BranchesTab({ db, all, busy, run }: { db: RedisDb; all: RedisDb[]; busy: string | null; run: Run }) {
  const router = useRouter();
  const children = all.filter((d) => d.parent_id === db.id);
  const [name, setName] = useState(`${db.name}-branch`.slice(0, 63));
  const [mode, setMode] = useState<"now" | "past">("now");
  const [at, setAt] = useState(() => Math.max(earliest(db), now() - 900));
  const valid = /^[A-Za-z0-9_-]{1,63}$/.test(name);
  const noWindow = db.history_retention_s === 0;

  return (
    <div className="grid gap-4 lg:grid-cols-5">
      <Panel title="Branches" aside={<span className="font-mono text-[12px] text-subtle">{children.length}</span>} className="lg:col-span-3">
        {children.length === 0 ? (
          <p className="px-5 py-6 text-[13px] text-muted">
            No branches yet. A branch is a new database with this one&apos;s data as of now or any second in the restore
            window — instant, nothing is copied.
          </p>
        ) : (
          <ul>
            {children.map((b) => (
              <li key={b.id} className="rise border-b border-line last:border-0">
                <Link
                  href={`/dashboard/redis/${b.id}`}
                  className="flex items-center gap-3 px-5 py-3 transition hover:bg-panel-2/60"
                >
                  <RedisStateDot state={b.status.state} />
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-[13px]">{b.name}</p>
                    <p className="font-mono text-[11px] text-subtle">
                      {b.id} · data as of {b.parent_at != null ? formatTime(b.parent_at) : "—"}
                    </p>
                  </div>
                  <span className="text-[12px] text-subtle">{ago(b.created_at)}</span>
                </Link>
              </li>
            ))}
          </ul>
        )}
      </Panel>

      <Panel title="New branch" className="lg:col-span-2">
        <form
          className="space-y-4 px-5 py-4"
          onSubmit={async (e) => {
            e.preventDefault();
            if (!valid) return;
            const b = await run("branch", () => redisApi.branch(db.id, name, mode === "past" ? at : undefined));
            if (b) router.push(`/dashboard/redis/${b.id}`);
          }}
        >
          <label className="block space-y-1">
            <span className="block text-[11px] text-subtle">Name</span>
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              spellCheck={false}
              className={`${input} w-full font-mono text-[12px]`}
            />
          </label>
          <div className="space-y-2">
            <span className="block text-[11px] text-subtle">Data from</span>
            <Segmented
              value={mode}
              options={[
                { label: "now", value: "now" },
                { label: "a point in time", value: "past" },
              ]}
              onChange={setMode}
            />
          </div>
          {mode === "past" &&
            (noWindow ? (
              <p className="text-[12px] text-muted">Point-in-time restore is off for this database (see Settings).</p>
            ) : (
              <PointInTime db={db} value={at} onChange={setAt} />
            ))}
          <button type="submit" disabled={busy !== null || !valid || (mode === "past" && noWindow)} className={primary}>
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

function RestoreTab({ db, busy, run }: { db: RedisDb; busy: string | null; run: Run }) {
  const [at, setAt] = useState(() => Math.max(earliest(db), now() - 900));
  const [confirming, setConfirming] = useState(false);
  const [done, setDone] = useState<{ restored_to: number; previous_state: string } | null>(null);

  if (db.history_retention_s === 0) {
    return (
      <Panel title="Point-in-time restore">
        <p className="px-5 py-6 text-[13px] text-muted">
          Point-in-time restore is off for this database. Set a restore window in Settings to keep its history.
        </p>
      </Panel>
    );
  }

  return (
    <div className="grid gap-4 lg:grid-cols-5">
      <Panel title="Point-in-time restore" className="lg:col-span-3">
        <div className="space-y-4 px-5 py-4">
          <p className="text-[13px] text-muted">
            Rewind <span className="text-fg">{db.name}</span> in place to any second in its restore window. Connected
            clients are disconnected and reconnect to the restored data; replicas reload. The current state is archived
            first, for as long as the restore window.
          </p>
          <PointInTime db={db} value={at} onChange={setAt} />
          {confirming ? (
            <Confirm
              word={db.name}
              action="Restore"
              busy={busy === "restore"}
              onCancel={() => setConfirming(false)}
              onConfirm={async () => {
                const r = await run("restore", () => redisApi.restore(db.id, at));
                if (r) {
                  setDone(r);
                  setConfirming(false);
                }
              }}
            >
              Restore <span className="text-fg">{db.name}</span> to{" "}
              <span className="font-mono text-fg">{formatTime(at)}</span> ({ago(at)})? Every write after that is replaced
              by the restored state.
            </Confirm>
          ) : (
            <button
              onClick={() => {
                setDone(null);
                setConfirming(true);
              }}
              disabled={busy !== null || at < earliest(db) || at > now()}
              className={primary}
            >
              <RotateCcw className="size-3.5" /> Review restore
            </button>
          )}
        </div>
      </Panel>
      <Panel title="Last restore" className="lg:col-span-2">
        {done ? (
          <div className="rise space-y-2 px-5 py-4 text-[13px]">
            <p>
              Restored to <span className="font-mono">{formatTime(done.restored_to)}</span>.
            </p>
            <p className="text-muted">The state before the restore is archived as</p>
            <p className="rounded-lg border border-line bg-bg px-2.5 py-2 font-mono text-[11px] break-all">
              {done.previous_state}
            </p>
          </div>
        ) : (
          <p className="px-5 py-6 text-[13px] text-muted">
            Prefer to look before you rewind? Create a branch at the same point in time and inspect it first.
          </p>
        )}
      </Panel>
    </div>
  );
}

// --- Usage ------------------------------------------------------------------------------------------

/** Dollars, keeping fractions of a cent visible: scale-to-zero bills are often tiny. */
function usd(n: number) {
  if (n === 0) return "$0";
  if (n < 0.0001) return "<$0.0001";
  return formatUsd(n);
}

function UsageTab({ db, info }: { db: RedisDb; info: RedisInfo }) {
  const [days, setDays] = useState(30);
  const [usage, setUsage] = useState<RedisUsage | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    const load = () =>
      redisApi.usage(db.id, days).then(
        (u) => {
          if (!alive) return;
          setUsage(u);
          setErr(null);
        },
        (e) => alive && setErr(e instanceof Error ? e.message : String(e)),
      );
    load();
    const t = setInterval(load, POLL_MS * 4);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [db.id, days]);

  const u = usage?.days === days ? usage : null;
  const c = u?.cost;
  const savings = c && c.elasticache_serverless_usd > 0 ? 1 - c.total_usd / c.elasticache_serverless_usd : null;
  const p = info.pricing;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[13px] text-muted">Metered every minute while loaded. Nothing accrues while scaled to zero.</p>
        <div className="w-48">
          <Segmented
            value={days}
            options={[
              { label: "24h", value: 1 },
              { label: "7d", value: 7 },
              { label: "30d", value: 30 },
            ]}
            onChange={setDays}
          />
        </div>
      </div>
      {err && <p className="text-[13px] text-danger">{err}</p>}
      <Stats
        items={[
          {
            label: "Active time",
            value: u ? `${(u.active_seconds / 3600).toFixed(1)} h` : "—",
            hint: u ? `${((u.active_seconds / (days * 86_400)) * 100).toFixed(1)}% of the period loaded` : undefined,
          },
          {
            label: "Memory",
            value: u ? `${u.gib_hours.toFixed(3)} GiB-h` : "—",
            hint: u ? `peak ${formatBytes(u.peak_memory_bytes)}` : undefined,
          },
          { label: "Commands", value: u ? formatCount(u.commands) : "—", hint: u ? `${formatBytes(u.net_in_bytes)} in` : undefined },
          {
            label: "Egress",
            value: u ? formatBytes(u.net_out_bytes) : "—",
            hint: u ? `${formatBytes(u.storage_bytes)} stored` : undefined,
          },
        ]}
      />
      <div className="grid gap-4 lg:grid-cols-5">
        <Panel
          title="AWS cost estimate"
          aside={<span className="font-mono text-[11px] text-subtle">last {days === 1 ? "24 hours" : `${days} days`}</span>}
          className="lg:col-span-3"
        >
          <div className="px-5 py-4">
            <CostRows
              rows={
                c
                  ? [
                      { label: "Memory", detail: `${p.usd_per_gib_hour} USD / GiB-hour loaded`, usd: c.memory_usd },
                      { label: "Compute", detail: `${p.usd_per_million_commands} USD / million commands`, usd: c.compute_usd },
                      { label: "Storage", detail: `S3 at ${p.usd_per_gb_month_s3} USD / GB-month`, usd: c.storage_usd },
                      { label: "Egress", detail: `${p.usd_per_gb_out} USD / GB out`, usd: c.egress_usd },
                    ]
                  : []
              }
              total={c?.total_usd ?? 0}
            />
          </div>
        </Panel>
        <Panel title="vs ElastiCache Serverless" className="lg:col-span-2">
          <div className="space-y-4 px-5 py-4">
            <CompareBars
              rows={[
                { label: "roll", usd: c?.total_usd ?? 0, accent: true },
                { label: "ElastiCache Serverless", usd: c?.elasticache_serverless_usd ?? 0 },
              ]}
            />
            <p className="text-[13px]">
              {savings == null ? (
                <span className="text-subtle">—</span>
              ) : savings > 0 ? (
                <>
                  <span className="font-mono font-semibold">
                    {savings > 0.99 ? ">99" : Math.round(savings * 100)}%
                  </span>{" "}
                  <span className="text-muted">less for the same workload</span>
                </>
              ) : (
                <span className="text-muted">About the same as ElastiCache for this workload.</span>
              )}
            </p>
            <p className="text-[11px] text-subtle">
              ElastiCache Serverless is always on: {p.elasticache_usd_per_gb_hour} USD per GB-hour with a{" "}
              {p.elasticache_min_gb} GB minimum, plus {p.elasticache_usd_per_million_ecpu} USD per million ECPUs and the
              same egress. us-east-1 list prices.
            </p>
          </div>
        </Panel>
      </div>
    </div>
  );
}

/** Cost breakdown: one neutral series, so bars carry magnitude and text carries identity. */
function CostRows({ rows, total }: { rows: { label: string; detail: string; usd: number }[]; total: number }) {
  if (rows.length === 0) return <p className="text-[13px] text-subtle">Loading…</p>;
  const max = Math.max(...rows.map((r) => r.usd), 1e-9);
  return (
    <table className="w-full text-[13px]">
      <tbody>
        {rows.map((r) => (
          <tr key={r.label} className="group border-b border-line" title={`${r.label}: $${r.usd.toPrecision(3)}`}>
            <td className="py-2.5 pr-3 align-top">
              <p>{r.label}</p>
              <p className="text-[11px] text-subtle">{r.detail}</p>
            </td>
            <td className="hidden w-2/5 py-2.5 pr-3 align-middle sm:table-cell">
              <div className="h-1.5 rounded-full bg-line">
                <div
                  className="h-full rounded-full bg-fg/30 transition-[width] duration-300 group-hover:bg-fg/60 motion-reduce:transition-none"
                  style={{ width: `${r.usd > 0 ? Math.max(2, (r.usd / max) * 100) : 0}%` }}
                />
              </div>
            </td>
            <td className="py-2.5 text-right align-top font-mono tabular-nums">{usd(r.usd)}</td>
          </tr>
        ))}
        <tr>
          <td className="pt-3 font-medium">Total</td>
          <td className="hidden sm:table-cell" />
          <td className="pt-3 text-right font-mono font-semibold tabular-nums">{usd(total)}</td>
        </tr>
      </tbody>
    </table>
  );
}

function CompareBars({ rows }: { rows: { label: string; usd: number; accent?: boolean }[] }) {
  const max = Math.max(...rows.map((r) => r.usd), 1e-9);
  return (
    <div className="space-y-3">
      {rows.map((r) => (
        <div key={r.label} title={`${r.label}: $${r.usd.toPrecision(3)}`}>
          <div className="flex items-baseline justify-between text-[12px]">
            <span className="text-muted">{r.label}</span>
            <span className="font-mono tabular-nums">{usd(r.usd)}</span>
          </div>
          <div className="mt-1.5 h-2 rounded-full bg-line">
            <div
              className={`h-full rounded-full transition-[width] duration-300 motion-reduce:transition-none ${
                r.accent ? "bg-accent" : "bg-fg/30"
              }`}
              style={{ width: `${r.usd > 0 ? Math.max(1.5, (r.usd / max) * 100) : 0}%` }}
            />
          </div>
        </div>
      ))}
    </div>
  );
}

// --- Settings, credentials, danger zone ----------------------------------------------------------

function settingsOf(db: RedisDb): RedisSettings {
  return {
    max_memory_mb: db.max_memory_mb,
    eviction_policy: db.eviction_policy,
    durability: db.durability,
    idle_timeout_s: db.idle_timeout_s,
    history_retention_s: db.history_retention_s,
  };
}

function SettingsTab({
  db,
  busy,
  run,
  onDeleted,
}: {
  db: RedisDb;
  busy: string | null;
  run: Run;
  onDeleted: () => void;
}) {
  const saved = settingsOf(db);
  const [draft, setDraft] = useState<RedisSettings>(saved);
  const [name, setName] = useState(db.name);
  const [rotating, setRotating] = useState(false);
  const [rotated, setRotated] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const changed = (Object.keys(saved) as (keyof RedisSettings)[]).filter((k) => draft[k] !== saved[k]);
  const renamed = name !== db.name;
  const validName = /^[A-Za-z0-9_-]{1,63}$/.test(name);

  return (
    <div className="space-y-4">
      <Panel
        title="Settings"
        aside={<span className="text-[12px] text-subtle">applied live — no restart, clients stay connected</span>}
      >
        <form
          className="space-y-5 px-5 py-4"
          onSubmit={(e) => {
            e.preventDefault();
            const patch: Partial<RedisSettings> & { name?: string } = {};
            for (const k of changed) Object.assign(patch, { [k]: draft[k] });
            if (renamed) patch.name = name;
            run("settings", () => redisApi.update(db.id, patch));
          }}
        >
          <label className="block max-w-sm space-y-1">
            <span className="block text-[11px] text-subtle">Name</span>
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              spellCheck={false}
              className={`${input} w-full font-mono text-[12px]`}
            />
          </label>
          <SettingsFields value={draft} onChange={setDraft} />
          <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line pt-4">
            <span className="text-[12px] text-subtle">
              {changed.length || renamed
                ? `${changed.length + (renamed ? 1 : 0)} unsaved change${changed.length + (renamed ? 1 : 0) === 1 ? "" : "s"}`
                : "Up to date"}
            </span>
            <div className="flex gap-2">
              {(changed.length > 0 || renamed) && (
                <button
                  type="button"
                  onClick={() => {
                    setDraft(saved);
                    setName(db.name);
                  }}
                  className={btn}
                >
                  Reset
                </button>
              )}
              <button
                type="submit"
                disabled={busy !== null || (!changed.length && !renamed) || !validName}
                className={primary}
              >
                {busy === "settings" ? <span className="shimmer">Applying…</span> : "Save"}
              </button>
            </div>
          </div>
        </form>
      </Panel>

      <div className="grid gap-4 lg:grid-cols-2">
        <Panel title="Credentials">
          <div className="space-y-3 px-5 py-4">
            <p className="text-[13px] text-muted">
              Rotate the password (and REST token). Existing connections stay up; new connections need the new one.
              Read replicas keep their own tokens.
            </p>
            {rotated && <p className="rise text-[12px] text-ok">Rotated. Copy the new URL and token from Overview.</p>}
            {rotating ? (
              <div className="flex gap-2">
                <button
                  disabled={busy !== null}
                  onClick={async () => {
                    const r = await run("rotate", () => redisApi.rotatePassword(db.id));
                    setRotating(false);
                    setRotated(!!r);
                  }}
                  className={primary}
                >
                  {busy === "rotate" ? <span className="shimmer">Rotating…</span> : "Rotate now"}
                </button>
                <button onClick={() => setRotating(false)} className={btn}>
                  Cancel
                </button>
              </div>
            ) : (
              <button
                onClick={() => {
                  setRotated(false);
                  setRotating(true);
                }}
                className={btn}
              >
                <KeyRound className="size-3.5" /> Rotate password
              </button>
            )}
          </div>
        </Panel>

        <Panel title="Delete database">
          <div className="space-y-3 px-5 py-4">
            <p className="text-[13px] text-muted">
              Disconnects every client, removes {db.replicas.length ? "its replicas and " : ""}the database and its
              restore history. Branches of it keep working.
            </p>
            {deleting ? (
              <Confirm
                word={db.name}
                action="Delete database"
                busy={busy === "delete"}
                onCancel={() => setDeleting(false)}
                onConfirm={async () => {
                  const r = await run("delete", () => redisApi.remove(db.id));
                  if (r) onDeleted();
                }}
              >
                This can&apos;t be undone.
              </Confirm>
            ) : (
              <button onClick={() => setDeleting(true)} className={`${btn} hover:border-danger/50 hover:text-danger`}>
                <Trash2 className="size-3.5" /> Delete {db.name}
              </button>
            )}
          </div>
        </Panel>
      </div>
    </div>
  );
}
