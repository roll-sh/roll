"use client";

import { HorizonMark } from "@roll/brand";
import { KeyRound, Moon, Plus, Power, Trash2, X } from "lucide-react";
import Link from "next/link";
import { useState } from "react";
import { formatBytes, formatNum, formatRate, qapi, type VHost, type VHostCredentials } from "@/lib/queues-api";
import { useAction, usePoll } from "./hooks";
import { QueuesShell } from "./shell";
import { Busy, CopyButton, CopyField, dangerIconBtn, Empty, ErrorNote, Field, ICON, iconBtn, input, Panel, Pending, primary, btn } from "./ui";

export function Vhosts() {
  const poll = usePoll("vhosts", qapi.vhosts);
  const { busy, error, setError, run } = useAction();
  const [creating, setCreating] = useState(false);
  const [issued, setIssued] = useState<{ vhost: string; credentials: VHostCredentials } | null>(null);
  const vhosts = poll.data;

  const rotate = (v: VHost) => {
    if (!confirm(`Issue new credentials for ${v.name}? Apps using its current password will need the new URL.`)) return;
    run(`creds-${v.name}`, async () => {
      setIssued({ vhost: v.name, credentials: await qapi.vhostCredentials(v.name) });
    });
  };

  const toggle = (v: VHost) => {
    const sleeping = v.state === "sleeping";
    const next: VHost["state"] = sleeping ? "running" : "sleeping";
    poll.mutate((vs) => vs.map((x) => (x.name === v.name ? { ...x, state: next } : x)));
    return run(
      `state-${v.name}`,
      async () => {
        await (sleeping ? qapi.wakeVhost(v.name) : qapi.suspendVhost(v.name));
        await poll.refresh();
      },
      {
        done: sleeping ? "success" : "sleep",
        rollback: () => poll.mutate((vs) => vs.map((x) => (x.name === v.name ? { ...x, state: v.state } : x))),
      },
    );
  };

  const remove = (v: VHost) => {
    if (!confirm(`Delete vhost ${v.name}? Its queues, exchanges and messages are deleted too.`)) return;
    const snapshot = vhosts;
    poll.mutate((vs) => vs.filter((x) => x.name !== v.name));
    run(`delete-${v.name}`, async () => {
      await qapi.deleteVhost(v.name);
      await poll.refresh();
    }, { rollback: () => snapshot && poll.mutate(() => snapshot) });
  };

  const awake = vhosts?.filter((v) => v.state === "running").length ?? 0;

  return (
    <QueuesShell
      title="Vhosts"
      crumb="vhosts"
      description="Isolated namespaces with their own queues, exchanges and permissions. Idle vhosts scale to zero."
      loading={poll.loading && !poll.error}
      offline={poll.offline && !vhosts}
      error={poll.error}
      actions={
        <button onClick={() => setCreating((c) => !c)} className={primary}>
          <Plus {...ICON} /> New vhost
        </button>
      }
    >
      {creating && (
        <CreateVhost
          onCancel={() => setCreating(false)}
          onCreated={(v) => {
            setCreating(false);
            if (v.credentials) setIssued({ vhost: v.name, credentials: v.credentials });
            poll.refresh();
          }}
        />
      )}
      {issued && <Credentials vhost={issued.vhost} credentials={issued.credentials} onClose={() => setIssued(null)} />}
      <ErrorNote error={error} onClose={() => setError(null)} />
      <Panel title="All vhosts" meta={vhosts && `${vhosts.length} · ${awake} awake`}>
        {vhosts && vhosts.length === 0 ? (
          <Empty>No vhosts yet.</Empty>
        ) : (
          <ul>
            {(vhosts ?? []).map((v) => {
              const pending = busy === `state-${v.name}`;
              const sleeping = v.state === "sleeping";
              return (
                <li key={v.name} className="rise flex flex-wrap items-center gap-x-4 gap-y-2 border-b border-line px-5 py-3.5 last:border-0">
                  <span className={sleeping ? "text-muted" : "text-fg"} title={sleeping ? "sleeping" : "running"}>
                    <HorizonMark state={pending ? "deploying" : sleeping ? "sleep" : "idle"} size={20} interactive={false} label={`${v.name} ${v.state}`} />
                  </span>
                  <div className="min-w-0 flex-1">
                    <p className="flex items-center gap-2 text-[13px]">
                      <Link href={`/dashboard/queues/queues?vhost=${encodeURIComponent(v.name)}`} className="truncate font-mono hover:underline">
                        {v.name}
                      </Link>
                      <span className={`text-[11px] ${sleeping ? "text-subtle" : "text-ok"}`}>
                        {pending ? <Pending>{sleeping ? "waking" : "suspending"}</Pending> : sleeping ? "sleeping" : "running"}
                      </span>
                    </p>
                    <p className="truncate text-[12px] text-subtle">{v.description || `default queue type ${v.default_queue_type}`}</p>
                  </div>
                  <dl className="hidden grid-cols-4 gap-6 font-mono text-[11px] md:grid">
                    <Metric label="queues" value={formatNum(v.queues)} sub={sleeping ? "last known" : undefined} />
                    <Metric label="ready" value={formatNum(v.messages_ready)} sub={`${formatNum(v.messages_unacked)} unacked`} />
                    <Metric label="in / out" value={formatRate(v.rates.publish)} sub={formatRate(v.rates.deliver)} />
                    <Metric label="stored" value={formatBytes(v.stored_bytes)} sub={`${v.connections} conn`} />
                  </dl>
                  <div className="flex items-center gap-0.5">
                    <CopyButton text={v.amqp_url} label="Copy AMQP URL" />
                    <button className={iconBtn} disabled={busy !== null} title="New credentials" onClick={() => rotate(v)}>
                      {busy === `creds-${v.name}` ? <Busy /> : <KeyRound {...ICON} />}
                    </button>
                    <button
                      className={iconBtn}
                      disabled={busy !== null}
                      title={sleeping ? "Wake now" : "Suspend (scale to zero)"}
                      onClick={() => toggle(v)}
                    >
                      {pending ? <Busy /> : sleeping ? <Power {...ICON} /> : <Moon {...ICON} />}
                    </button>
                    <button className={dangerIconBtn} disabled={busy !== null} title="Delete vhost" onClick={() => remove(v)}>
                      <Trash2 {...ICON} />
                    </button>
                  </div>
                </li>
              );
            })}
          </ul>
        )}
      </Panel>
      <p className="text-[12px] text-subtle">
        A sleeping vhost keeps its messages in the log and costs only storage. The first connection wakes it.
      </p>
    </QueuesShell>
  );
}

function Metric({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <div className="w-20">
      <dt className="text-[10px] text-subtle">{label}</dt>
      <dd className="tabular-nums">{value}</dd>
      {sub && <dd className="text-subtle tabular-nums">{sub}</dd>}
    </div>
  );
}

const PROTOCOLS: [keyof VHostCredentials["urls"], string][] = [
  ["amqp", "AMQP"],
  ["amqps", "AMQPS"],
  ["stream", "Streams"],
  ["mqtt", "MQTT"],
  ["stomp", "STOMP"],
  ["http", "HTTP"],
  ["management", "Management"],
];

/** Connection URLs for a vhost's own user, shown once right after they are issued. */
function Credentials({ vhost, credentials, onClose }: { vhost: string; credentials: VHostCredentials; onClose: () => void }) {
  return (
    <Panel
      className="rise"
      title={<span className="font-mono">{vhost}</span>}
      meta="copy these now; the password is not shown again"
      actions={
        <button onClick={onClose} className={iconBtn} title="Done" aria-label="Done">
          <X {...ICON} />
        </button>
      }
    >
      <dl className="grid gap-2 px-5 py-4 text-[12px] sm:grid-cols-[7rem_1fr] sm:items-center">
        <dt className="text-subtle">User</dt>
        <dd><CopyField value={credentials.user} /></dd>
        <dt className="text-subtle">Password</dt>
        <dd><CopyField value={credentials.password} /></dd>
        {PROTOCOLS.map(([key, label]) => {
          const url = credentials.urls[key];
          if (!url) return null;
          return [
            <dt key={`${key}-l`} className="text-subtle">{label}</dt>,
            <dd key={`${key}-v`}><CopyField value={url} /></dd>,
          ];
        })}
      </dl>
    </Panel>
  );
}

function CreateVhost({ onCancel, onCreated }: { onCancel: () => void; onCreated: (v: VHost) => void }) {
  const { busy, error, setError, run } = useAction();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [type, setType] = useState("classic");
  const [credentials, setCredentials] = useState(true);
  return (
    <form
      className="rise space-y-3 rounded-2xl border border-line bg-panel p-4"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!name.trim()) return;
        const out = await run("create", () =>
          qapi.createVhost({ name: name.trim(), description: description.trim() || undefined, default_queue_type: type, credentials }),
        );
        if (out !== undefined) onCreated(out);
      }}
    >
      <div className="grid gap-2 sm:grid-cols-[1fr_1.5fr_auto]">
        <Field label="Name">
          <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="production" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
        </Field>
        <Field label="Description">
          <input value={description} onChange={(e) => setDescription(e.target.value)} placeholder="Optional" className={`${input} w-full`} />
        </Field>
        <Field label="Default queue type">
          <select value={type} onChange={(e) => setType(e.target.value)} className={`${input} w-full`}>
            <option value="classic">classic</option>
            <option value="quorum">quorum</option>
            <option value="stream">stream</option>
          </select>
        </Field>
      </div>
      <label className="flex items-center gap-2 text-[13px] text-muted">
        <input type="checkbox" checked={credentials} onChange={(e) => setCredentials(e.target.checked)} className="accent-accent" />
        Create a user for just this vhost and show its connection URLs
      </label>
      <ErrorNote error={error} onClose={() => setError(null)} />
      <div className="flex justify-end gap-2">
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
        <button type="submit" disabled={busy !== null || !name.trim()} className={primary}>
          {busy ? <Pending>Creating…</Pending> : "Create vhost"}
        </button>
      </div>
    </form>
  );
}
