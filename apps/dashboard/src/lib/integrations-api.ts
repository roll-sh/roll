/**
 * Chat ops: the owner's Slack, Discord and GitHub connections for roll's agents
 * (`code-control/src/chatops/api.rs`, `/v1/code/agents/{owner}/integrations…`). The owner manages
 * them; agent tokens get a 403. The answer of `GET` is parsed leniently: a bad or missing value
 * becomes its safe default (nothing configured, nothing connected), so an older or newer server
 * never breaks the settings panel. Links from the server are only followed when they're http(s).
 */

import type { Millis } from "./agents-api";
import type { Call } from "./automations-api";

export type ChatPlatform = "slack" | "discord";
export const CHAT_PLATFORMS: readonly ChatPlatform[] = ["slack", "discord"];

/** A connected Slack workspace or Discord server (`chi_…`). */
export interface ChatInstall {
  id: string;
  team_id: string;
  team_name: string;
  bot_user_id: string;
  /** The platform user who installed it; `""` for Discord. */
  installer: string;
  /** Platform user ids allowed to drive roll from it besides the installer. */
  members: string[];
  /** Anyone in the workspace or server may. */
  anyone: boolean;
  created_at: Millis;
}

export interface ChatIntegration {
  /** The server has the platform's app credentials (`ROLL_SLACK_*`, `ROLL_DISCORD_*`). */
  configured: boolean;
  installs: ChatInstall[];
}

export type MirrorState = "ok" | "pending" | "error" | "paused";

export interface GitHubRepoLink {
  /** The roll repo (`owner/name`). */
  repo: string;
  /** Its GitHub repo (`acme/app`). */
  github: string;
  url: string | null;
  connected: boolean;
  mirror: MirrorState;
}

export interface GitHubIntegration {
  configured: boolean;
  app_slug: string | null;
  install_url: string | null;
  repos: GitHubRepoLink[];
}

export interface Integrations {
  slack: ChatIntegration;
  discord: ChatIntegration;
  github: GitHubIntegration;
  /** `ROLL_SECRET_KEY` is set (Slack's tokens are sealed with it). */
  sealing: boolean;
}

/** Who may drive roll from an install: either or both. */
export interface InstallPatch {
  members?: string[];
  anyone?: boolean;
}

export const NO_INTEGRATIONS: Integrations = {
  slack: { configured: false, installs: [] },
  discord: { configured: false, installs: [] },
  github: { configured: false, app_slug: null, install_url: null, repos: [] },
  sealing: false,
};

/** Member ids as the server takes them: letters and digits, at most 64; at most 200 of them. */
export const MAX_MEMBERS = 200;
export const isMemberId = (s: string) => /^[A-Za-z0-9]{1,64}$/.test(s);

// ── parsing ──

type Rec = Record<string, unknown>;
const rec = (v: unknown): Rec => (v && typeof v === "object" && !Array.isArray(v) ? (v as Rec) : {});
const text = (v: unknown, def = "") => (typeof v === "string" ? v : def);
const flag = (v: unknown) => v === true;
const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);

/** An http(s) URL, else `null`: a link from the server never runs script. */
export function safeUrl(v: unknown): string | null {
  if (typeof v !== "string" || !v) return null;
  try {
    const u = new URL(v);
    return u.protocol === "https:" || u.protocol === "http:" ? u.href : null;
  } catch {
    return null;
  }
}

export function parseInstall(v: unknown): ChatInstall | null {
  const r = rec(v);
  if (typeof r.id !== "string" || !r.id) return null;
  return {
    id: r.id,
    team_id: text(r.team_id),
    team_name: text(r.team_name) || text(r.team_id) || r.id,
    bot_user_id: text(r.bot_user_id),
    installer: text(r.installer),
    members: [...new Set(list(r.members).filter((m): m is string => typeof m === "string" && m.length > 0))],
    anyone: flag(r.anyone),
    created_at: typeof r.created_at === "number" && Number.isFinite(r.created_at) ? r.created_at : 0,
  };
}

function parseChat(v: unknown): ChatIntegration {
  const r = rec(v);
  return { configured: flag(r.configured), installs: list(r.installs).map(parseInstall).filter((i) => i !== null) };
}

const MIRROR: readonly MirrorState[] = ["ok", "pending", "error", "paused"];

function parseRepo(v: unknown): GitHubRepoLink | null {
  const r = rec(v);
  if (typeof r.repo !== "string" || !r.repo) return null;
  return {
    repo: r.repo,
    github: text(r.github),
    url: safeUrl(r.url),
    connected: flag(r.connected),
    mirror: MIRROR.includes(r.mirror as MirrorState) ? (r.mirror as MirrorState) : "pending",
  };
}

/** `GET …/integrations`, every bad or missing value its default. */
export function parseIntegrations(v: unknown): Integrations {
  const r = rec(v);
  const gh = rec(r.github);
  return {
    slack: parseChat(r.slack),
    discord: parseChat(r.discord),
    github: {
      configured: flag(gh.configured),
      app_slug: typeof gh.app_slug === "string" && gh.app_slug ? gh.app_slug : null,
      install_url: safeUrl(gh.install_url),
      repos: list(gh.repos).map(parseRepo).filter((x) => x !== null),
    },
    sealing: flag(r.sealing),
  };
}

// ── the deep link back from a platform ──

/**
 * Where Slack or Discord sends the owner after installing:
 * `/dashboard/agents?settings=integrations&integration=slack&connected=Acme` (or `&error=expired`).
 */
export interface IntegrationsLink {
  platform: ChatPlatform | null;
  /** The workspace or server just connected. */
  connected: string | null;
  error: string | null;
}

const ERRORS: Record<string, string> = {
  expired: "The install link expired. Connect again.",
  access_denied: "The install was cancelled.",
};

type Params = URLSearchParams | Record<string, string | string[] | undefined>;

function param(p: Params, k: string): string | null {
  const v = p instanceof URLSearchParams ? p.get(k) : p[k];
  const s = Array.isArray(v) ? v[0] : v;
  return typeof s === "string" && s.trim() ? s.trim().slice(0, 200) : null;
}

/** The link's parts; `null` unless it asks for the integrations (`settings=integrations`). */
export function parseIntegrationsLink(p: Params): IntegrationsLink | null {
  if (param(p, "settings") !== "integrations") return null;
  const platform = param(p, "integration");
  const error = param(p, "error");
  return {
    platform: CHAT_PLATFORMS.includes(platform as ChatPlatform) ? (platform as ChatPlatform) : null,
    connected: error ? null : param(p, "connected"),
    error: error ? (ERRORS[error] ?? error) : null,
  };
}

/** The query keys the link adds, to take off the URL once it's been shown. */
export const LINK_PARAMS = ["settings", "integration", "connected", "error"] as const;

// ── the client ──

const enc = encodeURIComponent;

export function createIntegrationsApi(call: Call) {
  const base = (owner: string) => `/v1/code/agents/${enc(owner)}/integrations`;
  return {
    list: async (owner: string): Promise<Integrations> => parseIntegrations(await call("GET", base(owner))),
    /** A link that installs the app for this owner (once, for ten minutes): navigate to it. 422 when the server can't. */
    install: async (owner: string, platform: ChatPlatform): Promise<{ url: string }> => {
      const url = safeUrl(rec(await call("POST", `${base(owner)}/${platform}/install`)).url);
      if (!url) throw new Error(`the server gave no install link for ${platform}`);
      return { url };
    },
    update: async (owner: string, platform: ChatPlatform, id: string, patch: InstallPatch): Promise<ChatInstall> => {
      const i = parseInstall(await call("PATCH", `${base(owner)}/${platform}/${enc(id)}`, patch));
      if (!i) throw new Error("the server's answer isn't an install");
      return i;
    },
    /** Forget it here; the app stays installed on the platform until removed there. */
    remove: async (owner: string, platform: ChatPlatform, id: string): Promise<void> => {
      await call("DELETE", `${base(owner)}/${platform}/${enc(id)}`);
    },
  };
}

export type IntegrationsApi = ReturnType<typeof createIntegrationsApi>;
