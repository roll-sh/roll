import { BellRing, Send, Trash2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { appsApi, type NotifyDelivery, type NotifyEvent, type NotifyTarget } from "@/lib/apps-api";
import { btn, CopyButton, input, Panel, primary, useCopy } from "../ui";
import { since, type Runner } from "./shared";

const EVENTS: { value: NotifyEvent; label: string }[] = [
  { value: "deploy.failed", label: "Deploy failed" },
  { value: "crash_loop", label: "Crash loop" },
  { value: "deploy.active", label: "Deployed" },
  { value: "spend.alert", label: "Spend alert" },
  { value: "spend.limit", label: "Spend limit" },
  { value: "alert.memory", label: "Memory alert" },
  { value: "alert.errors", label: "5xx alert" },
  { value: "alert.latency", label: "Latency alert" },
  { value: "alert.log", label: "Log alert" },
  { value: "cron.failed", label: "Cron run failed" },
  { value: "job.failed", label: "Job failed" },
  { value: "domain.active", label: "Domain live" },
  { value: "domain.expiring", label: "Domain unverified" },
  { value: "domain.expired", label: "Domain removed" },
];

const KINDS: { value: NotifyTarget["kind"]; label: string; placeholder: string }[] = [
  { value: "slack", label: "Slack", placeholder: "https://hooks.slack.com/services/…" },
  { value: "discord", label: "Discord", placeholder: "https://discord.com/api/webhooks/…" },
  { value: "webhook", label: "Webhook", placeholder: "https://example.com/roll-events" },
];

/** A project's notification targets: where its services' failed deploys and crash loops go. */
export function NotificationsPanel({ project, projectName, runner }: { project: string; projectName: string; runner: Runner }) {
  const [targets, setTargets] = useState<NotifyTarget[] | null>(null);
  const [kind, setKind] = useState<NotifyTarget["kind"]>("slack");
  const [url, setUrl] = useState("");
  const [events, setEvents] = useState<NotifyEvent[]>(["deploy.failed", "crash_loop", "spend.alert", "spend.limit", "alert.memory", "alert.errors", "alert.latency", "alert.log", "cron.failed", "domain.expiring", "domain.expired"]);
  const [envs, setEnvs] = useState("");
  const [previews, setPreviews] = useState(false);
  const [secret, setSecret] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);
  const { copied, copy } = useCopy();
  const load = useCallback(() => appsApi.notifyTargets(project).then(setTargets, () => setTargets([])), [project]);
  useEffect(() => {
    void load();
  }, [load]);

  const add = async () => {
    const environments = envs.split(",").map((e) => e.trim()).filter(Boolean);
    const t = await runner.run("notification", () => appsApi.addNotifyTarget(project, { kind, url: url.trim(), events, environments, previews }));
    if (t) {
      setUrl("");
      setEnvs("");
      setSecret(t.secret ?? null);
      void load();
    }
  };

  return (
    <Panel title={`Notifications for ${projectName}`} aside={<span className="text-[12px] text-muted">every service in the project</span>}>
      {targets && targets.length > 0 && (
        <ul className="divide-y divide-line border-b border-line">
          {targets.map((t) => (
            <li key={t.id} className="px-5 py-3">
              <div className="flex flex-wrap items-center gap-2 text-[13px]">
                <BellRing className="size-3.5 text-muted" />
                <span className="font-medium">{KINDS.find((k) => k.value === t.kind)?.label}</span>
                <span className="font-mono text-[12px] text-muted">{t.host}</span>
                <span className="text-[12px] text-subtle">{t.events.map((e) => EVENTS.find((x) => x.value === e)?.label ?? e).join(", ")}</span>
                <span className="text-[12px] text-subtle">
                  · {t.environments.length ? t.environments.join(", ") : "every environment"}
                  {t.previews ? ", previews too" : ""}
                </span>
                <span className="ml-auto flex gap-1.5">
                  <button type="button" className={btn} disabled={runner.busy !== null} onClick={() => void runner.run("test", () => appsApi.testNotifyTarget(project, t.id)).then((r) => r && !r.ok && runner.setError(`The test didn't arrive: ${r.error ?? "no answer"}`))}>
                    <Send className="size-3.5" /> Test
                  </button>
                  <button type="button" className={btn} onClick={() => setOpen(open === t.id ? null : t.id)}>
                    Deliveries
                  </button>
                  <button type="button" className={btn} aria-label="Remove" disabled={runner.busy !== null} onClick={() => void runner.run("remove", () => appsApi.removeNotifyTarget(project, t.id)).then(load)}>
                    <Trash2 className="size-3.5" />
                  </button>
                </span>
              </div>
              {open === t.id && <Deliveries project={project} target={t.id} />}
            </li>
          ))}
        </ul>
      )}
      {secret && (
        <div className="flex flex-wrap items-center gap-2 border-b border-line px-5 py-3 text-[12px]">
          <span className="text-muted">Its requests are signed with this secret; it isn't shown again:</span>
          <code className="font-mono text-fg">{secret}</code>
          <CopyButton id="secret" text={secret} copied={copied} onCopy={copy} />
        </div>
      )}
      <form
        className="flex flex-wrap items-end gap-2 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          void add();
        }}
      >
        <select value={kind} onChange={(e) => setKind(e.target.value as NotifyTarget["kind"])} aria-label="Kind" className={`${input} w-28 text-[12px]`}>
          {KINDS.map((k) => (
            <option key={k.value} value={k.value}>
              {k.label}
            </option>
          ))}
        </select>
        <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder={KINDS.find((k) => k.value === kind)?.placeholder} aria-label="URL" spellCheck={false} autoComplete="off" className={`${input} min-w-0 flex-1 font-mono text-[12px]`} />
        <div className="flex gap-2 text-[12px] text-muted">
          {EVENTS.map((e) => (
            <label key={e.value} className="inline-flex items-center gap-1">
              <input type="checkbox" checked={events.includes(e.value)} onChange={(x) => setEvents((es) => (x.target.checked ? [...es, e.value] : es.filter((v) => v !== e.value)))} />
              {e.label}
            </label>
          ))}
        </div>
        <input value={envs} onChange={(e) => setEnvs(e.target.value)} placeholder="every environment" aria-label="Only these environments (comma-separated)" title="Only these environments' events (comma-separated); empty: every environment" spellCheck={false} autoComplete="off" className={`${input} w-44 font-mono text-[12px]`} />
        <label className="inline-flex items-center gap-1 text-[12px] text-muted" title="Each pull request's preview: its failed deploys and alerts too">
          <input type="checkbox" checked={previews} onChange={(e) => setPreviews(e.target.checked)} />
          Include previews
        </label>
        <button type="submit" className={primary} disabled={!url.trim() || !events.length || runner.busy !== null}>
          Add
        </button>
      </form>
    </Panel>
  );
}

function Deliveries({ project, target }: { project: string; target: string }) {
  const [list, setList] = useState<NotifyDelivery[] | null>(null);
  useEffect(() => {
    appsApi.notifyDeliveries(project, target).then(setList, () => setList([]));
  }, [project, target]);
  if (!list) return null;
  if (!list.length) return <p className="mt-2 text-[12px] text-subtle">Nothing sent yet.</p>;
  return (
    <ul className="mt-2 space-y-1 font-mono text-[11.5px]">
      {list.slice(0, 10).map((d) => (
        <li key={d.id} className="flex gap-3">
          <span className={d.status === "failed" ? "text-danger" : d.status === "pending" ? "text-accent" : "text-muted"}>{d.status}</span>
          <span className="text-fg">{d.kind}</span>
          <span className="text-subtle">{d.code ?? "—"}</span>
          <span className="min-w-0 flex-1 truncate text-subtle">{d.error ?? ""}</span>
          <span className="text-subtle">{since(d.at)}</span>
        </li>
      ))}
    </ul>
  );
}
