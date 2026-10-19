"use client";

import { Download, Square, Upload } from "lucide-react";
import { useRef, useState } from "react";
import { formatNum, type MigrateResult, qapi, type Shovel, type SkippedItem } from "@/lib/queues-api";
import { useAction, usePoll } from "./hooks";
import { QueuesShell } from "./shell";
import { btn, Empty, ErrorNote, Field, ICON, input, Panel, Pending, primary, StateDot, td, th, Toggle } from "./ui";

/**
 * Where a migration shovel is. Only a running shovel that has read the source and found it
 * empty is drained; `terminated` means it failed (it retries) and carries a reason.
 */
function shovelState(s: Shovel) {
  if (s.state === "terminated") return "failed" as const;
  if (s.state !== "running" || s.remaining == null) return "waiting" as const;
  return s.remaining === 0 ? ("drained" as const) : ("moving" as const);
}

export function Migrate() {
  const shovels = usePoll("migrate", qapi.migration, 2500);
  const { busy, error, setError, run } = useAction();
  const [url, setUrl] = useState("http://rabbitmq.internal:15672");
  const [username, setUsername] = useState("guest");
  const [password, setPassword] = useState("");
  const [vhost, setVhost] = useState("");
  const [amqpUri, setAmqpUri] = useState("");
  const [move, setMove] = useState(true);
  const [includeShovels, setIncludeShovels] = useState(false);
  const [result, setResult] = useState<MigrateResult | null>(null);
  const [stopped, setStopped] = useState<number | null>(null);

  const list = shovels.data?.shovels ?? [];
  const active = list.filter((s) => shovelState(s) !== "drained").length;
  const failed = list.filter((s) => shovelState(s) === "failed").length;

  const submit = async () => {
    setResult(null);
    setStopped(null);
    try {
      new URL(url);
    } catch {
      return setError("Enter the source's management URL, e.g. http://rabbitmq:15672");
    }
    if (amqpUri.trim() && !/^amqps?:\/\/[^/?]/.test(amqpUri.trim())) {
      return setError("The AMQP URI starts with amqp:// or amqps:// and names the source's host, e.g. amqps://rabbitmq:5671");
    }
    const out = await run("migrate", () =>
      qapi.migrate({
        source: {
          management_url: url.trim(),
          username,
          password,
          vhost: vhost.trim() || undefined,
          amqp_uri: amqpUri.trim() || undefined,
        },
        move_messages: move,
        include_shovels: includeShovels || undefined,
      }),
    );
    if (out) {
      setResult(out);
      shovels.refresh();
    }
  };

  const stop = async () => {
    if (
      !confirm(
        `Stop the migration? Its ${list.length} shovel${list.length === 1 ? " is" : "s are"} removed along with the source credentials they hold. Messages already moved stay here; anything still in the source stays there.`,
      )
    )
      return;
    const out = await run("stop", () => qapi.stopMigration());
    if (out) {
      setStopped(out.stopped.length);
      shovels.refresh();
    }
  };

  return (
    <QueuesShell
      title="Migrate from RabbitMQ"
      crumb="migrate"
      description="Copy topology, users and policies from a running RabbitMQ, then drain its messages with shovels. Clients just change their URL."
      loading={shovels.loading && !shovels.error && !shovels.offline}
      offline={shovels.offline && !shovels.data}
      error={shovels.error}
    >
      <div className="grid gap-4 lg:grid-cols-5">
        <Panel title="Source" className="lg:col-span-3">
          <form
            className="space-y-3 px-5 py-4"
            onSubmit={(e) => {
              e.preventDefault();
              submit();
            }}
          >
            <Field label="Management URL" hint="The RabbitMQ management plugin (port 15672 by default)">
              <input value={url} onChange={(e) => setUrl(e.target.value)} spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
            </Field>
            <div className="grid gap-2 sm:grid-cols-3">
              <Field label="Username">
                <input value={username} onChange={(e) => setUsername(e.target.value)} autoComplete="off" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
              </Field>
              <Field label="Password">
                <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="off" className={`${input} w-full`} />
              </Field>
              <Field label="Vhost">
                <input value={vhost} onChange={(e) => setVhost(e.target.value)} placeholder="all vhosts" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
              </Field>
            </div>
            <Field
              label="AMQP URI (optional)"
              hint="Only if the source's AMQP port isn't the one it reports (NAT, a load balancer, Docker). Credentials default to the ones above; https management URLs use amqps by default."
            >
              <input
                value={amqpUri}
                onChange={(e) => setAmqpUri(e.target.value)}
                placeholder="amqps://rabbitmq.internal:5671"
                autoComplete="off"
                spellCheck={false}
                className={`${input} w-full font-mono text-[12px]`}
              />
            </Field>
            <div className="flex flex-col gap-2">
              <Toggle checked={move} onChange={setMove} label="Move messages too (start a shovel per queue)" />
              <Toggle
                checked={includeShovels}
                onChange={setIncludeShovels}
                label="Also import the source's own shovels and federation upstreams (they start running here)"
              />
            </div>
            <ErrorNote error={error} onClose={() => setError(null)} />
            {busy === "migrate" && (
              <p className="rise text-[12px]">
                <Pending>Reading definitions from {safeHost(url)} and importing…</Pending>
              </p>
            )}
            {result && <Imported result={result} />}
            <div className="flex justify-end">
              <button type="submit" disabled={busy !== null || !url.trim() || !username} className={primary}>
                {busy === "migrate" ? <Pending>Migrating…</Pending> : "Start migration"}
              </button>
            </div>
          </form>
        </Panel>

        <Panel title="How it works" className="lg:col-span-2">
          <ol className="space-y-3 px-5 py-4 text-[13px] text-muted">
            {[
              ["Import", "Vhosts, exchanges, queues, bindings, policies and users are read from the management API and declared here."],
              ["Switch", "Point producers and consumers at the roll AMQP URL. Nothing else changes: same protocol, same client libraries."],
              [
                "Drain",
                "A shovel per queue moves what's waiting in the source and keeps draining it while producers still publish there. Stop the migration once the source is empty.",
              ],
            ].map(([t, d], i) => (
              <li key={t} className="flex gap-3">
                <span className="mt-px grid size-5 shrink-0 place-items-center rounded-full border border-line-2 font-mono text-[10px] text-fg">{i + 1}</span>
                <span>
                  <span className="text-fg">{t}.</span> {d}
                </span>
              </li>
            ))}
          </ol>
          <Definitions />
        </Panel>
      </div>

      <Panel
        title="Shovels"
        meta={
          shovels.data
            ? [active ? `${active} of ${list.length} still moving` : list.length ? `${list.length} drained` : "", failed ? `${failed} failed` : ""]
                .filter(Boolean)
                .join(" · ") || undefined
            : undefined
        }
        actions={
          list.length > 0 && (
            <button className={btn} disabled={busy !== null} onClick={stop}>
              <Square {...ICON} /> {busy === "stop" ? <Pending>Stopping…</Pending> : "Stop migration"}
            </button>
          )
        }
      >
        {stopped !== null && list.length === 0 && (
          <p className="rise border-b border-line px-5 py-2.5 text-[12px] text-muted">
            Migration stopped: {stopped} shovel{stopped === 1 ? "" : "s"} removed with the source credentials they held.
          </p>
        )}
        {list.length === 0 ? (
          <Empty>No shovels yet. They appear here once a migration moves messages.</Empty>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr>
                  <th className={th}>Shovel</th>
                  <th className={th}>Route</th>
                  <th className={`${th} w-1/4`}>Progress</th>
                  <th className={`${th} text-right`}>Moved</th>
                  <th className={`${th} text-right`}>Left</th>
                </tr>
              </thead>
              <tbody>
                {list.map((s) => {
                  const st = shovelState(s);
                  const total = s.transferred + (s.remaining ?? 0);
                  const pct = st === "drained" ? 100 : s.remaining != null && total > 0 ? (s.transferred / total) * 100 : 0;
                  return (
                    <tr key={`${s.vhost ?? ""}/${s.name}`}>
                      <td className={`${td} whitespace-nowrap`}>
                        <span className="flex items-center gap-2">
                          <StateDot state={st === "drained" ? "running" : st === "failed" ? "off" : "busy"} />
                          <span className="font-mono text-[12px]">{s.name}</span>
                          <span className={`text-[11px] ${st === "failed" ? "text-danger" : "text-subtle"}`}>
                            {st === "moving" || st === "waiting" ? <Pending>{st}</Pending> : st}
                          </span>
                        </span>
                        {st === "failed" && s.reason && (
                          <span className="mt-1 line-clamp-2 max-w-[28rem] whitespace-normal text-[11px] text-danger" title={s.reason}>
                            {s.reason} — retrying
                          </span>
                        )}
                      </td>
                      <td className={`${td} max-w-0 w-full`}>
                        <span className="block truncate font-mono text-[11px] text-muted" title={`${s.src} → ${s.dest}`}>
                          {s.src} <span className="text-subtle">→</span> {s.dest}
                        </span>
                      </td>
                      <td className={td}>
                        <div className="h-1 overflow-hidden rounded-full bg-line">
                          <div
                            className={`h-full rounded-full transition-[width] duration-500 ease-out motion-reduce:transition-none ${st === "moving" || st === "waiting" ? "bg-accent" : st === "failed" ? "bg-danger" : "bg-fg"}`}
                            style={{ width: `${pct}%` }}
                          />
                        </div>
                      </td>
                      <td className={`${td} text-right font-mono text-[12px] tabular-nums`}>{formatNum(s.transferred)}</td>
                      <td className={`${td} text-right font-mono text-[12px] tabular-nums text-muted`}>{formatNum(s.remaining)}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </QueuesShell>
  );
}

function safeHost(url: string) {
  try {
    return new URL(url).host;
  } catch {
    return "the source";
  }
}

function Imported({ result }: { result: MigrateResult }) {
  const items = Object.entries(result.imported) as [string, number][];
  return (
    <div className="rise space-y-2">
      <p className="text-[12px] text-ok">
        Imported. {result.shovels.length ? `${result.shovels.length} shovel${result.shovels.length === 1 ? "" : "s"} moving messages.` : "No messages to move."}
      </p>
      <dl className="grid grid-cols-3 gap-px overflow-hidden rounded-lg border border-line bg-line sm:grid-cols-6">
        {items.map(([k, n]) => (
          <div key={k} className="bg-panel px-3 py-2">
            <dt className="text-[10px] text-subtle">{k}</dt>
            <dd className="font-mono text-[13px] tabular-nums">{formatNum(n)}</dd>
          </div>
        ))}
      </dl>
      <Skipped items={result.skipped ?? []} />
    </div>
  );
}

/** What an import left out (or changed), and why. */
function Skipped({ items }: { items: SkippedItem[] }) {
  if (items.length === 0) return null;
  const changed = items.filter((i) => i.partial).length;
  const left = items.length - changed;
  const summary = [left ? `${left} skipped` : "", changed ? `${changed} changed` : ""].filter(Boolean).join(", ");
  return (
    <details className="rise rounded-lg border border-line" open={items.length <= 5}>
      <summary className="cursor-pointer px-3 py-2 text-[12px] text-muted select-none">
        {summary} <span className="text-subtle">— not supported by roll, or obsolete in RabbitMQ 4</span>
      </summary>
      <ul className="max-h-64 divide-y divide-line overflow-y-auto border-t border-line">
        {items.map((i, n) => (
          <li key={`${i.kind}/${i.vhost ?? ""}/${i.name}/${n}`} className="space-y-0.5 px-3 py-2 text-[12px]">
            <p className="min-w-0 truncate">
              <span className="text-subtle">
                {i.partial ? "changed" : "skipped"} {i.kind.replace(/_/g, " ")}
              </span>{" "}
              <span className="font-mono text-[11px] text-fg">{i.name || "(unnamed)"}</span>
              {i.vhost && <span className="text-subtle"> in {i.vhost}</span>}
            </p>
            <p className="text-[11px] text-muted">{i.reason}</p>
          </li>
        ))}
      </ul>
    </details>
  );
}

/** Export and import RabbitMQ definitions JSON. */
function Definitions() {
  const { busy, error, setError, run } = useAction();
  const [done, setDone] = useState<string | null>(null);
  const [skipped, setSkipped] = useState<SkippedItem[]>([]);
  const file = useRef<HTMLInputElement>(null);

  const exportDefs = async () => {
    setDone(null);
    setSkipped([]);
    const defs = await run("export", () => qapi.definitions(), { mark: "thinking" });
    if (defs === undefined) return;
    const blob = new Blob([JSON.stringify(defs, null, 2)], { type: "application/json" });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = "roll-queues-definitions.json";
    a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
    setDone("Definitions downloaded");
  };

  const importDefs = async (f: File) => {
    setDone(null);
    setSkipped([]);
    let defs: unknown;
    try {
      defs = JSON.parse(await f.text());
    } catch {
      return setError(`${f.name} isn't valid JSON`);
    }
    const out = await run("import", () => qapi.importDefinitions(defs));
    if (out !== undefined) {
      setDone(`Imported ${f.name}`);
      setSkipped(out?.skipped ?? []);
    }
  };

  return (
    <div className="mt-auto space-y-2 border-t border-line px-5 py-3">
      <p className="text-[11px] text-subtle">Definitions (RabbitMQ JSON format)</p>
      <div className="flex flex-wrap gap-1.5">
        <button className={btn} disabled={busy !== null} onClick={exportDefs}>
          <Download {...ICON} /> {busy === "export" ? <Pending>Exporting…</Pending> : "Export"}
        </button>
        <button className={btn} disabled={busy !== null} onClick={() => file.current?.click()}>
          <Upload {...ICON} /> {busy === "import" ? <Pending>Importing…</Pending> : "Import file"}
        </button>
        <input
          ref={file}
          type="file"
          accept="application/json,.json"
          className="hidden"
          onChange={(e) => {
            const f = e.target.files?.[0];
            e.target.value = "";
            if (f) importDefs(f);
          }}
        />
      </div>
      {done && <p className="text-[12px] text-ok">{done}</p>}
      <Skipped items={skipped} />
      <ErrorNote error={error} onClose={() => setError(null)} />
    </div>
  );
}
