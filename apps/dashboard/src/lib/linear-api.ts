/**
 * The owner's Linear workspace: the wire types of `GET /v1/code/agents/{owner}/linear` and the
 * owner routes around it (docs/code/http-api.md, "Linear"). Tokens and signing secrets never come
 * here: only whether they're set (`secret_hint` is a secret's last four characters).
 *
 * `memoryLinear` answers the same interface in memory, for the demo and tests.
 */
import type { Millis } from "./agents-api";
import { ApiError } from "./code-api";
import { array, bool, decode, int, literal, map, nullable, obj, optional, str, withDefault } from "./decode";
import type { Call } from "./automations-api";

/** What the server has of roll's Linear app. */
export interface LinearApp {
  /** `ROLL_LINEAR_CLIENT_ID`/`ROLL_LINEAR_CLIENT_SECRET`: owners can install roll as Linear's agent. */
  oauth: boolean;
  /** `ROLL_LINEAR_WEBHOOK_SECRET`: the app's deliveries can be checked. */
  webhook: boolean;
  webhook_url: string;
  redirect_uri: string | null;
  /** `ROLL_SECRET_KEY`: needed to keep any token (both ways of connecting). */
  secret_key: boolean;
}

export interface LinearWorkspace {
  id: string;
  name: string;
  url_key: string;
  url: string;
}

export interface LinearUser {
  id: string;
  name: string;
}

/** Which roll code repo a team's issues are in (`event.data.repo`). */
export interface LinearSettings {
  /** Team key → `repo` or `owner/repo`. */
  repos: Record<string, string>;
  default_repo?: string;
}

/** `app`: roll's OAuth app delivers; `workspace`: roll made a webhook with the API key; `manual`: the owner makes one. */
export type LinearWebhookMode = "app" | "workspace" | "manual";
export type LinearWebhookStatus = "ok" | "waiting" | "failing" | "needs_secret";

export interface LinearWebhook {
  mode: LinearWebhookMode;
  url: string;
  status: LinearWebhookStatus;
  secret_hint: string | null;
  last_delivery_at: Millis | null;
  rejected_at: Millis | null;
  rejected_why: string | null;
}

export type LinearKind = "oauth" | "api_key";

export interface LinearConnection {
  kind: LinearKind;
  workspace: LinearWorkspace;
  /** Who roll is in Linear (the app's actor, or the key's user). */
  user: LinearUser;
  /** roll can be assigned and mentioned (installed as an agent). */
  agent: boolean;
  scopes: string[];
  /** `reauth`: Linear stopped taking the token; connect again. */
  status: "ok" | "reauth";
  error: string | null;
  expires_at: Millis | null;
  created_at: Millis;
  created_by: string;
  settings: LinearSettings;
  webhook: LinearWebhook;
}

export type LinearDeliveryResult = "recorded" | "duplicate" | "ignored";

export interface LinearDelivery {
  id: string;
  at: Millis;
  /** Linear's delivery id. */
  delivery: string;
  /** `Issue`, `Comment`, `AgentSessionEvent`, … */
  type: string;
  action: string;
  summary: string;
  result: LinearDeliveryResult;
  /** Automation events it made. */
  events: number;
}

export interface LinearStatus {
  connected: boolean;
  app: LinearApp;
  connection: LinearConnection | null;
  /** Newest first. */
  deliveries: LinearDelivery[];
}

export interface LinearTeam {
  id: string;
  key: string;
  name: string;
}

// ── decoders ──

const strs = withDefault(array(str), () => []);

export const linearAppD = obj<LinearApp>({
  oauth: withDefault(bool, () => false),
  webhook: withDefault(bool, () => false),
  webhook_url: withDefault(str, () => ""),
  redirect_uri: nullable(str),
  secret_key: withDefault(bool, () => false),
});

export const linearSettingsD = obj<LinearSettings>({
  repos: withDefault(map(str), () => ({})),
  default_repo: optional(str),
});

export const linearWebhookD = obj<LinearWebhook>({
  mode: literal("app", "workspace", "manual"),
  url: str,
  status: literal("ok", "waiting", "failing", "needs_secret"),
  secret_hint: nullable(str),
  last_delivery_at: nullable(int),
  rejected_at: nullable(int),
  rejected_why: nullable(str),
});

export const linearConnectionD = obj<LinearConnection>({
  kind: literal("oauth", "api_key"),
  workspace: obj<LinearWorkspace>({ id: str, name: str, url_key: str, url: str }),
  user: obj<LinearUser>({ id: str, name: str }),
  agent: withDefault(bool, () => false),
  scopes: strs,
  status: withDefault(literal("ok", "reauth"), () => "ok" as const),
  error: nullable(str),
  expires_at: nullable(int),
  created_at: int,
  created_by: withDefault(str, () => ""),
  settings: withDefault(linearSettingsD, () => ({ repos: {} })),
  webhook: linearWebhookD,
});

export const linearDeliveryD = obj<LinearDelivery>({
  id: str,
  at: int,
  delivery: withDefault(str, () => ""),
  type: str,
  action: withDefault(str, () => ""),
  summary: withDefault(str, () => ""),
  result: literal("recorded", "duplicate", "ignored"),
  events: withDefault(int, () => 0),
});

export const linearStatusD = obj<LinearStatus>({
  connected: bool,
  app: linearAppD,
  connection: nullable(linearConnectionD),
  deliveries: withDefault(array(linearDeliveryD), () => []),
});

export const linearTeamsD = obj<{ teams: LinearTeam[] }>({
  teams: withDefault(array(obj<LinearTeam>({ id: str, key: str, name: str })), () => []),
});

// ── the client ──

/** `/v1/code/agents/{owner}/linear…`. */
export interface LinearApi {
  getLinear(owner: string): Promise<LinearStatus>;
  /** Linear's authorize URL; it comes back to `returnTo` (a dashboard path) with `?linear=…`. */
  connectOAuth(owner: string, returnTo: string): Promise<string>;
  /** Connect with a personal API key (checked with Linear first). */
  putKey(owner: string, key: string): Promise<LinearStatus>;
  /** Replaces the settings whole (a missing `default_repo` clears it). */
  putSettings(owner: string, settings: LinearSettings): Promise<LinearStatus>;
  /** A hand-made webhook's signing secret (API keys only). */
  putWebhookSecret(owner: string, secret: string): Promise<LinearStatus>;
  getTeams(owner: string): Promise<LinearTeam[]>;
  disconnect(owner: string): Promise<void>;
}

const enc = encodeURIComponent;

export function createLinearApi(call: Call): LinearApi {
  const base = (owner: string) => `/v1/code/agents/${enc(owner)}/linear`;
  return {
    getLinear: async (owner) => decode(linearStatusD, await call("GET", base(owner))),
    connectOAuth: async (owner, returnTo) => decode(obj<{ url: string }>({ url: str }), await call("POST", `${base(owner)}/oauth`, { return_to: returnTo })).url,
    putKey: async (owner, key) => decode(linearStatusD, await call("PUT", `${base(owner)}/key`, { key })),
    putSettings: async (owner, settings) => decode(linearStatusD, await call("PUT", `${base(owner)}/settings`, settings)),
    putWebhookSecret: async (owner, secret) => decode(linearStatusD, await call("PUT", `${base(owner)}/webhook/secret`, { secret })),
    getTeams: async (owner) => decode(linearTeamsD, await call("GET", `${base(owner)}/teams`)).teams,
    disconnect: async (owner) => {
      await call("DELETE", base(owner));
    },
  };
}

// ── in memory ──

/** Linear in memory (the demo, tests): a key connects (webhook `manual` until a secret is sent), OAuth connects at once. */
export function memoryLinear(initial: LinearStatus, teams: LinearTeam[] = []): LinearApi {
  let status: LinearStatus = structuredClone(initial);
  const copy = () => structuredClone(status);
  const connection = () => {
    if (!status.connection) throw new ApiError("Linear isn't connected", 404, null);
    return status.connection;
  };
  const connect = (kind: LinearKind): LinearConnection => ({
    kind,
    workspace: { id: "org_demo", name: "Acme", url_key: "acme", url: "https://linear.app/acme" },
    user: { id: "usr_demo", name: kind === "oauth" ? "roll" : "Ada Lovelace" },
    agent: kind === "oauth",
    scopes: kind === "oauth" ? ["read", "write", "issues:create", "comments:create", "app:assignable", "app:mentionable"] : [],
    status: "ok",
    error: null,
    expires_at: null,
    created_at: Date.now(),
    created_by: "owner",
    settings: { repos: {} },
    webhook: {
      mode: kind === "oauth" ? "app" : "manual",
      url: kind === "oauth" ? status.app.webhook_url : `${status.app.webhook_url}/lwh_demo`,
      status: kind === "oauth" ? (status.app.webhook ? "waiting" : "needs_secret") : "needs_secret",
      secret_hint: null,
      last_delivery_at: null,
      rejected_at: null,
      rejected_why: null,
    },
  });
  const set = (c: LinearConnection | null) => {
    status = { ...status, connected: c !== null, connection: c, deliveries: c ? status.deliveries : [] };
  };
  return {
    getLinear: async () => copy(),
    connectOAuth: async (_owner, returnTo) => {
      if (!status.app.oauth || !status.app.secret_key) throw new ApiError("roll's Linear app isn't configured on this server", 422, null);
      set(connect("oauth"));
      // Where the callback sends the browser: `return_to` with its answer appended, as the server does.
      return `${returnTo}${returnTo.includes("?") ? "&" : "?"}linear=connected`;
    },
    putKey: async (_owner, key) => {
      if (!status.app.secret_key) throw new ApiError("the server needs ROLL_SECRET_KEY", 422, null);
      if (!/^lin_api_\w{8,}$/.test(key.trim())) throw new ApiError("Linear refused the key", 400, null);
      set(connect("api_key"));
      return copy();
    },
    putSettings: async (_owner, settings) => {
      const c = connection();
      const bad = [...Object.values(settings.repos), ...(settings.default_repo === undefined ? [] : [settings.default_repo])].find((r) => !/^([\w.-]+\/)?[\w.-]+$/.test(r));
      if (bad !== undefined) throw new ApiError(`${JSON.stringify(bad)} isn't a repo (\`repo\` or \`owner/repo\`)`, 400, null);
      set({ ...c, settings: structuredClone(settings) });
      return copy();
    },
    putWebhookSecret: async (_owner, secret) => {
      const c = connection();
      if (c.kind === "oauth") throw new ApiError("an OAuth connection uses the app's signing secret", 409, null);
      if (secret.trim().length < 8) throw new ApiError("that isn't a signing secret", 400, null);
      set({ ...c, webhook: { ...c.webhook, secret_hint: secret.trim().slice(-4), status: c.webhook.last_delivery_at ? "ok" : "waiting" } });
      return copy();
    },
    getTeams: async () => {
      connection();
      return structuredClone(teams);
    },
    disconnect: async () => {
      connection();
      set(null);
    },
  };
}

/** Not connected, on a server with everything set up. */
export const DISCONNECTED: LinearStatus = {
  connected: false,
  app: { oauth: true, webhook: true, webhook_url: "https://roll.example/v1/code/linear/webhook", redirect_uri: "https://roll.example/v1/code/linear/oauth/callback", secret_key: true },
  connection: null,
  deliveries: [],
};

// ── back from OAuth ──

/** What the OAuth callback said (`?linear=connected`, or `error` and a reason). */
export type LinearReturn = { outcome: "connected" } | { outcome: "error"; reason: string };

/**
 * The callback's answer on the page's URL, and the URL without it. The server appends its query
 * to `return_to`, which ends in the agents hash, so it can land after the `#`
 * (`#agents/settings/owner?linear=connected`) as well as before it.
 */
export function linearReturn(href: string): { result: LinearReturn | null; clean: string } {
  const url = new URL(href, "http://x");
  const [view, ...rest] = url.hash.slice(1).split(/[?&]/);
  const inHash = new URLSearchParams(rest.join("&"));
  const get = (k: string) => url.searchParams.get(k) ?? inHash.get(k);
  const outcome = get("linear");
  const result: LinearReturn | null =
    outcome === "connected" ? { outcome } : outcome === "error" ? { outcome, reason: get("reason")?.trim() || "Linear didn't connect." } : null;
  // Only what the callback added goes (`?demo` stays `?demo`, not `?demo=`).
  const strip = (q: string) =>
    q
      .split("&")
      .filter((p) => p && !/^(linear|reason)(=|$)/.test(p))
      .join("&");
  const search = strip(url.search.slice(1));
  const hashQuery = strip(rest.join("&"));
  const hash = url.hash ? `#${view}${hashQuery ? `?${hashQuery}` : ""}` : "";
  return { result, clean: `${url.pathname}${search ? `?${search}` : ""}${hash}` };
}
