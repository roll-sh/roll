/**
 * The owner's Linear workspace in the agents settings (docs/code/http-api.md, "Linear"): connect
 * it (roll's app, as an agent, or a personal API key), see its webhook and what Linear delivered,
 * map teams to roll code repos, paste a hand-made webhook's signing secret, disconnect. Tokens and
 * secrets only ever go to roll; nothing here shows one.
 */

import { Check, Copy, ExternalLink, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useState, type FormEvent } from "react";
import { linearReturn, type LinearConnection, type LinearReturn, type LinearSettings, type LinearStatus, type LinearTeam, type LinearWebhookStatus } from "@/lib/linear-api";
import { LookupPicker } from "./lookup-picker";
import { field, Note, SaveState, SettingRow, SettingsSection, type Setting } from "./settings-ui";
import { ago } from "./shared";
import { cn } from "./ui";
import { useAgents } from "./use-agents";
import { useEscapeLayer } from "./escape-layers";

const S = {
  connect: {
    id: "linear.connect",
    title: "Connect Linear",
    section: "Linear",
    description: "Assign or mention roll on an issue and an agent works on it; automations hear the workspace's issues and comments.",
    terms: ["linear", "oauth", "api key", "integration", "issues", "workspace"],
  },
  workspace: { id: "linear.workspace", title: "Workspace", section: "Linear", terms: ["linear", "connected", "organization"] },
  reauth: { id: "linear.reauth", title: "Connect again", section: "Linear", terms: ["linear", "reconnect", "token", "expired"] },
  webhook: { id: "linear.webhook", title: "Webhook", section: "Linear", terms: ["linear", "deliveries", "events", "url"] },
  secret: { id: "linear.secret", title: "Signing secret", section: "Linear", terms: ["linear", "webhook", "secret", "signature"] },
  repos: {
    id: "linear.repos",
    title: "Repos by team",
    section: "Linear",
    description: "Which roll code repo a team's issues are worked in. Agents start in it.",
    terms: ["linear", "team", "repository", "default repo", "mapping"],
  },
  deliveries: { id: "linear.deliveries", title: "Recent deliveries", section: "Linear", terms: ["linear", "webhook", "events", "log"] },
  disconnect: { id: "linear.disconnect", title: "Disconnect", section: "Linear", description: "roll forgets the workspace and stops hearing from it.", terms: ["linear", "remove", "revoke"] },
} satisfies Record<string, Setting>;

/** Every Linear row, for the settings search. */
export const LINEAR_SETTINGS: readonly Setting[] = Object.values(S);

/** Where Linear's OAuth sends the browser back to: this page, with these settings open. */
const returnTo = () => `${window.location.pathname}${window.location.search}#agents/settings/owner`;

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function LinearSettingsSection() {
  const { owner, services } = useAgents();
  const linear = services?.linear;
  const [status, setStatus] = useState<LinearStatus | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<LinearReturn | null>(null);

  const load = useCallback(() => {
    if (!linear) return;
    linear.getLinear(owner).then(
      (s) => {
        setStatus(s);
        setLoadError(null);
      },
      (e) => setLoadError(message(e)),
    );
  }, [linear, owner]);

  useEffect(load, [load]);

  // Back from Linear's OAuth: say how it went once, and take it off the URL.
  useEffect(() => {
    const read = () => {
      const { result, clean } = linearReturn(window.location.href);
      if (!result) return;
      setNotice(result);
      window.history.replaceState(window.history.state, "", clean);
      load();
    };
    read();
    window.addEventListener("hashchange", read);
    return () => window.removeEventListener("hashchange", read);
  }, [load]);

  /** Run a change; one that answers the status shows it. */
  const act = async (f: () => Promise<LinearStatus | void>) => {
    setBusy(true);
    setError(null);
    try {
      const s = await f();
      if (s) setStatus(s);
      return true;
    } catch (e) {
      setError(message(e));
      return false;
    } finally {
      setBusy(false);
    }
  };

  const teams = useCallback(() => (linear ? linear.getTeams(owner) : Promise.resolve([])), [linear, owner]);

  if (!linear) return null;
  const c = status?.connection ?? null;
  return (
    <SettingsSection title="Linear" ids={LINEAR_SETTINGS.map((s) => s.id)}>
      {notice && <Notice notice={notice} onDismiss={() => setNotice(null)} />}
      {(busy || error) && (
        <div className="px-3.5 pt-3 [&>p]:mb-0">
          <SaveState saving={busy} error={error} />
        </div>
      )}
      {!status ? (
        <SettingRow s={S.connect}>
          <p className="mt-2 text-[12px] text-subtle">{loadError ? <span className="text-danger">{loadError}</span> : <span className="shimmer">Loading…</span>}</p>
        </SettingRow>
      ) : !c ? (
        <Connect
          status={status}
          busy={busy}
          onOAuth={() =>
            void act(async () => {
              window.location.href = await linear.connectOAuth(owner, returnTo());
            })
          }
          onKey={(key) => act(() => linear.putKey(owner, key))}
        />
      ) : (
        <Connected
          status={status}
          c={c}
          busy={busy}
          onReconnect={() =>
            void act(async () => {
              window.location.href = await linear.connectOAuth(owner, returnTo());
            })
          }
          onKey={(key) => act(() => linear.putKey(owner, key))}
          onSecret={(secret) => act(() => linear.putWebhookSecret(owner, secret))}
          onSettings={(settings) => act(() => linear.putSettings(owner, settings))}
          teams={teams}
          onDisconnect={() =>
            act(async () => {
              await linear.disconnect(owner);
              return { ...status, connected: false, connection: null, deliveries: [] };
            })
          }
        />
      )}
    </SettingsSection>
  );
}

function Notice({ notice, onDismiss }: { notice: LinearReturn; onDismiss: () => void }) {
  const ok = notice.outcome === "connected";
  return (
    <p role="status" className={cn("flex items-center gap-2 px-3.5 py-2.5 text-[12.5px]", ok ? "text-fg" : "text-danger")}>
      {ok ? <Check className="size-3.5 shrink-0 text-ok" strokeWidth={1.5} /> : <X className="size-3.5 shrink-0" strokeWidth={1.5} />}
      <span className="min-w-0 flex-1">{ok ? "Linear is connected." : `Linear didn't connect: ${notice.reason}`}</span>
      <button type="button" onClick={onDismiss} aria-label="Dismiss" className="grid size-5 place-items-center rounded text-subtle hover:text-fg">
        <X className="size-3" />
      </button>
    </p>
  );
}

const primary = "h-7 rounded-md bg-fg px-2.5 text-[12px] font-medium text-bg hover:opacity-85 disabled:opacity-40";
const secondary = "h-7 rounded-md border border-line-2 px-2.5 text-[12px] text-muted hover:text-fg disabled:opacity-40";

/** A secret typed once and sent (never shown again, never saved on blur). */
function SecretForm({ label, placeholder, submit, disabled, onSubmit }: { label: string; placeholder: string; submit: string; disabled?: boolean; onSubmit: (v: string) => Promise<boolean> }) {
  const [value, setValue] = useState("");
  const send = async (e: FormEvent) => {
    e.preventDefault();
    if (value.trim() && (await onSubmit(value.trim()))) setValue("");
  };
  return (
    <form onSubmit={send} className="mt-2.5 flex flex-wrap items-center gap-2">
      <input
        type="password"
        aria-label={label}
        value={value}
        placeholder={placeholder}
        disabled={disabled}
        autoComplete="off"
        spellCheck={false}
        onChange={(e) => setValue(e.target.value)}
        className={cn(field, "w-72 max-w-full font-mono text-[12px]")}
      />
      <button type="submit" disabled={disabled || !value.trim()} className={secondary}>
        {submit}
      </button>
    </form>
  );
}

function Connect({ status, busy, onOAuth, onKey }: { status: LinearStatus; busy: boolean; onOAuth: () => void; onKey: (key: string) => Promise<boolean> }) {
  const noKey = !status.app.secret_key;
  return (
    <SettingRow
      s={S.connect}
      note={noKey ? <Note title="Tokens are sealed with it">needs ROLL_SECRET_KEY</Note> : undefined}
      control={
        status.app.oauth ? (
          <button type="button" disabled={busy || noKey} onClick={onOAuth} className={primary}>
            Connect Linear
          </button>
        ) : undefined
      }
    >
      {noKey && <p className="mt-2 text-[12px] text-subtle">This server can't keep a Linear token until it has ROLL_SECRET_KEY set.</p>}
      <p className="mt-2 text-[12px] text-subtle">
        {status.app.oauth ? "Or with a personal API key" : "With a personal API key"} (Linear: Settings → Security &amp; access → Personal API keys). An admin&apos;s key lets roll make the webhook
        itself.
      </p>
      <SecretForm label="Linear API key" placeholder="lin_api_…" submit="Connect" disabled={busy || noKey} onSubmit={onKey} />
    </SettingRow>
  );
}

const HOOK_WORD: Record<LinearWebhookStatus, string> = { ok: "Receiving", waiting: "Waiting for a delivery", failing: "Failing", needs_secret: "Needs its secret" };
const HOOK_DOT: Record<LinearWebhookStatus, string> = { ok: "bg-ok", waiting: "bg-subtle", failing: "bg-danger", needs_secret: "border border-muted" };
const MODE: Record<LinearConnection["webhook"]["mode"], string> = {
  app: "roll's Linear app delivers every workspace's events here.",
  workspace: "roll made this webhook in Linear for issues and comments.",
  manual: "Make a webhook in Linear (Settings → API → Webhooks) to this URL for Issues and Comments, then paste its signing secret below.",
};

function Connected({
  status,
  c,
  busy,
  onReconnect,
  onKey,
  onSecret,
  onSettings,
  teams,
  onDisconnect,
}: {
  status: LinearStatus;
  c: LinearConnection;
  busy: boolean;
  onReconnect: () => void;
  onKey: (key: string) => Promise<boolean>;
  onSecret: (secret: string) => Promise<boolean>;
  onSettings: (s: LinearSettings) => Promise<boolean>;
  teams: () => Promise<LinearTeam[]>;
  onDisconnect: () => Promise<boolean>;
}) {
  const hook = c.webhook;
  const oauth = c.kind === "oauth";
  return (
    <>
      <SettingRow
        s={{ ...S.workspace, description: `${oauth ? (c.agent ? "roll app (agent)" : "roll app") : "API key"} · as ${c.user.name}` }}
        control={
          <a href={c.workspace.url} target="_blank" rel="noreferrer" className="flex items-center gap-1 text-[13px] font-medium text-fg hover:text-accent">
            {c.workspace.name} <ExternalLink className="size-3.5 text-subtle" strokeWidth={1.5} />
          </a>
        }
      />
      {c.status === "reauth" && (
        <SettingRow
          s={{ ...S.reauth, description: c.error ?? "Linear stopped taking roll's token." }}
          note={<span className="size-1.5 rounded-full bg-danger" aria-hidden />}
          control={
            oauth && status.app.oauth ? (
              <button type="button" disabled={busy} onClick={onReconnect} className={primary}>
                Reconnect
              </button>
            ) : undefined
          }
        >
          {!oauth && <SecretForm label="New Linear API key" placeholder="lin_api_…" submit="Reconnect" disabled={busy} onSubmit={onKey} />}
        </SettingRow>
      )}
      <SettingRow
        s={S.webhook}
        control={
          <span className="flex items-center gap-1.5 text-[12px] text-muted">
            <span className={cn("size-1.5 rounded-full", HOOK_DOT[hook.status])} aria-hidden />
            {HOOK_WORD[hook.status]}
          </span>
        }
      >
        <p className="mt-1 text-[12px] leading-snug text-subtle">{MODE[hook.mode]}</p>
        <Url url={hook.url} />
        <p className="mt-1.5 text-[12px] text-subtle">{hook.last_delivery_at ? `Last delivery ${ago(hook.last_delivery_at)}` : "Nothing delivered yet."}</p>
        {hook.status === "failing" && hook.rejected_why && (
          <p className="mt-1 text-[12px] text-danger">
            Rejected {hook.rejected_at ? ago(hook.rejected_at) : ""}: {hook.rejected_why}
          </p>
        )}
        {oauth && hook.status === "needs_secret" && <p className="mt-1 text-[12px] text-subtle">The server needs ROLL_LINEAR_WEBHOOK_SECRET, the app&apos;s signing secret.</p>}
      </SettingRow>
      {!oauth && (hook.mode === "manual" || hook.status === "needs_secret") && (
        <SettingRow s={S.secret} note={hook.secret_hint ? <Note>…{hook.secret_hint}</Note> : undefined}>
          <SecretForm label="Webhook signing secret" placeholder="lin_wh_…" submit={hook.secret_hint ? "Replace" : "Save"} disabled={busy} onSubmit={onSecret} />
        </SettingRow>
      )}
      <Repos settings={c.settings} teams={teams} disabled={busy} onSave={onSettings} />
      <Deliveries deliveries={status.deliveries} />
      <Disconnect name={c.workspace.name} busy={busy} onDisconnect={onDisconnect} />
    </>
  );
}

function Url({ url }: { url: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <p className="mt-2 flex min-w-0 items-center gap-1.5 text-[11.5px]">
      <code className="min-w-0 truncate rounded border border-line-2 bg-panel-2 px-1.5 py-0.5 font-mono text-muted" title={url}>
        {url}
      </code>
      <button
        type="button"
        aria-label="Copy the webhook URL"
        title={copied ? "Copied" : "Copy"}
        onClick={() => void navigator.clipboard?.writeText(url).then(() => setCopied(true))}
        className="grid size-5 shrink-0 place-items-center rounded text-subtle hover:bg-fg/[0.06] hover:text-fg"
      >
        {copied ? <Check className="size-3" /> : <Copy className="size-3" />}
      </button>
    </p>
  );
}

/** The owner's roll code repos (a team's issues are in one). */
const REPOS = { source: "code", kind: "repos" } as const;

function Repos({ settings, teams, disabled, onSave }: { settings: LinearSettings; teams: () => Promise<LinearTeam[]>; disabled: boolean; onSave: (s: LinearSettings) => Promise<boolean> }) {
  const [known, setKnown] = useState<LinearTeam[] | null>(null);
  const [adding, setAdding] = useState("");
  /** Keys added here that Linear didn't list and that have no repo yet. */
  const [extra, setExtra] = useState<string[]>([]);
  useEffect(() => {
    let live = true;
    teams().then(
      (t) => live && setKnown(t),
      () => live && setKnown([]),
    );
    return () => {
      live = false;
    };
  }, [teams]);
  // Linear's teams, then any mapped or added key it didn't list.
  const keys = [...new Set([...(known ?? []).map((t) => t.key), ...Object.keys(settings.repos), ...extra])];
  const name = (key: string) => known?.find((t) => t.key === key)?.name;
  const setRepo = (team: string, repo: string) => {
    const repos = { ...settings.repos };
    if (repo) repos[team] = repo;
    else delete repos[team];
    return onSave({ ...settings, repos });
  };
  const setDefault = (repo: string) => {
    const { default_repo: _, ...rest } = settings;
    return onSave(repo ? { ...rest, default_repo: repo } : rest);
  };
  const add = (e: FormEvent) => {
    e.preventDefault();
    const team = adding.trim();
    if (!team) return;
    if (!keys.includes(team)) setExtra((x) => [...x, team]);
    setAdding("");
  };
  return (
    <SettingRow s={S.repos}>
      <ul className="mt-2.5 space-y-1.5" aria-label="Repos by team">
        {keys.map((key) => (
          <li key={key} className="flex items-center gap-2">
            <span className="w-16 shrink-0 truncate font-mono text-[12px] text-fg" title={name(key)}>
              {key}
            </span>
            <span className="min-w-0 flex-1 truncate text-[12px] text-subtle">{name(key) ?? ""}</span>
            <LookupPicker label={`${key} repo`} from={REPOS} value={settings.repos[key] ?? null} placeholder={settings.default_repo ?? "Pick a repo"} custom clearable disabled={disabled} className="w-48" onChange={(r) => void setRepo(key, r ?? "")} />
          </li>
        ))}
        <li className="flex items-center gap-2 pt-1">
          <span className="min-w-0 flex-1 text-[12px] text-muted">Default repo</span>
          <LookupPicker label="Default repo" from={REPOS} value={settings.default_repo ?? null} placeholder="None" custom clearable disabled={disabled} className="w-48" onChange={(r) => void setDefault(r ?? "")} />
        </li>
      </ul>
      {known === null ? (
        <p className="mt-2 text-[12px] text-subtle">
          <span className="shimmer">Asking Linear for its teams…</span>
        </p>
      ) : (
        <form onSubmit={add} className="mt-2 flex items-center gap-2">
          <input aria-label="Team key" value={adding} placeholder="Another team's key" spellCheck={false} onChange={(e) => setAdding(e.target.value.toUpperCase())} className={cn(field, "h-7 w-44 font-mono text-[12px]")} />
          <button type="submit" disabled={!adding.trim()} className={secondary}>
            Add
          </button>
        </form>
      )}
    </SettingRow>
  );
}

const RESULT: Record<LinearStatus["deliveries"][number]["result"], string> = { recorded: "text-fg", duplicate: "text-subtle", ignored: "text-subtle" };

function Deliveries({ deliveries }: { deliveries: LinearStatus["deliveries"] }) {
  const sorted = [...deliveries].sort((a, b) => b.at - a.at);
  return (
    <SettingRow s={S.deliveries}>
      {sorted.length === 0 ? (
        <p className="mt-1.5 text-[12px] text-subtle">Nothing yet: what Linear sends shows here.</p>
      ) : (
        <ul className="mt-2 divide-y divide-line rounded-lg border border-line-2" aria-label="Recent deliveries">
          {sorted.map((d) => (
            <li key={d.id} className="flex items-center gap-2.5 px-2.5 py-1.5 text-[12px]">
              <span className="w-20 shrink-0 font-mono text-[11px] text-subtle tabular-nums" title={new Date(d.at).toLocaleString()}>
                {ago(d.at)}
              </span>
              <span className="w-36 shrink-0 truncate font-mono text-[11px] text-muted">
                {d.type}
                {d.action ? `.${d.action}` : ""}
              </span>
              <span className="min-w-0 flex-1 truncate text-soft" title={d.summary}>
                {d.summary}
              </span>
              <span className={cn("shrink-0", RESULT[d.result])}>{d.result}</span>
            </li>
          ))}
        </ul>
      )}
    </SettingRow>
  );
}

function Disconnect({ name, busy, onDisconnect }: { name: string; busy: boolean; onDisconnect: () => Promise<boolean> }) {
  const [asking, setAsking] = useState(false);
  useEscapeLayer(asking, () => setAsking(false));
  return (
    <SettingRow
      s={S.disconnect}
      control={
        asking ? (
          <span className="flex items-center gap-1">
            <button type="button" disabled={busy} onClick={() => void onDisconnect().then(() => setAsking(false))} className="h-7 rounded-md bg-danger/10 px-2.5 text-[12px] font-medium text-danger hover:bg-danger/20">
              {busy ? <span className="shimmer">Disconnecting…</span> : `Disconnect ${name}`}
            </button>
            <button type="button" onClick={() => setAsking(false)} className="grid size-7 place-items-center rounded-md text-subtle hover:text-fg" aria-label="Keep it">
              <X className="size-3.5" />
            </button>
          </span>
        ) : (
          <button type="button" onClick={() => setAsking(true)} className="flex h-7 items-center gap-1.5 rounded-md border border-line-2 px-2.5 text-[12px] text-danger hover:bg-danger/10">
            <Trash2 className="size-3.5" strokeWidth={1.5} /> Disconnect
          </button>
        )
      }
    />
  );
}
