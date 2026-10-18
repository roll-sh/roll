"use client";

import { ArrowDown, ArrowUp, Plus, Search } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useEffect, useMemo, useState } from "react";
import { formatBytes, formatNum, formatRate, qapi, type Queue, queueHref, type QueueType } from "@/lib/queues-api";
import { ArgumentsEditor, argsFromRows, type ArgRow, QUEUE_ARGS } from "./args-editor";
import { useAction, usePoll } from "./hooks";
import { QueuesShell } from "./shell";
import { Badge, btn, Empty, ErrorNote, Field, ICON, input, num, Panel, Pending, primary, Segmented, td, th, Toggle, VhostSelect } from "./ui";

type SortKey = "name" | "messages_ready" | "messages_unacked" | "consumers" | "publish" | "bytes";

const TYPES = ["all", "classic", "quorum", "stream"] as const;

/** Read `?vhost=` once on mount without opting the page into request-time rendering. */
export function useInitialVhost(set: (v: string) => void) {
  useEffect(() => {
    const v = new URLSearchParams(window.location.search).get("vhost");
    if (v !== null) set(v);
  }, [set]);
}

export function QueueList() {
  const vhostsPoll = usePoll("vhosts", qapi.vhosts, 15_000);
  const names = useMemo(() => vhostsPoll.data?.map((v) => v.name) ?? null, [vhostsPoll.data]);
  const [vhost, setVhost] = useState("");
  useInitialVhost(setVhost);
  const scope = vhost ? [vhost] : names;
  const key = scope ? `queues:${scope.join("\u0000")}` : null;
  const poll = usePoll(key, async () => (await Promise.all((scope ?? []).map((v) => qapi.queues(v)))).flat());

  const [q, setQ] = useState("");
  const [type, setType] = useState<(typeof TYPES)[number]>("all");
  const [dlqOnly, setDlqOnly] = useState(false);
  const [nonEmpty, setNonEmpty] = useState(false);
  const [sort, setSort] = useState<{ key: SortKey; desc: boolean }>({ key: "name", desc: false });
  const [declaring, setDeclaring] = useState(false);

  const rows = useMemo(() => {
    const needle = q.trim().toLowerCase();
    const list = (poll.data ?? []).filter(
      (x) =>
        (!needle || x.name.toLowerCase().includes(needle)) &&
        (type === "all" || x.type === type) &&
        (!dlqOnly || x.is_dlq) &&
        (!nonEmpty || x.messages > 0),
    );
    const val = (x: Queue) =>
      sort.key === "name" ? `${x.vhost}\u0000${x.name}` : sort.key === "publish" ? x.rates.publish : x[sort.key];
    return list.sort((a, b) => {
      const av = val(a);
      const bv = val(b);
      const c = typeof av === "string" ? av.localeCompare(bv as string) : (av as number) - (bv as number);
      return sort.desc ? -c : c;
    });
  }, [poll.data, q, type, dlqOnly, nonEmpty, sort]);

  const total = poll.data?.length ?? 0;
  const showVhost = !vhost && (names?.length ?? 0) > 1;

  const header = (label: string, k: SortKey, right = true) => (
    <th className={`${th} ${right ? "text-right" : ""}`}>
      <button
        onClick={() => setSort((s) => ({ key: k, desc: s.key === k ? !s.desc : k !== "name" }))}
        className={`inline-flex items-center gap-1 hover:text-fg ${sort.key === k ? "text-fg" : ""}`}
      >
        {label}
        {sort.key === k && (sort.desc ? <ArrowDown className="size-3" strokeWidth={1.5} /> : <ArrowUp className="size-3" strokeWidth={1.5} />)}
      </button>
    </th>
  );

  return (
    <QueuesShell
      title="Queues"
      crumb="queues"
      description="Classic, quorum and stream queues across your vhosts."
      loading={vhostsPoll.loading && !vhostsPoll.error}
      offline={vhostsPoll.offline && !names}
      error={vhostsPoll.error ?? poll.error}
      actions={
        <button onClick={() => setDeclaring((d) => !d)} className={primary}>
          <Plus {...ICON} /> Declare queue
        </button>
      }
    >
      {declaring && names && (
        <DeclareQueue
          vhosts={names}
          initialVhost={vhost || names[0] || "/"}
          defaultType={(vhostsPoll.data?.find((v) => v.name === (vhost || names[0]))?.default_queue_type as QueueType) ?? "classic"}
          onCancel={() => setDeclaring(false)}
          onDeclared={() => {
            setDeclaring(false);
            poll.refresh();
          }}
        />
      )}

      <div className="flex flex-wrap items-center gap-2">
        <div className="relative min-w-48 flex-1">
          <Search {...ICON} className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-subtle" />
          <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Filter by name" className={`${input} w-full pl-8`} />
        </div>
        <VhostSelect value={vhost} vhosts={names ?? []} onChange={setVhost} all="All vhosts" />
        <Segmented value={type} options={TYPES} onChange={setType} />
        <Toggle checked={dlqOnly} onChange={setDlqOnly} label="Dead-letter queues" />
        <Toggle checked={nonEmpty} onChange={setNonEmpty} label="Has messages" />
      </div>

      <Panel>
        {poll.data === null ? (
          <Empty>{poll.error ? "Couldn't load queues." : <Pending>Loading queues…</Pending>}</Empty>
        ) : rows.length === 0 ? (
          <Empty>{total === 0 ? "No queues yet. Declare one, or connect a client and let it declare its own." : "No queues match these filters."}</Empty>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr>
                  {header("Name", "name", false)}
                  {header("Ready", "messages_ready")}
                  {header("Unacked", "messages_unacked")}
                  {header("Consumers", "consumers")}
                  {header("In", "publish")}
                  <th className={`${th} text-right`}>Out</th>
                  {header("Size", "bytes")}
                </tr>
              </thead>
              <tbody>
                {rows.map((x) => (
                  <tr key={`${x.vhost}\u0000${x.name}`} className="transition hover:bg-panel-2/60">
                    <td className={`${td} max-w-0 w-full`}>
                      <div className="flex min-w-0 items-center gap-2">
                        <Link href={queueHref(x.vhost, x.name)} className="truncate font-mono text-[12px] hover:underline">
                          {x.name}
                        </Link>
                        <QueueBadges q={x} />
                      </div>
                      {showVhost && <p className="truncate font-mono text-[11px] text-subtle">vhost {x.vhost}</p>}
                    </td>
                    <td className={`${td} ${num} text-right`}>{formatNum(x.messages_ready)}</td>
                    <td className={`${td} ${num} text-right text-muted`}>{formatNum(x.messages_unacked)}</td>
                    <td className={`${td} ${num} text-right text-muted`}>{x.consumers}</td>
                    <td className={`${td} ${num} text-right text-muted`}>{formatRate(x.rates.publish)}</td>
                    <td className={`${td} ${num} text-right text-muted`}>{formatRate(x.rates.deliver)}</td>
                    <td className={`${td} ${num} text-right whitespace-nowrap text-subtle`}>{formatBytes(x.bytes)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
      {poll.data && (
        <p className="font-mono text-[11px] text-subtle">
          {rows.length} of {total} queues · refreshes every 5s
        </p>
      )}
    </QueuesShell>
  );
}

export function QueueBadges({ q }: { q: Queue }) {
  return (
    <>
      {q.type !== "classic" && <Badge>{q.type}</Badge>}
      {q.is_dlq && <Badge tone="danger" title="Something dead-letters into this queue">DLQ</Badge>}
      {q.policy && <Badge title="Policy">{q.policy}</Badge>}
      {q.exclusive && <Badge>exclusive</Badge>}
      {q.auto_delete && <Badge>auto-delete</Badge>}
      {!q.durable && <Badge>transient</Badge>}
    </>
  );
}

function DeclareQueue({
  vhosts,
  initialVhost,
  defaultType,
  onCancel,
  onDeclared,
}: {
  vhosts: string[];
  initialVhost: string;
  defaultType: QueueType;
  onCancel: () => void;
  onDeclared: () => void;
}) {
  const router = useRouter();
  const { busy, error, setError, run } = useAction();
  const [vhost, setVhost] = useState(initialVhost);
  const [name, setName] = useState("");
  const [type, setType] = useState<QueueType>(defaultType);
  const [durable, setDurable] = useState(true);
  const [autoDelete, setAutoDelete] = useState(false);
  const [args, setArgs] = useState<ArgRow[]>([]);
  const replicated = type !== "classic";

  const submit = async (open: boolean) => {
    const { args: table, error: argError } = argsFromRows(args);
    if (argError) return setError(argError);
    const out = await run("declare", () =>
      qapi.declareQueue(vhost, {
        name: name.trim(),
        type,
        durable: replicated ? true : durable,
        auto_delete: replicated ? false : autoDelete,
        arguments: table,
      }),
    );
    if (out === undefined) return;
    if (open) router.push(queueHref(vhost, out?.name ?? name.trim()));
    else onDeclared();
  };

  return (
    <form
      className="rise space-y-4 rounded-2xl border border-line bg-panel p-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (name.trim()) submit(false);
      }}
    >
      <div className="grid gap-3 sm:grid-cols-[auto_1fr_auto]">
        <Field label="Vhost">
          <VhostSelect value={vhost} vhosts={vhosts} onChange={setVhost} className="w-full" />
        </Field>
        <Field label="Name">
          <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="orders" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
        </Field>
        <Field label="Type">
          <Segmented value={type} options={["classic", "quorum", "stream"] as const} onChange={setType} />
        </Field>
      </div>
      <div className="flex flex-wrap items-center gap-4">
        <Toggle checked={replicated || durable} onChange={setDurable} disabled={replicated} label="Durable" />
        <Toggle checked={!replicated && autoDelete} onChange={setAutoDelete} disabled={replicated} label="Auto-delete" />
        <span className="text-[11px] text-subtle">
          {type === "quorum"
            ? "Replicated to a keeper quorum in three AZs. Delivery limit 20 by default."
            : type === "stream"
              ? "Append-only log read by offset. Messages stay after they are consumed."
              : "Single-partition FIFO queue. Persistent messages are written to the replicated log."}
        </span>
      </div>
      <ArgumentsEditor rows={args} onChange={setArgs} suggestions={QUEUE_ARGS.filter((a) => !a.for || a.for.includes(type))} />
      <ErrorNote error={error} onClose={() => setError(null)} />
      <div className="flex justify-end gap-2">
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
        <button type="button" disabled={busy !== null || !name.trim()} onClick={() => submit(true)} className={btn}>
          Declare and open
        </button>
        <button type="submit" disabled={busy !== null || !name.trim()} className={primary}>
          {busy ? <Pending>Declaring…</Pending> : "Declare"}
        </button>
      </div>
    </form>
  );
}
