"use client";

/**
 * Chat ops in the owner's agents settings: one card each for Slack, Discord and GitHub
 * (`lib/integrations-api.ts`, `code-control/src/chatops/`). Slack and Discord say whether the
 * server can connect them, list the connected workspaces or servers, and edit who may drive roll
 * from each (the installer, member ids, or anyone there); GitHub lists the owner's repos mirrored
 * to GitHub. Coming back from a platform's install page
 * (`/dashboard/agents?settings=integrations&integration=slack&connected=Acme`) opens the settings
 * here, scrolled to this section, with the result in a notice.
 */

import { CircleAlert, CircleCheck, ExternalLink, Plus, TriangleAlert, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import { setAgentsView } from "@/lib/agents-surface";
import {
  isMemberId,
  MAX_MEMBERS,
  type ChatInstall,
  type ChatIntegration,
  type ChatPlatform,
  type GitHubIntegration,
  type GitHubRepoLink,
  type InstallPatch,
  type Integrations,
  type IntegrationsApi,
  type IntegrationsLink,
} from "@/lib/integrations-api";
import { DiscordMark, GitHubMark, SlackMark, type Glyph } from "./brand-icons";
import { field, SettingRow, SettingsSection, type Setting } from "./settings-ui";
import { cn, Switch } from "./ui";

export const INTEGRATION_SETTINGS = {
  slack: {
    id: "integrations.slack",
    title: "Slack",
    section: "Integrations",
    description: "/roll, mentions and reactions start agents; their status updates in the thread.",
    terms: ["chat ops", "workspace", "slash command", "bot", "members"],
  },
  discord: {
    id: "integrations.discord",
    title: "Discord",
    section: "Integrations",
    description: "/roll starts agents; their status updates in the channel.",
    terms: ["chat ops", "server", "guild", "slash command", "bot", "members"],
  },
  github: {
    id: "integrations.github",
    title: "GitHub",
    section: "Integrations",
    description: "Issues, @roll comments and failed checks start agents; agents open pull requests.",
    terms: ["github app", "mirror", "pull request", "issues", "checks"],
  },
} satisfies Record<ChatPlatform | "github", Setting>;

const IDS = Object.values(INTEGRATION_SETTINGS).map((s) => s.id);

const CHAT: Record<ChatPlatform, { name: string; unit: string; units: string; env: string[]; icon: Glyph; memberHint: string }> = {
  slack: {
    name: "Slack",
    unit: "workspace",
    units: "workspaces",
    env: ["ROLL_SLACK_CLIENT_ID", "ROLL_SLACK_CLIENT_SECRET", "ROLL_SLACK_SIGNING_SECRET"],
    icon: SlackMark,
    memberHint: "Slack user id (U…)",
  },
  discord: {
    name: "Discord",
    unit: "server",
    units: "servers",
    env: ["ROLL_DISCORD_APPLICATION_ID", "ROLL_DISCORD_PUBLIC_KEY", "ROLL_DISCORD_BOT_TOKEN", "ROLL_DISCORD_CLIENT_SECRET"],
    icon: DiscordMark,
    memberHint: "Discord user id",
  },
};

const GITHUB_ENV = ["ROLL_GITHUB_APP_ID", "ROLL_GITHUB_APP_SLUG", "ROLL_GITHUB_PRIVATE_KEY_PATH", "ROLL_GITHUB_WEBHOOK_SECRET"];

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

// ── the deep link ──

/** The link back from a platform, until the section has shown it. */
let pending: IntegrationsLink | null = null;

/** Open the owner's settings on the integrations, with the link's result. */
export function openIntegrations(link: IntegrationsLink) {
  pending = link;
  setAgentsView({ kind: "settings", target: "owner" });
}

/** For tests. */
export function resetIntegrationsLink() {
  pending = null;
}

// ── the section ──

const button = "flex h-7 items-center gap-1.5 rounded-md px-2.5 text-[12px] font-medium transition-colors disabled:opacity-50";
const primary = cn(button, "bg-fg text-bg hover:opacity-85");
const quiet = cn(button, "border border-line-2 text-muted hover:bg-fg/[0.06] hover:text-fg");

export function IntegrationsSection({
  owner,
  api,
  navigate = (url) => window.location.assign(url),
}: {
  owner: string;
  api: IntegrationsApi;
  /** Where Connect goes (the platform's install page). */
  navigate?: (url: string) => void;
}) {
  const [data, setData] = useState<Integrations | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Read, not taken: React may run this twice; the effect below forgets it once shown.
  const [link, setLink] = useState<IntegrationsLink | null>(() => pending);
  const top = useRef<HTMLDivElement>(null);

  const load = useCallback(() => {
    setError(null);
    api.list(owner).then(setData, (e: unknown) => setError(message(e)));
  }, [api, owner]);
  useEffect(load, [load]);

  useEffect(() => {
    if (!link) return;
    pending = null;
    top.current?.scrollIntoView({ block: "start" });
  }, [link]);

  const setChat = (platform: ChatPlatform, f: (installs: ChatInstall[]) => ChatInstall[]) =>
    setData((d) => (d ? { ...d, [platform]: { ...d[platform], installs: f(d[platform].installs) } } : d));

  return (
    <div ref={top} id="integrations" className="scroll-mt-3">
      {link && <LinkNotice link={link} onDismiss={() => setLink(null)} />}
      <SettingsSection title="Integrations" ids={IDS}>
        {(["slack", "discord"] as const).map((p) => (
          <ChatCard
            key={p}
            platform={p}
            owner={owner}
            api={api}
            integration={data?.[p] ?? null}
            sealing={data?.sealing ?? true}
            navigate={navigate}
            onSaved={(i) => setChat(p, (all) => all.map((x) => (x.id === i.id ? i : x)))}
            onRemoved={(id) => setChat(p, (all) => all.filter((x) => x.id !== id))}
          />
        ))}
        <GitHubCard github={data?.github ?? null} />
        {error && (
          <p role="alert" className="flex items-center gap-2 px-3.5 py-2.5 text-[12px] text-danger">
            <CircleAlert className="size-3.5 shrink-0" strokeWidth={1.5} />
            <span className="min-w-0 flex-1">Couldn&apos;t load the integrations: {error}</span>
            <button type="button" onClick={load} className={quiet}>
              Retry
            </button>
          </p>
        )}
      </SettingsSection>
    </div>
  );
}

function LinkNotice({ link, onDismiss }: { link: IntegrationsLink; onDismiss: () => void }) {
  const name = link.platform ? CHAT[link.platform].name : "The integration";
  const ok = !link.error;
  return (
    <div role={ok ? "status" : "alert"} className="rise mb-3 flex items-start gap-2 rounded-lg border border-line-2 bg-raised px-3 py-2 text-[12.5px]">
      {ok ? <CircleCheck className="mt-px size-3.5 shrink-0 text-ok" strokeWidth={1.5} /> : <CircleAlert className="mt-px size-3.5 shrink-0 text-danger" strokeWidth={1.5} />}
      <p className="min-w-0 flex-1 text-soft">
        {ok
          ? link.connected
            ? `Connected ${link.connected} to ${link.platform ? CHAT[link.platform].name : "roll"}. Choose below who can drive roll from it.`
            : `${name} is connected.`
          : `${name} wasn't connected: ${link.error}`}
      </p>
      <button type="button" onClick={onDismiss} aria-label="Dismiss" className="text-subtle hover:text-fg">
        <X className="size-3.5" strokeWidth={1.5} />
      </button>
    </div>
  );
}

function Env({ names }: { names: string[] }) {
  return (
    <>
      {names.map((n, i) => (
        <span key={n}>
          {i > 0 && (i === names.length - 1 ? " and " : ", ")}
          <code className="font-mono text-[11.5px] text-muted">{n}</code>
        </span>
      ))}
    </>
  );
}

function Hint({ children, warn = false }: { children: ReactNode; warn?: boolean }) {
  return (
    <p className="mt-2 flex items-start gap-1.5 text-[12px] leading-snug text-subtle">
      {warn && <TriangleAlert className="mt-px size-3.5 shrink-0" strokeWidth={1.5} />}
      <span>{children}</span>
    </p>
  );
}

function Checking() {
  return <span className="shimmer text-[12px] text-subtle">Checking…</span>;
}

// ── Slack and Discord ──

export function ChatCard({
  platform,
  owner,
  api,
  integration,
  sealing,
  navigate,
  onSaved,
  onRemoved,
}: {
  platform: ChatPlatform;
  owner: string;
  api: IntegrationsApi;
  /** `null` while loading. */
  integration: ChatIntegration | null;
  sealing: boolean;
  navigate: (url: string) => void;
  onSaved: (install: ChatInstall) => void;
  onRemoved: (id: string) => void;
}) {
  const c = CHAT[platform];
  const Icon = c.icon;
  const [opening, setOpening] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const installs = integration?.installs ?? [];
  const needsKey = platform === "slack" && !sealing;
  const canConnect = !!integration?.configured && !needsKey;

  const connect = async () => {
    setOpening(true);
    setError(null);
    try {
      const { url } = await api.install(owner, platform);
      navigate(url);
    } catch (e) {
      setError(message(e));
      setOpening(false);
    }
  };

  return (
    <SettingRow
      s={INTEGRATION_SETTINGS[platform]}
      note={
        <>
          <Icon className="size-3.5 text-subtle" strokeWidth={1.5} aria-hidden />
          {installs.length > 0 && (
            <span className="text-[11px] text-subtle">
              {installs.length} {installs.length === 1 ? c.unit : c.units}
            </span>
          )}
        </>
      }
      control={
        !integration ? (
          <Checking />
        ) : canConnect ? (
          <button type="button" onClick={() => void connect()} disabled={opening} className={installs.length ? quiet : primary}>
            <Plus className="size-3.5" strokeWidth={1.5} />
            {opening ? "Opening…" : installs.length ? "Add another" : "Connect"}
          </button>
        ) : undefined
      }
    >
      {integration && !integration.configured && (
        <Hint>
          Not configured on this server. Set <Env names={c.env} /> on the server, then restart it.
        </Hint>
      )}
      {integration?.configured && needsKey && (
        <Hint warn>
          Needs <code className="font-mono text-[11.5px] text-muted">ROLL_SECRET_KEY</code> on the server: roll seals Slack&apos;s tokens with it.
        </Hint>
      )}
      {error && (
        <p role="alert" className="mt-2 text-[12px] text-danger">
          {error}
        </p>
      )}
      {installs.length > 0 && (
        <ul className="mt-3 space-y-2" aria-label={`Connected ${c.units}`}>
          {installs.map((i) => (
            <InstallEditor key={i.id} platform={platform} owner={owner} api={api} install={i} onSaved={onSaved} onRemoved={onRemoved} />
          ))}
        </ul>
      )}
    </SettingRow>
  );
}

/** One workspace or server: who may drive roll from it, and disconnecting it. */
function InstallEditor({
  platform,
  owner,
  api,
  install,
  onSaved,
  onRemoved,
}: {
  platform: ChatPlatform;
  owner: string;
  api: IntegrationsApi;
  install: ChatInstall;
  onSaved: (install: ChatInstall) => void;
  onRemoved: (id: string) => void;
}) {
  const c = CHAT[platform];
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [asking, setAsking] = useState(false);

  const save = async (patch: InstallPatch) => {
    setBusy(true);
    setError(null);
    try {
      onSaved(await api.update(owner, platform, install.id, patch));
      return true;
    } catch (e) {
      setError(message(e));
      return false;
    } finally {
      setBusy(false);
    }
  };

  const add = async (e: FormEvent) => {
    e.preventDefault();
    const ids = draft.split(/[\s,]+/).filter(Boolean);
    if (!ids.length) return;
    const bad = ids.find((m) => !isMemberId(m));
    if (bad) return setError(`${bad} isn't a ${c.name} user id: letters and digits only.`);
    const members = [...new Set([...install.members, ...ids])];
    if (members.length > MAX_MEMBERS) return setError(`At most ${MAX_MEMBERS} members.`);
    if (await save({ members })) setDraft("");
  };

  const disconnect = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.remove(owner, platform, install.id);
      onRemoved(install.id);
    } catch (e) {
      setError(message(e));
      setBusy(false);
    }
  };

  const who = `Anyone in the ${c.unit}`;
  return (
    <li className="rounded-lg border border-line-2 bg-panel-2 px-3 py-2.5" aria-label={install.team_name}>
      <div className="flex items-center gap-2">
        <span className="truncate font-medium tracking-[-0.01em]">{install.team_name}</span>
        {install.team_id && <span className="font-mono text-[11px] text-subtle">{install.team_id}</span>}
        <span className="flex-1" />
        {busy && <span className="shimmer text-[11.5px] text-subtle">Saving…</span>}
        {!asking && (
          <button type="button" onClick={() => setAsking(true)} disabled={busy} className="h-6 rounded-md px-1.5 text-[12px] text-muted hover:text-danger disabled:opacity-50">
            Disconnect
          </button>
        )}
      </div>
      {asking && (
        <div className="mt-2 flex flex-wrap items-center gap-2 rounded-md border border-line-2 bg-raised px-2.5 py-2 text-[12px]">
          <span className="min-w-0 flex-1 text-muted">
            Disconnect {install.team_name}? roll stops answering there. The app stays installed in {c.name} until you remove it there.
          </span>
          <button type="button" onClick={() => void disconnect()} disabled={busy} className={cn(button, "border border-danger/40 text-danger hover:bg-danger/10")}>
            Disconnect
          </button>
          <button type="button" onClick={() => setAsking(false)} className="h-7 rounded-md px-2 text-[12px] text-muted hover:text-fg">
            Cancel
          </button>
        </div>
      )}

      <p className="mt-2.5 text-[12px] font-medium text-soft">Who can drive roll from here</p>
      {install.installer && (
        <p className="mt-1 text-[12px] text-subtle">
          <code className="font-mono text-[11.5px] text-muted">{install.installer}</code> installed it, and always can.
        </p>
      )}
      <ul className="mt-1.5 flex flex-wrap gap-1.5" aria-label="Members">
        {install.members.map((m) => (
          <li key={m} className="flex h-6 items-center gap-1 rounded-md border border-line-2 bg-raised pr-0.5 pl-2 font-mono text-[11.5px] text-muted">
            {m}
            <button
              type="button"
              aria-label={`Remove ${m}`}
              disabled={busy}
              onClick={() => void save({ members: install.members.filter((x) => x !== m) })}
              className="grid size-5 place-items-center rounded text-subtle hover:bg-fg/[0.06] hover:text-fg"
            >
              <X className="size-3" strokeWidth={1.5} />
            </button>
          </li>
        ))}
      </ul>
      <form onSubmit={(e) => void add(e)} className="mt-1.5 flex items-center gap-1.5">
        <input
          aria-label="Add a member"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder={c.memberHint}
          spellCheck={false}
          disabled={busy}
          className={cn(field, "h-7 w-52 max-w-full font-mono text-[12px]")}
        />
        <button type="submit" disabled={busy || !draft.trim()} className={quiet}>
          Add
        </button>
      </form>

      <div className="mt-3 flex items-start gap-2.5">
        <Switch size="sm" checked={install.anyone} disabled={busy} onCheckedChange={(on) => void save({ anyone: on })} aria-label={who} className="mt-px" />
        <div className="min-w-0 text-[12px] leading-snug">
          <p className="text-fg">{who}</p>
          <p className={cn("mt-0.5 flex items-start gap-1", install.anyone ? "text-soft" : "text-subtle")}>
            {install.anyone && <TriangleAlert className="mt-px size-3 shrink-0" strokeWidth={1.5} />}
            Anyone in {install.team_name} can start agents with your access and answer their approvals.
          </p>
        </div>
      </div>
      {error && (
        <p role="alert" className="mt-2 text-[12px] text-danger">
          {error}
        </p>
      )}
    </li>
  );
}

// ── GitHub ──

const MIRROR: Record<GitHubRepoLink["mirror"], { label: string; className: string; live?: boolean }> = {
  ok: { label: "Mirrored", className: "text-muted" },
  pending: { label: "Syncing", className: "text-muted", live: true },
  error: { label: "Mirror failing", className: "text-danger" },
  paused: { label: "Paused", className: "text-subtle" },
};

function MirrorState({ repo }: { repo: GitHubRepoLink }) {
  const m = repo.connected ? MIRROR[repo.mirror] : { label: "Disconnected", className: "text-subtle", live: false };
  return (
    <span className={cn("flex shrink-0 items-center gap-1.5 text-[11.5px]", m.className)}>
      {m.live && <span aria-hidden className="size-1.5 rounded-full bg-accent motion-safe:animate-pulse" />}
      {m.label}
    </span>
  );
}

export function GitHubCard({ github }: { github: GitHubIntegration | null }) {
  return (
    <SettingRow
      s={INTEGRATION_SETTINGS.github}
      note={<GitHubMark className="size-3.5 text-subtle" />}
      control={
        !github ? (
          <Checking />
        ) : github.install_url ? (
          <a href={github.install_url} target="_blank" rel="noreferrer" className={quiet}>
            Install the GitHub App
            <ExternalLink className="size-3.5" strokeWidth={1.5} />
          </a>
        ) : undefined
      }
    >
      {github && !github.configured && (
        <Hint>
          Not configured on this server. Set <Env names={GITHUB_ENV} /> on the server (docs/code/github-app.md).
        </Hint>
      )}
      {github?.configured && (
        <>
          {github.repos.length > 0 ? (
            <ul className="mt-3 divide-y divide-line rounded-lg border border-line-2 bg-panel-2" aria-label="Connected repos">
              {github.repos.map((r) => (
                <li key={r.repo} className="flex items-center gap-2 px-3 py-2 text-[12.5px]">
                  <code className="truncate font-mono text-[12px]">{r.repo}</code>
                  <span className="text-faint" aria-hidden>
                    →
                  </span>
                  {r.url ? (
                    <a href={r.url} target="_blank" rel="noreferrer" className="min-w-0 flex-1 truncate font-mono text-[12px] text-muted hover:text-fg">
                      {r.github}
                    </a>
                  ) : (
                    <span className="min-w-0 flex-1 truncate font-mono text-[12px] text-muted">{r.github}</span>
                  )}
                  <MirrorState repo={r} />
                </li>
              ))}
            </ul>
          ) : (
            <Hint>No repos connected to GitHub yet.</Hint>
          )}
          <Hint>Connect a repo to GitHub when you create it. Its issues, @roll comments and failed checks can then start agents.</Hint>
        </>
      )}
    </SettingRow>
  );
}
