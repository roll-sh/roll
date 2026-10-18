"use client";

import { Lock, Search, X } from "lucide-react";
import { useMemo, useState } from "react";
import { type Connection, formatBytes, formatDuration, qapi } from "@/lib/queues-api";
import { useAction, usePoll } from "./hooks";
import { QueuesShell } from "./shell";
import { Badge, Busy, dangerIconBtn, Empty, ErrorNote, ICON, input, num, Panel, Pending, td, th } from "./ui";

export function Connections() {
  const conns = usePoll("connections", qapi.connections);
  const chans = usePoll("channels", qapi.channels);
  const { busy, error, setError, run } = useAction();
  const [q, setQ] = useState("");
  const [focus, setFocus] = useState<number | null>(null);

  const needle = q.trim().toLowerCase();
  const rows = useMemo(
    () =>
      (conns.data ?? []).filter(
        (c) =>
          !needle ||
          [c.name, c.peer, c.user, c.vhost, c.protocol, c.client ?? ""].some((s) => s.toLowerCase().includes(needle)),
      ),
    [conns.data, needle],
  );
  const channels = (chans.data ?? []).filter((c) => focus === null || c.connection === focus);

  const close = (c: Connection) => {
    if (!confirm(`Force-close ${c.name || c.peer}? The client gets CONNECTION_FORCED and its unacked messages are requeued.`)) return;
    const before = conns.data;
    conns.mutate((d) => d.filter((x) => x.id !== c.id));
    if (focus === c.id) setFocus(null);
    run(`close-${c.id}`, async () => {
      await qapi.closeConnection(c.id);
      await Promise.all([conns.refresh(), chans.refresh()]);
    }, { rollback: () => before && conns.mutate(() => before) });
  };

  return (
    <QueuesShell
      title="Connections"
      crumb="connections"
      description="Every client connection across protocols, and the channels open on them."
      loading={conns.loading && !conns.error}
      offline={conns.offline && !conns.data}
      error={conns.error ?? chans.error}
    >
      <ErrorNote error={error} onClose={() => setError(null)} />
      <div className="relative max-w-sm">
        <Search {...ICON} className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-subtle" />
        <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Filter by peer, user, vhost, client" className={`${input} w-full pl-8`} />
      </div>

      <Panel title="Connections" meta={conns.data ? `${rows.length} open` : undefined}>
        {rows.length === 0 ? (
          <Empty>{conns.data?.length ? "No connections match." : "No clients connected. Connect one with the AMQP URL from the overview."}</Empty>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr>
                  <th className={th}>Client</th>
                  <th className={th}>Protocol</th>
                  <th className={th}>User · vhost</th>
                  <th className={`${th} text-right`}>Channels</th>
                  <th className={`${th} text-right`}>In / out</th>
                  <th className={`${th} text-right`}>Open for</th>
                  <th className={th} />
                </tr>
              </thead>
              <tbody>
                {rows.map((c) => (
                  <tr
                    key={c.id}
                    onClick={() => setFocus((f) => (f === c.id ? null : c.id))}
                    className={`cursor-pointer transition ${focus === c.id ? "bg-panel-2" : "hover:bg-panel-2/60"}`}
                  >
                    <td className={`${td} max-w-0 w-full`}>
                      <p className="flex min-w-0 items-center gap-1.5">
                        {c.tls && <Lock {...ICON} className="size-3 shrink-0 text-subtle" aria-label="TLS" />}
                        <span className="truncate font-mono text-[12px]">{c.peer}</span>
                        {c.name && c.name !== c.peer && <span className="truncate text-[12px] text-subtle">{c.name}</span>}
                      </p>
                      {c.client && <p className="truncate text-[11px] text-subtle">{c.client}</p>}
                    </td>
                    <td className={td}>
                      <Badge>{c.protocol}</Badge>
                    </td>
                    <td className={`${td} font-mono text-[12px] whitespace-nowrap text-muted`}>
                      {c.user} · {c.vhost}
                    </td>
                    <td className={`${td} ${num} text-right`}>{c.channels}</td>
                    <td className={`${td} ${num} text-right whitespace-nowrap text-muted`}>
                      {formatBytes(c.recv_oct)} / {formatBytes(c.send_oct)}
                    </td>
                    <td className={`${td} ${num} text-right whitespace-nowrap text-subtle`}>
                      {formatDuration(Math.max(0, (Date.now() - c.connected_at_ms) / 1000))}
                    </td>
                    <td className={`${td} text-right`}>
                      <button
                        className={dangerIconBtn}
                        disabled={busy !== null}
                        title="Force close"
                        onClick={(e) => {
                          e.stopPropagation();
                          close(c);
                        }}
                      >
                        {busy === `close-${c.id}` ? <Busy /> : <X {...ICON} />}
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>

      <Panel
        title="Channels"
        meta={focus !== null ? `on connection ${focus}` : chans.data ? `${channels.length} open` : undefined}
        actions={
          focus !== null && (
            <button onClick={() => setFocus(null)} className="text-[12px] text-subtle hover:text-fg">
              Show all
            </button>
          )
        }
      >
        {chans.data === null ? (
          <Empty>
            <Pending>Loading channels…</Pending>
          </Empty>
        ) : channels.length === 0 ? (
          <Empty>No channels{focus !== null ? " on this connection" : ""}.</Empty>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr>
                  <th className={th}>Channel</th>
                  <th className={th}>User · vhost</th>
                  <th className={th}>Mode</th>
                  <th className={`${th} text-right`}>Prefetch</th>
                  <th className={`${th} text-right`}>Unacked</th>
                  <th className={`${th} text-right`}>Consumers</th>
                </tr>
              </thead>
              <tbody>
                {channels.map((c) => (
                  <tr key={c.id}>
                    <td className={`${td} ${num} w-full`}>
                      {c.connection} <span className="text-subtle">/</span> {c.number}
                    </td>
                    <td className={`${td} font-mono text-[12px] whitespace-nowrap text-muted`}>
                      {c.user} · {c.vhost}
                    </td>
                    <td className={`${td} whitespace-nowrap`}>
                      <span className="inline-flex gap-1">
                        {c.confirm && <Badge>confirm</Badge>}
                        {c.transactional && <Badge>tx</Badge>}
                        {!c.confirm && !c.transactional && <span className="text-[12px] text-subtle">—</span>}
                      </span>
                    </td>
                    <td className={`${td} ${num} text-right`}>{c.prefetch || "∞"}</td>
                    <td className={`${td} ${num} text-right ${c.unacked > 0 ? "" : "text-subtle"}`}>{c.unacked}</td>
                    <td className={`${td} ${num} text-right`}>{c.consumers}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </QueuesShell>
  );
}
