import { Bell, Trash2 } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { createIntegrationsApi, type ChatInstall } from "@/lib/integrations-api";
import { call } from "@/lib/roll-api";
import type { Environment, SecretsApi, SecretsNotification } from "@/lib/secrets-api";
import { FAMILIES, type Family } from "@/lib/secrets-events";
import { channelLabel, kindsLabel, notifyBody, notifyProblem, type NotifyTarget } from "@/lib/secrets-notify";
import { useShownOwner } from "../agents/use-connection";
import { Select } from "../agents/ui/select";
import { ago, card, Chip, ErrorNote, errorText, field, ghost, iconButton, label, primary } from "./ui";

const ALL = "*";
const EMAIL = "email";
const integrations = createIntegrationsApi(call);

/** The person's own chat installs (their roll code owner's), for the channel picker. */
function useInstalls(): ChatInstall[] {
  const shown = useShownOwner();
  const [installs, setInstalls] = useState<ChatInstall[]>([]);
  useEffect(() => {
    if (!shown?.owner) return;
    let live = true;
    integrations.list(shown.owner).then(
      (i) => live && setInstalls([...i.slack.installs.map((x) => ({ ...x, platform: "slack" as const })), ...i.discord.installs.map((x) => ({ ...x, platform: "discord" as const }))]),
      () => live && setInstalls([]),
    );
    return () => {
      live = false;
    };
  }, [shown?.owner]);
  return installs;
}

type Platformed = ChatInstall & { platform?: "slack" | "discord" };

/**
 * "Notify me" on the Events tab (docs/secrets.md, "Being told"): kind chips and an environment
 * (starting from the tab's filter), then where to be told: the person's own email, or a Slack or
 * Discord channel through one of their own chat installs. Below the form, the person's
 * subscriptions in this project (an admin's: everyone's), each removable.
 */
export function NotifyMenu({ api, envs, families: tabFamilies, environment: tabEnv }: { api: Pick<SecretsApi, "notifications" | "notify" | "stopNotification">; envs: Environment[]; families: Family[]; environment?: string }) {
  const [open, setOpen] = useState(false);
  const [families, setFamilies] = useState<Family[]>(tabFamilies);
  const [env, setEnv] = useState(tabEnv ?? ALL);
  const [where, setWhere] = useState(EMAIL);
  const [channel, setChannel] = useState("");
  const [mine, setMine] = useState<SecretsNotification[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const installs: Platformed[] = useInstalls();
  const root = useRef<HTMLDivElement>(null);

  const reload = () => api.notifications().then(setMine, (e) => setError(errorText(e)));
  useEffect(() => {
    if (!open) return;
    // Each opening starts from the tab's filter.
    setFamilies(tabFamilies);
    setEnv(tabEnv ?? ALL);
    void reload();
    const away = (e: MouseEvent) => root.current && !root.current.contains(e.target as Node) && setOpen(false);
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", away);
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("mousedown", away);
      document.removeEventListener("keydown", esc);
    };
  }, [open]);

  const picked = installs.find((i) => i.id === where);
  const target: NotifyTarget = picked ? { kind: picked.platform ?? "slack", install: picked.id, channel } : { kind: "email" };
  const choice = { families, environment: env === ALL ? undefined : env, target };
  const problem = notifyProblem(choice);
  const toggle = (f: Family) => setFamilies((on) => (on.includes(f) ? on.filter((x) => x !== f) : [...on, f]));

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.notify(notifyBody(choice));
      setChannel("");
      await reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const stop = (id: string) => api.stopNotification(id).then(reload, (e) => setError(errorText(e)));

  return (
    <div ref={root} className="relative">
      <button onClick={() => setOpen((o) => !o)} className={ghost} aria-expanded={open}>
        <Bell className="size-3.5" strokeWidth={1.5} />
        Notify me
      </button>
      {open && (
        <div className={`${card} rise absolute top-full right-0 z-20 mt-2 w-[22rem] max-w-[calc(100vw-2rem)] p-3 shadow-pop motion-reduce:animate-none`} role="dialog" aria-label="Notify me">
          <span className={label}>Tell me about</span>
          <div className="flex flex-wrap gap-1.5">
            <Chip on={families.length === 0} onClick={() => setFamilies([])}>
              Everything
            </Chip>
            {FAMILIES.map((f) => (
              <Chip key={f.id} on={families.includes(f.id)} onClick={() => toggle(f.id)} title={f.kinds.join(", ")}>
                {f.label}
              </Chip>
            ))}
          </div>
          <div className="mt-3 grid grid-cols-2 gap-2">
            <div>
              <span className={label}>In</span>
              <Select label="Environment" value={env} onChange={setEnv} options={[{ value: ALL, label: "All environments" }, ...envs.map((e) => ({ value: e.slug, label: e.name }))]} />
            </div>
            <div>
              <span className={label}>Through</span>
              <Select
                label="Channel"
                value={where}
                onChange={setWhere}
                options={[{ value: EMAIL, label: "My email" }, ...installs.map((i) => ({ value: i.id, label: `${i.platform === "discord" ? "Discord" : "Slack"} · ${i.team_name}` }))]}
              />
            </div>
          </div>
          {picked && (
            <div className="mt-2">
              <span className={label}>Channel id</span>
              <input value={channel} onChange={(e) => setChannel(e.target.value)} placeholder="C0123ABCD" className={`${field} font-mono`} aria-label="Channel id" />
            </div>
          )}
          {!installs.length && <p className="mt-2 text-[11.5px] text-subtle">Slack and Discord go through your own chat installs: connect one in the agents' integrations.</p>}
          <ErrorNote error={error} onDismiss={() => setError(null)} className="mt-2" />
          <div className="mt-3 flex items-center justify-between gap-2">
            <span className="text-[11.5px] text-subtle">{problem ?? "Names only, never a value."}</span>
            <button onClick={() => void save()} disabled={busy || !!problem} className={primary}>
              {busy ? "Saving…" : "Notify me"}
            </button>
          </div>
          {mine && mine.length > 0 && (
            <div className="mt-3 border-t border-line pt-2">
              <span className={label}>{mine.some((n) => n.user !== mine[0].user) ? "In this project" : "Yours"}</span>
              <ul className="space-y-1.5">
                {mine.map((n) => (
                  <li key={n.id} className="flex items-start gap-2 text-[12px]">
                    <div className="min-w-0 flex-1">
                      <div className="truncate">
                        {kindsLabel(n.kinds)}
                        {n.environment && <span className="font-mono text-[11px] text-muted"> · {n.environment}</span>}
                      </div>
                      <div className="truncate text-[11px] text-muted">
                        {channelLabel(n, installs)}
                        {n.delivered_at ? ` · last ${ago(n.delivered_at)}` : ""}
                      </div>
                      {n.error && (
                        <div className={`text-[11px] ${n.state === "disabled" ? "text-danger" : "text-subtle"}`} title={n.error}>
                          {n.state === "disabled" ? "Stopped: " : ""}
                          {n.error}
                        </div>
                      )}
                    </div>
                    <button onClick={() => void stop(n.id)} className={iconButton} aria-label="Stop these notifications" title="Stop">
                      <Trash2 className="size-3.5" strokeWidth={1.5} />
                    </button>
                  </li>
                ))}
              </ul>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
