import { BellRing, Send, Trash2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { appsApi, type NotifyDelivery, type NotifyEvent, type NotifyTarget } from "@/lib/apps-api";
import { btn, CopyButton, input, Panel, primary, useCopy } from "../ui";
import { since, SureButton, type Runner } from "./shared";

const EVENTS: { value: NotifyEvent; label: string }[] = [
  { value: "deploy.failed", label: "Deploy failed" },
  { value: "rollout.waiting", label: "Rollout waits for you" },
  { value: "deploy.base_updated", label: "Base image updated" },
  { value: "deploy.ready", label: "Built, waits to go live" },
  { value: "crash_loop", label: "Crash loop" },
  { value: "deploy.active", label: "Deployed" },
  { value: "spend.alert", label: "Spend alert" },
  { value: "spend.limit", label: "Spend limit" },
  { value: "alert.memory", label: "Memory alert" },
  { value: "alert.errors", label: "5xx alert" },
  { value: "alert.latency", label: "Latency alert" },
  { value: "alert.capacity", label: "At capacity" },
  { value: "alert.slo", label: "Error budget burning" },
  { value: "alert.log", label: "Log alert" },
  { value: "error.new", label: "New error" },
  { value: "error.regressed", label: "A resolved error came back" },
  { value: "cron.failed", label: "Cron run failed" },
  { value: "job.failed", label: "Job failed" },
  { value: "domain.active", label: "Domain live" },
  { value: "domain.expiring", label: "Domain unverified" },
  { value: "domain.expired", label: "Domain removed" },
  { value: "domain.tls_failed", label: "Certificate failed" },
  { value: "volume.backup_failed", label: "Backup failed" },
  { value: "volume.full", label: "Volume 90% full" },
  { value: "check.down", label: "Uptime check down" },
  { value: "heartbeat.missed", label: "Heartbeat missed" },
  { value: "deploy.vulnerable", label: "New known vulnerabilities" },
  { value: "audit", label: "Every change people make (audit log)" },
];

const KINDS: { value: NotifyTarget["kind"]; label: string; placeholder: string }[] = [
  { value: "slack", label: "Slack", placeholder: "https://hooks.slack.com/services/…" },
  { value: "discord", label: "Discord", placeholder: "https://discord.com/api/webhooks/…" },
  { value: "teams", label: "Microsoft Teams", placeholder: "A Workflows webhook URL (…logic.azure.com/…)" },
  { value: "googlechat", label: "Google Chat", placeholder: "https://chat.googleapis.com/v1/spaces/…" },
  { value: "webhook", label: "Webhook", placeholder: "https://example.com/roll-events" },
  { value: "email", label: "Email", placeholder: "" },
  // On-call: each failure an incident, closed when its alert resolves or the next deploy works.
  { value: "pagerduty", label: "PagerDuty", placeholder: "Integration key (Events API v2)" },
  { value: "opsgenie", label: "Opsgenie", placeholder: "API key (an API integration's)" },
];

/** A project's notification targets: where its services' failed deploys and crash loops go. */
export function NotificationsPanel({ project, projectName, runner }: { project: string; projectName: string; runner: Runner }) {
  const [targets, setTargets] = useState<NotifyTarget[] | null>(null);
  const [kind, setKind] = useState<NotifyTarget["kind"]>("slack");
  const [url, setUrl] = useState("");
  const [region, setRegion] = useState<"us" | "eu">("us");
  const oncall = kind === "pagerduty" || kind === "opsgenie";
  const [events, setEvents] = useState<NotifyEvent[]>(["check.down", "heartbeat.missed", "alert.slo", "deploy.failed", "rollout.waiting", "deploy.ready", "error.new", "error.regressed", "crash_loop", "spend.alert", "spend.limit", "alert.memory", "alert.errors", "alert.latency", "alert.capacity", "alert.log", "cron.failed", "domain.expiring", "domain.expired", "domain.tls_failed", "volume.backup_failed", "volume.full"]);
  const [envs, setEnvs] = useState("");
  const [previews, setPreviews] = useState(false);
  // Only some of the project's services (none chosen: every one).
  const [all, setAll] = useState<{ id: string; name: string }[]>([]);
  const [only, setOnly] = useState<string[]>([]);
  useEffect(() => {
    appsApi.services(project).then((s) => setAll(s.map((x) => ({ id: x.id, name: x.name }))), () => setAll([]));
  }, [project]);
  const [secret, setSecret] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);
  // The target whose events are being changed (in place: its URL and secret stay).
  const [editing, setEditing] = useState<string | null>(null);
  const { copied, copy } = useCopy();
  // A read that failed says so (an empty list would read as "nothing set").
  const [failed, setFailed] = useState<string | null>(null);
  const load = useCallback(
    () =>
      appsApi.notifyTargets(project).then(
        (t) => {
          setTargets(t);
          setFailed(null);
        },
        (e: unknown) => setFailed(e instanceof Error ? e.message : String(e)),
      ),
    [project],
  );
  useEffect(() => {
    void load();
  }, [load]);

  const add = async () => {
    const environments = envs.split(",").map((e) => e.trim()).filter(Boolean);
    const where = kind === "email" ? {} : kind === "pagerduty" ? { routing_key: url.trim(), region } : kind === "opsgenie" ? { api_key: url.trim(), region } : { url: url.trim() };
    const t = await runner.run("notification", () => appsApi.addNotifyTarget(project, { kind, ...where, events, environments, previews, ...(only.length ? { services: only } : {}) }));
    if (t) {
      setUrl("");
      setEnvs("");
      setOnly([]);
      setSecret(t.secret ?? null);
      void load();
    }
  };

  return (
    <Panel title={`Notifications for ${projectName}`} aside={<span className="text-[12px] text-muted">the project's services</span>}>
      {failed && <p className="border-b border-line px-5 py-3 text-[12.5px] text-danger">Its targets couldn't be read: {failed}</p>}
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
                  · {t.services.length ? t.services.map((id) => all.find((s) => s.id === id)?.name ?? id).join(", ") : "every service"}
                  {" "}· {t.environments.length ? t.environments.join(", ") : "every environment"}
                  {t.previews ? ", previews too" : ""}
                </span>
                <span className="ml-auto flex gap-1.5">
                  <button type="button" className={btn} disabled={runner.busy !== null} onClick={() => void runner.run("test", () => appsApi.testNotifyTarget(project, t.id)).then((r) => r && !r.ok && runner.setError(`The test didn't arrive: ${r.error ?? "no answer"}`))}>
                    <Send className="size-3.5" /> Test
                  </button>
                  <button type="button" className={btn} aria-expanded={editing === t.id} onClick={() => setEditing(editing === t.id ? null : t.id)}>
                    Events
                  </button>
                  <button type="button" className={btn} onClick={() => setOpen(open === t.id ? null : t.id)}>
                    Deliveries
                  </button>
                  <SureButton className={btn} label="Remove" sure="Sure? Remove" disabled={runner.busy !== null} onSure={() => void runner.run("remove", () => appsApi.removeNotifyTarget(project, t.id)).then(load)}>
                    <Trash2 className="size-3.5" />
                  </SureButton>
                </span>
              </div>
              {editing === t.id && (
                <EditEvents
                  current={t.events}
                  busy={runner.busy !== null}
                  onSave={async (events) => {
                    if (await runner.run("events", () => appsApi.updateNotifyTarget(project, t.id, { events }))) {
                      setEditing(null);
                      void load();
                    }
                  }}
                />
              )}
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
        {kind === "email" ? (
          <p className="min-w-0 flex-1 text-[12px] text-muted">To the project's owner, at their account's address.</p>
        ) : (
          <>
            <input
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              placeholder={KINDS.find((k) => k.value === kind)?.placeholder}
              aria-label={oncall ? "Key" : "URL"}
              type={oncall ? "password" : "text"}
              spellCheck={false}
              autoComplete="off"
              className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
            />
            {oncall && (
              <select value={region} onChange={(e) => setRegion(e.target.value as "us" | "eu")} aria-label="Region" className={`${input} w-20 text-[12px]`}>
                <option value="us">US</option>
                <option value="eu">EU</option>
              </select>
            )}
          </>
        )}
        <div className="flex flex-wrap gap-x-3 gap-y-1 text-[12px] text-muted">
          {EVENTS.map((e) => (
            <label key={e.value} className="inline-flex items-center gap-1">
              <input type="checkbox" checked={events.includes(e.value)} onChange={(x) => setEvents((es) => (x.target.checked ? [...es, e.value] : es.filter((v) => v !== e.value)))} />
              {e.label}
            </label>
          ))}
        </div>
        <input value={envs} onChange={(e) => setEnvs(e.target.value)} placeholder="every environment" aria-label="Only these environments (comma-separated)" title="Only these environments' events (comma-separated); empty: every environment" spellCheck={false} autoComplete="off" className={`${input} w-44 font-mono text-[12px]`} />
        {all.length > 1 && (
          <div className="flex flex-wrap gap-x-3 gap-y-1 text-[12px] text-muted" role="group" aria-label="Only these services (none: every one)">
            {all.map((s) => (
              <label key={s.id} className="inline-flex items-center gap-1">
                <input type="checkbox" checked={only.includes(s.id)} onChange={(x) => setOnly((o) => (x.target.checked ? [...o, s.id] : o.filter((v) => v !== s.id)))} />
                {s.name}
              </label>
            ))}
          </div>
        )}
        <label className="inline-flex items-center gap-1 text-[12px] text-muted" title="Each pull request's preview: its failed deploys and alerts too">
          <input type="checkbox" checked={previews} onChange={(e) => setPreviews(e.target.checked)} />
          Include previews
        </label>
        <button type="submit" className={primary} disabled={(kind !== "email" && !url.trim()) || !events.length || runner.busy !== null}>
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

/** A target's events, changed in place. A resolved alert goes to whoever gets that alert. */
function EditEvents({ current, busy, onSave }: { current: NotifyEvent[]; busy: boolean; onSave: (events: NotifyEvent[]) => void }) {
  const [events, setEvents] = useState<NotifyEvent[]>(current);
  return (
    <div className="rise mt-3 flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px] text-muted">
      {EVENTS.map((e) => (
        <label key={e.value} className="inline-flex items-center gap-1">
          <input type="checkbox" checked={events.includes(e.value)} onChange={(x) => setEvents((es) => (x.target.checked ? [...es, e.value] : es.filter((v) => v !== e.value)))} />
          {e.label}
        </label>
      ))}
      <button type="button" className={btn} disabled={busy || events.length === 0} onClick={() => onSave(events)}>
        Save
      </button>
    </div>
  );
}
