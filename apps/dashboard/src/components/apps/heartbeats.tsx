import { Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import { appsApi, type Environment, type Heartbeat } from "@/lib/apps-api";
import { CopyButton, input, Panel, primary, useCopy } from "../ui";
import { since, SureButton, type Runner } from "./shared";

const NAME = /^[a-z0-9-]{1,32}$/;
const EVERY: [string, string][] = [
  ["300", "5 min"],
  ["3600", "1 h"],
  ["86400", "1 day"],
  ["604800", "1 week"],
];

function period(s: number) {
  return EVERY.find(([v]) => Number(v) === s)?.[1] ?? (s % 3600 === 0 ? `${s / 3600} h` : `${Math.round(s / 60)} min`);
}

/**
 * An environment's heartbeats (Healthchecks', Cronitor's): something outside roll (a backup, a
 * job on another machine) pings its URL; one that doesn't within its period and grace is
 * `heartbeat.missed` for the notification targets, the next ping resolves it. A URL is shown
 * once, when it's added.
 */
export function HeartbeatsPanel({ service, env, runner }: { service: string; env: Environment; runner: Runner }) {
  const beats = env.heartbeats ?? [];
  const [name, setName] = useState("");
  const [every, setEvery] = useState("86400");
  const [urls, setUrls] = useState<Record<string, string>>({});
  const { copied, copy } = useCopy();
  const keep = (b: Heartbeat) => ({ name: b.name, every_s: b.every_s, grace_s: b.grace_s });
  const save = (next: Pick<Heartbeat, "name" | "every_s" | "grace_s">[]) => runner.run("heartbeats", () => appsApi.putEnvironment(service, env.name, { heartbeats: next.length ? next : null }));
  const ok = NAME.test(name) && !beats.some((b) => b.name === name) && beats.length < 10;
  const busy = runner.busy !== null;
  return (
    <Panel title="Heartbeats" aside={<span className="text-[12px] text-muted">a ping that doesn't come notifies</span>}>
      {beats.length > 0 && (
        <ul className="divide-y divide-line border-b border-line">
          {beats.map((b) => {
            const st = env.heartbeat_states?.find((s) => s.name === b.name);
            const tone = st?.missed ? "bg-danger" : st?.last_ping_at ? "bg-accent" : "bg-subtle";
            const url = urls[b.name];
            return (
              <li key={b.name} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-5 py-2.5 text-[12.5px]">
                <i className={`size-1.5 rounded-full ${tone}`} aria-label={st?.missed ? "missed" : st?.last_ping_at ? "pinging" : "no ping yet"} />
                <span className="font-medium">{b.name}</span>
                <span className="min-w-0 flex-1 truncate text-[11.5px] text-subtle">
                  every {period(b.every_s)}, {Math.round((b.grace_s ?? 300) / 60)} min grace
                  {st?.last_ping_at ? ` · last ping ${since(st.last_ping_at, Date.now())}` : " · no ping yet"}
                  {st?.missed ? " · missed" : ""}
                </span>
                <SureButton className="p-1 text-subtle hover:text-fg" label={`Remove the heartbeat ${b.name}`} sure="Sure? Remove" disabled={busy} onSure={() => save(beats.filter((x) => x.name !== b.name).map(keep))}>
                  <Trash2 className="size-3.5" />
                </SureButton>
                {url && (
                  <div className="flex w-full flex-wrap items-center gap-2 text-[12px]">
                    <span className="text-muted">Ping (GET or POST) this; it isn't shown again:</span>
                    <code className="min-w-0 truncate font-mono text-fg">{url}</code>
                    <CopyButton id={`hb:${b.name}`} text={url} copied={copied} onCopy={copy} />
                  </div>
                )}
              </li>
            );
          })}
        </ul>
      )}
      <form
        className="flex flex-wrap gap-2 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          void save([...beats.map(keep), { name, every_s: Number(every) }]).then((r) => {
            if (!r) return;
            setUrls((u) => ({ ...u, ...r.heartbeat_urls }));
            setName("");
          });
        }}
      >
        <input value={name} onChange={(e) => setName(e.target.value.toLowerCase())} placeholder="nightly-backup" aria-label="Heartbeat name" spellCheck={false} className={`${input} w-44 font-mono text-[12px]`} />
        <select value={every} onChange={(e) => setEvery(e.target.value)} aria-label="Every" className={`${input} w-32 text-[12px]`}>
          {EVERY.map(([v, l]) => (
            <option key={v} value={v}>
              every {l}
            </option>
          ))}
        </select>
        <button type="submit" className={primary} disabled={!ok || busy}>
          <Plus className="size-3.5" /> Add
        </button>
      </form>
    </Panel>
  );
}
