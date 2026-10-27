"use client";

import { HorizonMark } from "@roll/brand";
import { GitBranch, GitCompare, RefreshCw, RotateCcw } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useState } from "react";
import { branchTree, descendants, formatSpan, nosqlApi, validName, type NosqlDb, type NosqlDiff } from "@/lib/nosql-api";
import { diffKind } from "@/lib/nosql-cql";
import { ago, btn, Confirm, CopyButton, input, Panel, primary, useCopy } from "../ui";
import { ExpiryField } from "./overview";
import {
  earliest,
  Expiry,
  Field,
  formatTime,
  fromLocalInput,
  now,
  NosqlStateDot,
  ReadOnlyBadge,
  Segmented,
  toLocalInput,
  type Run,
} from "./shared";

function PointInTime({ db, value, onChange }: { db: NosqlDb; value: number; onChange: (t: number) => void }) {
  const t = now();
  const min = earliest(db, t);
  const presets = [
    { label: "1 min ago", s: 60 },
    { label: "15 min ago", s: 900 },
    { label: "1 hour ago", s: 3600 },
    { label: "1 day ago", s: 86_400 },
  ].filter((p) => t - p.s >= min);
  return (
    <div className="space-y-2">
      <input
        type="datetime-local"
        step={1}
        value={toLocalInput(value)}
        min={toLocalInput(min)}
        max={toLocalInput(t)}
        onChange={(e) => {
          const v = fromLocalInput(e.target.value);
          if (v != null) onChange(v);
        }}
        aria-label="Point in time"
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
        Window: {formatTime(min)} → now ({formatSpan(db.history_retention)} history)
      </p>
    </div>
  );
}

export function BranchesTab({ db, all, busy, run }: { db: NosqlDb; all: NosqlDb[]; busy: string | null; run: Run }) {
  const parent = db.parent_id ? all.find((d) => d.id === db.parent_id) ?? null : null;
  return (
    <div className="space-y-4">
      <div className="grid gap-4 lg:grid-cols-5">
        <BranchList db={db} all={all} />
        <NewBranch db={db} busy={busy} run={run} />
      </div>
      {db.parent_id && <SchemaDiff db={db} parent={parent} />}
      <div className="grid gap-4 lg:grid-cols-2">
        {db.parent_id && <ResetPanel db={db} parent={parent} busy={busy} run={run} />}
        <RestorePanel db={db} busy={busy} run={run} />
      </div>
    </div>
  );
}

function BranchList({ db, all }: { db: NosqlDb; all: NosqlDb[] }) {
  const below = descendants(all, db.id);
  const rows = branchTree([db, ...below]).filter((r) => r.db.id !== db.id);
  return (
    <Panel title="Branches" aside={<span className="font-mono text-[12px] text-subtle">{below.length}</span>} className="lg:col-span-3">
      {rows.length === 0 ? (
        <p className="px-5 py-6 text-[13px] text-muted">
          No branches yet. A branch is a new database with this one&apos;s schema, roles and data as of now or any second
          in its history — created instantly, nothing copied. Give each pull request its own, with an expiry.
        </p>
      ) : (
        <ul>
          {rows.map(({ db: b, depth }) => (
            <li key={b.id} className="rise border-b border-line last:border-0">
              <Link href={`/dashboard/nosql/${b.id}`} className="flex items-center gap-3 px-5 py-3 transition hover:bg-panel-2/60">
                <NosqlStateDot state={b.state} />
                <div className="flex min-w-0 flex-1 items-center gap-2" style={{ paddingLeft: `${Math.min(depth - 1, 6) * 20}px` }}>
                  {depth > 1 && <GitBranch className="size-3.5 shrink-0 text-subtle" />}
                  <div className="min-w-0">
                    <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-[13px]">
                      <span className="truncate">{b.name}</span>
                      {b.read_only && <ReadOnlyBadge />}
                      {b.schema_only && <span className="text-[11px] text-subtle">schema only</span>}
                      {b.expires_at != null && <Expiry at={b.expires_at} />}
                    </p>
                    <p className="truncate font-mono text-[11px] text-subtle">
                      {b.id} · data as of {b.parent_at != null ? formatTime(b.parent_at) : "—"}
                    </p>
                  </div>
                </div>
                <span className="shrink-0 text-[12px] text-subtle">{ago(b.created_at)}</span>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

function NewBranch({ db, busy, run }: { db: NosqlDb; busy: string | null; run: Run }) {
  const router = useRouter();
  const [name, setName] = useState(`${db.name}-branch`.slice(0, 40));
  const [mode, setMode] = useState<"now" | "past">("now");
  const [at, setAt] = useState(() => Math.max(earliest(db, now()), now() - 900));
  const [contents, setContents] = useState<"data" | "schema">("data");
  const [access, setAccess] = useState<"rw" | "ro">("rw");
  const [expires, setExpires] = useState<number | null>(null);
  const valid = validName(name) && (expires == null || expires > now());
  const noWindow = db.history_retention === 0;

  return (
    <Panel title="New branch" className="lg:col-span-2">
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!valid) return;
          const b = await run("branch", () =>
            nosqlApi.branch(db.id, {
              name,
              ...(mode === "past" ? { at } : {}),
              ...(contents === "schema" ? { schema_only: true } : {}),
              ...(access === "ro" ? { read_only: true } : {}),
              ...(expires != null ? { expires_at: expires } : {}),
            }),
          );
          if (b) router.push(`/dashboard/nosql/${b.id}`);
        }}
      >
        <Field label="Name" hint={name && !validName(name) ? "1–40 lowercase letters, digits or -" : undefined}>
          <input value={name} onChange={(e) => setName(e.target.value)} spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
        </Field>
        <Field label="Data from">
          <Segmented
            value={mode}
            options={[
              { label: "now", value: "now" },
              { label: "a point in time", value: "past" },
            ]}
            onChange={setMode}
          />
        </Field>
        {mode === "past" &&
          (noWindow ? (
            <p className="text-[12px] text-muted">Point-in-time history is off for this database (see Overview → Settings).</p>
          ) : (
            <PointInTime db={db} value={at} onChange={setAt} />
          ))}
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Contents" hint={contents === "schema" ? "schema and roles, no rows" : "everything, zero-copy"}>
            <Segmented
              value={contents}
              options={[
                { label: "data", value: "data" },
                { label: "schema", value: "schema" },
              ]}
              onChange={setContents}
            />
          </Field>
          <Field label="Access" hint={access === "ro" ? "safe production debugging" : undefined}>
            <Segmented
              value={access}
              options={[
                { label: "read-write", value: "rw" },
                { label: "read-only", value: "ro" },
              ]}
              onChange={setAccess}
            />
          </Field>
        </div>
        <ExpiryField value={expires} onChange={setExpires} />
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
  );
}

function SchemaDiff({ db, parent }: { db: NosqlDb; parent: NosqlDb | null }) {
  const [diff, setDiff] = useState<NosqlDiff | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const { copied, copy } = useCopy();
  const loaded = db.state === "running" && parent?.state === "running";

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setDiff(await nosqlApi.diff(db.id));
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [db.id]);

  // Comparing loads both databases: do it at once only when they're already loaded.
  useEffect(() => {
    if (loaded) load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [db.id]);

  return (
    <Panel
      title={
        <span className="flex items-center gap-2">
          <GitCompare className="size-4 text-subtle" /> Schema diff vs {parent?.name ?? "parent"}
        </span>
      }
      aside={
        <div className="flex items-center gap-1">
          {diff && diff.diff && <CopyButton id="diff" text={diff.diff} copied={copied} onCopy={copy} />}
          <button onClick={load} disabled={loading || !parent} className={btn}>
            {loading ? <HorizonMark state="thinking" size={14} interactive={false} /> : <RefreshCw className="size-3.5" />}
            {diff ? "Refresh" : "Compare"}
          </button>
        </div>
      }
    >
      {error ? (
        <p className="px-5 py-4 text-[13px] text-danger">{error}</p>
      ) : !diff ? (
        <p className="px-5 py-6 text-[13px] text-muted">
          {loading ? (
            <span className="shimmer">Describing both schemas…</span>
          ) : (
            <>
              The CQL that turns {parent?.name ?? "the parent"}&apos;s schema into this branch&apos;s, generated from both
              sides&apos; DESCRIBE output. Comparing wakes both databases if they&apos;re scaled to zero.
            </>
          )}
        </p>
      ) : diff.statements.length === 0 ? (
        <p className="px-5 py-6 text-[13px] text-muted">No schema changes: this branch matches {parent?.name ?? "its parent"}.</p>
      ) : (
        <div className={`py-2 ${loading ? "opacity-60" : ""}`}>
          <ol className="font-mono text-[12px] leading-relaxed">
            {diff.statements.map((s, i) => (
              <DiffStatement key={i} text={s} />
            ))}
          </ol>
          <p className="border-t border-line px-5 pt-3 pb-1 text-[11px] text-subtle">
            {diff.statements.length} statement{diff.statements.length === 1 ? "" : "s"} · run them against{" "}
            {parent?.name ?? "the parent"} to apply this branch&apos;s migration
            {!diff.described && " · generated (DESCRIBE wasn't available)"}
          </p>
        </div>
      )}
    </Panel>
  );
}

const MARK = {
  create: { sign: "+", label: "adds", line: "border-ok/50 bg-ok/[0.04]", sign_: "text-ok" },
  drop: { sign: "−", label: "drops", line: "border-danger/50 bg-danger/[0.04]", sign_: "text-danger" },
  alter: { sign: "~", label: "changes", line: "border-accent/50 bg-accent/[0.04]", sign_: "text-accent" },
  other: { sign: "·", label: "", line: "border-line-2", sign_: "text-subtle" },
} as const;

/** One statement with a gutter sign: + creates, − drops, ~ alters (tokens only, sign carries meaning). */
function DiffStatement({ text }: { text: string }) {
  const k = MARK[diffKind(text)];
  return (
    <li className={`flex gap-3 border-l-2 px-5 py-1.5 ${k.line}`} aria-label={k.label || undefined}>
      <span className={`w-3 shrink-0 select-none ${k.sign_}`} aria-hidden>
        {k.sign}
      </span>
      <pre className="min-w-0 flex-1 overflow-x-auto whitespace-pre-wrap text-fg">{text}</pre>
    </li>
  );
}

function ResetPanel({ db, parent, busy, run }: { db: NosqlDb; parent: NosqlDb | null; busy: string | null; run: Run }) {
  const [mode, setMode] = useState<"latest" | "past">("latest");
  const [at, setAt] = useState(() => (parent ? Math.max(earliest(parent, now()), now() - 900) : now()));
  const [confirming, setConfirming] = useState(false);
  const [done, setDone] = useState<number | null>(null);
  return (
    <Panel title="Reset from parent">
      <div className="space-y-4 px-5 py-4">
        <p className="text-[13px] text-muted">
          Re-branch from {parent ? <span className="text-fg">{parent.name}</span> : "the parent"}, discarding this
          branch&apos;s changes. The id, endpoint and credentials stay, so a preview deployment needs no new config.
        </p>
        {!parent ? (
          <p className="text-[12px] text-muted">The parent was deleted; this branch can&apos;t be reset.</p>
        ) : (
          <>
            <Segmented
              value={mode}
              options={[
                { label: "latest", value: "latest" },
                { label: "a point in time", value: "past" },
              ]}
              onChange={setMode}
            />
            {mode === "past" && <PointInTime db={parent} value={at} onChange={setAt} />}
            {done != null && (
              <p className="rise text-[12px] text-muted">
                Reset to {parent.name} as of <span className="font-mono text-fg">{formatTime(done)}</span>.
              </p>
            )}
            {confirming ? (
              <Confirm
                word={db.name}
                action="Reset branch"
                busy={busy === "reset"}
                onCancel={() => setConfirming(false)}
                onConfirm={async () => {
                  const r = await run("reset", () => nosqlApi.reset(db.id, mode === "past" ? at : undefined));
                  if (r) {
                    setDone(r.parent_at ?? now());
                    setConfirming(false);
                  }
                }}
              >
                Replace <span className="text-fg">{db.name}</span>&apos;s data and schema with {parent.name}&apos;s
                {mode === "past" ? (
                  <>
                    {" "}
                    as of <span className="font-mono text-fg">{formatTime(at)}</span>
                  </>
                ) : (
                  " latest state"
                )}
                ? Connected clients reconnect.
              </Confirm>
            ) : (
              <button
                onClick={() => {
                  setDone(null);
                  setConfirming(true);
                }}
                disabled={busy !== null}
                className={btn}
              >
                <RefreshCw className="size-3.5" /> Review reset
              </button>
            )}
          </>
        )}
      </div>
    </Panel>
  );
}

function RestorePanel({ db, busy, run }: { db: NosqlDb; busy: string | null; run: Run }) {
  const [at, setAt] = useState(() => Math.max(earliest(db, now()), now() - 900));
  const [confirming, setConfirming] = useState(false);
  const [done, setDone] = useState<{ restored_to: number; archived: string } | null>(null);
  const { copied, copy } = useCopy();

  if (db.history_retention === 0) {
    return (
      <Panel title="Point-in-time restore">
        <p className="px-5 py-6 text-[13px] text-muted">
          History is off for this database. Set a point-in-time window in Overview → Settings to restore it.
        </p>
      </Panel>
    );
  }

  return (
    <Panel title="Point-in-time restore">
      <div className="space-y-4 px-5 py-4">
        <p className="text-[13px] text-muted">
          Rewind <span className="text-fg">{db.name}</span> in place to any second in its history. Clients reconnect to
          the restored state; the current state is archived first. To look before you rewind, branch at the same time.
        </p>
        <PointInTime db={db} value={at} onChange={setAt} />
        {done && (
          <div className="rise space-y-1.5 text-[13px]">
            <p>
              Restored to <span className="font-mono">{formatTime(done.restored_to)}</span>. The replaced state is archived
              as
            </p>
            <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
              <code className="min-w-0 flex-1 font-mono text-[11px] break-all">{done.archived}</code>
              <CopyButton id="archive" text={done.archived} copied={copied} onCopy={copy} />
            </div>
          </div>
        )}
        {confirming ? (
          <Confirm
            word={db.name}
            action="Restore"
            busy={busy === "restore"}
            onCancel={() => setConfirming(false)}
            onConfirm={async () => {
              const r = await run("restore", () => nosqlApi.restore(db.id, at));
              if (r) {
                setDone(r);
                setConfirming(false);
              }
            }}
          >
            Restore <span className="text-fg">{db.name}</span> to <span className="font-mono text-fg">{formatTime(at)}</span> (
            {ago(at)})? Every write after that is replaced by the restored state.
          </Confirm>
        ) : (
          <button
            onClick={() => {
              setDone(null);
              setConfirming(true);
            }}
            disabled={busy !== null || at < earliest(db, now()) || at > now()}
            className={primary}
          >
            <RotateCcw className="size-3.5" /> Review restore
          </button>
        )}
      </div>
    </Panel>
  );
}
