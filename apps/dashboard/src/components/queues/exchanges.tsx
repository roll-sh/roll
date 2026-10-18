"use client";

import { Link2, Plus, Trash2, Unlink } from "lucide-react";
import Link from "next/link";
import { useMemo, useState } from "react";
import { type Binding, type Exchange, exchangeLabel, formatNum, formatRate, qapi, queueHref } from "@/lib/queues-api";
import { ArgumentsEditor, argsFromRows, type ArgRow, EXCHANGE_ARGS } from "./args-editor";
import { useAction, usePoll } from "./hooks";
import { PublishForm } from "./publish-form";
import { useInitialVhost } from "./queue-list";
import { QueuesShell } from "./shell";
import {
  Badge,
  btn,
  dangerIconBtn,
  Empty,
  ErrorNote,
  Field,
  ICON,
  input,
  JsonInline,
  num,
  Panel,
  Pending,
  primary,
  Segmented,
  td,
  th,
  Toggle,
  VhostSelect,
} from "./ui";

const TYPES = ["direct", "fanout", "topic", "headers", "x-delayed-message"];

const bindingId = (b: Binding) =>
  `${b.source}\u0000${b.destination_type}\u0000${b.destination}\u0000${b.routing_key}\u0000${JSON.stringify(b.arguments)}`;

export function Exchanges() {
  const vhostsPoll = usePoll("vhosts", qapi.vhosts, 15_000);
  const names = useMemo(() => vhostsPoll.data?.map((v) => v.name) ?? null, [vhostsPoll.data]);
  const [picked, setPicked] = useState("");
  useInitialVhost(setPicked);
  const vhost = picked && names?.includes(picked) ? picked : (names?.[0] ?? "");
  const xs = usePoll(vhost ? `exchanges:${vhost}` : null, () => qapi.exchanges(vhost));
  const bs = usePoll(vhost ? `bindings:${vhost}` : null, () => qapi.bindings(vhost));
  const qs = usePoll(vhost ? `queues:${vhost}` : null, () => qapi.queues(vhost), 15_000);
  const { busy, error, setError, run } = useAction();
  const [declaring, setDeclaring] = useState(false);
  const [hideSystem, setHideSystem] = useState(true);
  const [focus, setFocus] = useState<string | null>(null);

  const exchanges = (xs.data ?? []).filter((x) => !hideSystem || !(x.name === "" || x.name.startsWith("amq.")));
  const bindings = (bs.data ?? []).filter((b) => b.source !== "" && (focus === null || b.source === focus));

  const removeExchange = (x: Exchange) => {
    if (!confirm(`Delete exchange ${x.name}? Its ${x.bindings} binding${x.bindings === 1 ? "" : "s"} go with it.`)) return;
    const before = xs.data;
    xs.mutate((d) => d.filter((e) => e.name !== x.name));
    run(`del-${x.name}`, async () => {
      await qapi.deleteExchange(vhost, x.name);
      await Promise.all([xs.refresh(), bs.refresh()]);
    }, { rollback: () => before && xs.mutate(() => before) });
  };

  const unbind = (b: Binding) => {
    const before = bs.data;
    bs.mutate((d) => d.filter((x) => bindingId(x) !== bindingId(b)));
    run(`unbind-${bindingId(b)}`, async () => {
      await qapi.unbind(vhost, { source: b.source, destination: b.destination, destination_type: b.destination_type, routing_key: b.routing_key, arguments: b.arguments });
      await bs.refresh();
    }, { rollback: () => before && bs.mutate(() => before) });
  };

  return (
    <QueuesShell
      title="Exchanges"
      crumb="exchanges"
      description="Exchanges route published messages to queues and other exchanges through bindings."
      loading={vhostsPoll.loading && !vhostsPoll.error}
      offline={vhostsPoll.offline && !names}
      error={vhostsPoll.error ?? xs.error ?? bs.error}
      actions={
        <button onClick={() => setDeclaring((d) => !d)} className={primary} disabled={!vhost}>
          <Plus {...ICON} /> Declare exchange
        </button>
      }
    >
      <div className="flex flex-wrap items-center gap-2">
        <VhostSelect value={vhost} vhosts={names ?? []} onChange={(v) => { setPicked(v); setFocus(null); }} />
        <Toggle checked={hideSystem} onChange={setHideSystem} label="Hide built-in (amq.*)" />
      </div>

      {declaring && vhost && (
        <DeclareExchange
          vhost={vhost}
          onCancel={() => setDeclaring(false)}
          onDeclared={() => {
            setDeclaring(false);
            xs.refresh();
          }}
        />
      )}
      <ErrorNote error={error} onClose={() => setError(null)} />

      <Panel title="Exchanges" meta={xs.data ? `${exchanges.length} shown` : undefined}>
        {xs.data === null ? (
          <Empty>
            <Pending>Loading exchanges…</Pending>
          </Empty>
        ) : exchanges.length === 0 ? (
          <Empty>No exchanges{hideSystem ? " besides the built-in ones" : ""}.</Empty>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr>
                  <th className={th}>Name</th>
                  <th className={th}>Type</th>
                  <th className={`${th} text-right`}>Bindings</th>
                  <th className={`${th} text-right`}>In</th>
                  <th className={`${th} text-right`}>Out</th>
                  <th className={th} />
                </tr>
              </thead>
              <tbody>
                {exchanges.map((x) => (
                  <tr
                    key={x.name}
                    onClick={() => setFocus((f) => (f === x.name ? null : x.name))}
                    className={`cursor-pointer transition ${focus === x.name ? "bg-panel-2" : "hover:bg-panel-2/60"}`}
                  >
                    <td className={`${td} max-w-0 w-full`}>
                      <span className="flex min-w-0 items-center gap-2">
                        <span className="truncate font-mono text-[12px]">{exchangeLabel(x.name)}</span>
                        {!x.durable && <Badge>transient</Badge>}
                        {x.auto_delete && <Badge>auto-delete</Badge>}
                        {x.internal && <Badge>internal</Badge>}
                        {Object.keys(x.arguments ?? {}).length > 0 && <JsonInline value={x.arguments} />}
                      </span>
                    </td>
                    <td className={`${td} font-mono text-[12px] text-muted`}>{x.type}</td>
                    <td className={`${td} ${num} text-right`}>{formatNum(x.bindings)}</td>
                    <td className={`${td} ${num} text-right text-muted`}>{formatRate(x.rates.publish_in)}</td>
                    <td className={`${td} ${num} text-right text-muted`}>{formatRate(x.rates.publish_out)}</td>
                    <td className={`${td} text-right`}>
                      {x.name !== "" && !x.name.startsWith("amq.") && (
                        <button
                          className={dangerIconBtn}
                          disabled={busy !== null}
                          title="Delete exchange"
                          onClick={(e) => {
                            e.stopPropagation();
                            removeExchange(x);
                          }}
                        >
                          <Trash2 {...ICON} />
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>

      <div className="grid gap-4 lg:grid-cols-5">
        <Panel
          title="Bindings"
          meta={focus !== null ? <span className="font-mono">from {exchangeLabel(focus)}</span> : bs.data ? `${bindings.length}` : undefined}
          actions={
            focus !== null && (
              <button onClick={() => setFocus(null)} className="text-[12px] text-subtle hover:text-fg">
                Show all
              </button>
            )
          }
          className="lg:col-span-3"
        >
          {bs.data === null ? (
            <Empty>
              <Pending>Loading bindings…</Pending>
            </Empty>
          ) : bindings.length === 0 ? (
            <Empty>No bindings{focus !== null ? " from this exchange" : ""}.</Empty>
          ) : (
            <ul>
              {bindings.map((b) => (
                <li key={bindingId(b)} className="flex items-center gap-3 border-b border-line px-5 py-2.5 last:border-0">
                  <span className="min-w-0 flex-1 truncate font-mono text-[12px]">
                    {b.source}
                    <span className="text-subtle"> —{b.routing_key ? ` ${b.routing_key} ` : " "}→ </span>
                    {b.destination_type === "queue" ? (
                      <Link href={queueHref(vhost, b.destination)} className="hover:underline">
                        {b.destination}
                      </Link>
                    ) : (
                      <span>
                        {b.destination} <span className="text-subtle">(exchange)</span>
                      </span>
                    )}
                  </span>
                  {Object.keys(b.arguments ?? {}).length > 0 && <JsonInline value={b.arguments} />}
                  <button className={dangerIconBtn} disabled={busy !== null} onClick={() => unbind(b)} title="Unbind">
                    <Unlink {...ICON} />
                  </button>
                </li>
              ))}
            </ul>
          )}
          {xs.data && (
            <BindForm
              key={vhost}
              vhost={vhost}
              source={focus}
              exchanges={xs.data.map((x) => x.name).filter((n) => n !== "")}
              queues={qs.data?.map((q) => q.name) ?? []}
              onBound={() => {
                bs.refresh();
                xs.refresh();
              }}
            />
          )}
        </Panel>

        <Panel title="Publish" className="lg:col-span-2">
          {xs.data ? (
            <PublishForm key={`${vhost}:${focus ?? ""}`} vhost={vhost} exchanges={orderFor(xs.data.map((x) => x.name), focus)} />
          ) : (
            <Empty>
              <Pending>Loading…</Pending>
            </Empty>
          )}
        </Panel>
      </div>
    </QueuesShell>
  );
}

/** Put the focused exchange first so the publish form starts on it. */
function orderFor(names: string[], focus: string | null) {
  if (focus === null || !names.includes(focus)) return names;
  return [focus, ...names.filter((n) => n !== focus)];
}

function BindForm({
  vhost,
  source: focused,
  exchanges,
  queues,
  onBound,
}: {
  vhost: string;
  source: string | null;
  exchanges: string[];
  queues: string[];
  onBound: () => void;
}) {
  const { busy, error, setError, run } = useAction();
  const [source, setSource] = useState("");
  const [destType, setDestType] = useState<"queue" | "exchange">("queue");
  const [dest, setDest] = useState("");
  const [key, setKey] = useState("");
  const [args, setArgs] = useState<ArgRow[]>([]);
  const [showArgs, setShowArgs] = useState(false);
  const src = source || focused || exchanges.find((x) => !x.startsWith("amq.")) || exchanges[0] || "";
  const targets = destType === "queue" ? queues : exchanges;

  return (
    <form
      className="mt-auto space-y-2 border-t border-line px-5 py-3"
      onSubmit={async (e) => {
        e.preventDefault();
        const { args: table, error: argError } = argsFromRows(args);
        if (argError) return setError(argError);
        if (!src || !dest.trim()) return;
        const out = await run("bind", () =>
          qapi.bind(vhost, { source: src, destination: dest.trim(), destination_type: destType, routing_key: key, arguments: table }),
        );
        if (out !== undefined) {
          setKey("");
          onBound();
        }
      }}
    >
      <div className="flex flex-wrap items-center gap-2">
        <Link2 {...ICON} className="size-3.5 text-subtle" />
        <select value={src} onChange={(e) => setSource(e.target.value)} className={`${input} min-w-0 flex-1 font-mono text-[12px]`} aria-label="Source exchange">
          {exchanges.map((x) => (
            <option key={x} value={x}>
              {x}
            </option>
          ))}
        </select>
        <span className="text-[12px] text-subtle">→</span>
        <Segmented value={destType} options={["queue", "exchange"] as const} onChange={(v) => { setDestType(v); setDest(""); }} />
        <input
          list={`bind-targets-${destType}`}
          value={dest}
          onChange={(e) => setDest(e.target.value)}
          placeholder={destType === "queue" ? "queue" : "exchange"}
          spellCheck={false}
          className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
        />
        <datalist id={`bind-targets-${destType}`}>
          {targets.map((t) => (
            <option key={t} value={t} />
          ))}
        </datalist>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <input value={key} onChange={(e) => setKey(e.target.value)} placeholder="routing key (orders.* for topic)" spellCheck={false} className={`${input} min-w-0 flex-1 font-mono text-[12px]`} />
        <button type="button" onClick={() => setShowArgs((s) => !s)} className="text-[12px] text-subtle hover:text-fg">
          {showArgs ? "Hide arguments" : "Arguments"}
        </button>
        <button type="submit" disabled={busy !== null || !src || !dest.trim()} className={btn}>
          {busy ? <Pending>Binding…</Pending> : "Bind"}
        </button>
      </div>
      {showArgs && (
        <div className="rise">
          <ArgumentsEditor rows={args} onChange={setArgs} label="Binding arguments (x-match, headers)" suggestions={[{ key: "x-match", type: "string", value: "all", title: "headers exchange: all | any" }]} />
        </div>
      )}
      <ErrorNote error={error} onClose={() => setError(null)} />
    </form>
  );
}

function DeclareExchange({ vhost, onCancel, onDeclared }: { vhost: string; onCancel: () => void; onDeclared: () => void }) {
  const { busy, error, setError, run } = useAction();
  const [name, setName] = useState("");
  const [type, setType] = useState("topic");
  const [durable, setDurable] = useState(true);
  const [autoDelete, setAutoDelete] = useState(false);
  const [internal, setInternal] = useState(false);
  const [args, setArgs] = useState<ArgRow[]>([]);

  return (
    <form
      className="rise space-y-4 rounded-2xl border border-line bg-panel p-4"
      onSubmit={async (e) => {
        e.preventDefault();
        const { args: table, error: argError } = argsFromRows(args);
        if (argError) return setError(argError);
        if (!name.trim()) return;
        const out = await run("declare", () =>
          qapi.declareExchange(vhost, { name: name.trim(), type, durable, auto_delete: autoDelete, internal, arguments: table }),
        );
        if (out !== undefined) onDeclared();
      }}
    >
      <div className="grid gap-3 sm:grid-cols-[1fr_auto]">
        <Field label={`Name · vhost ${vhost}`}>
          <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="events" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
        </Field>
        <Field label="Type">
          <select value={type} onChange={(e) => setType(e.target.value)} className={`${input} w-full font-mono text-[12px]`}>
            {TYPES.map((t) => (
              <option key={t} value={t}>
                {t}
              </option>
            ))}
          </select>
        </Field>
      </div>
      <div className="flex flex-wrap gap-4">
        <Toggle checked={durable} onChange={setDurable} label="Durable" />
        <Toggle checked={autoDelete} onChange={setAutoDelete} label="Auto-delete" />
        <Toggle checked={internal} onChange={setInternal} label="Internal" />
      </div>
      <ArgumentsEditor
        rows={args}
        onChange={setArgs}
        suggestions={EXCHANGE_ARGS.filter((a) => a.key !== "x-delayed-type" || type === "x-delayed-message")}
      />
      <ErrorNote error={error} onClose={() => setError(null)} />
      <div className="flex justify-end gap-2">
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
        <button type="submit" disabled={busy !== null || !name.trim()} className={primary}>
          {busy ? <Pending>Declaring…</Pending> : "Declare"}
        </button>
      </div>
    </form>
  );
}
