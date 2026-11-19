/**
 * Client for roll's accounts (`crates/roll-accounts`, docs/accounts.md): sign-in by email code,
 * GitHub or Google, the second factor, your account, and (admins) invites and users. The session
 * is an httpOnly cookie the API sets; this side only ever sees its CSRF token. Change both together.
 */

import { useEffect, useSyncExternalStore } from "react";
import { notifyTokenChanged, setCsrf, withSession } from "./code-token";
import { ROLL_API } from "./roll-api";
import { withReauth } from "./reauth";

export type Provider = "github" | "google";

export interface User {
  id: string;
  email: string;
  handle: string;
  name: string;
  admin: boolean;
  disabled: boolean;
  created_at: number;
  /** When they last used roll (a sign-in or a session's use); `null`: not since roll began keeping it. */
  last_active_at?: number | null;
  totp: boolean;
  /** How many passkeys they have (with `totp`: whether they have a second factor). */
  passkeys?: number;
  /** GitHub, Google, or an SSO connection's link (`sso:<id>`). */
  identities: { provider: Provider | `sso:${string}`; login: string; email: string }[];
}

export interface Session {
  id: string;
  created_at: number;
  seen_at: number;
  expires_at: number;
  user_agent: string;
  current: boolean;
  /** How it signed in (absent: sessions from before roll recorded it). */
  method?: "email" | "github" | "google" | "sso" | "passkey" | null;
  /** Whether a passkey was used in it: with `sso`, a phishing-resistant session. */
  passkey?: boolean;
}

const SIGNED_IN_WITH: Record<string, string> = { email: "an email code", github: "GitHub", google: "Google", sso: "your company's sign-in", passkey: "a passkey" };

/** How a session signed in, in words (`null`: not recorded). */
export function sessionMethod(s: Pick<Session, "method">): string | null {
  return s.method ? (SIGNED_IN_WITH[s.method] ?? s.method) : null;
}

/** Whether a session is phishing-resistant (docs/accounts.md): it used a passkey, or came from SSO. */
export function phishingResistant(s: Pick<Session, "method" | "passkey">): boolean {
  return Boolean(s.passkey) || s.method === "sso" || s.method === "passkey";
}

export interface Invite {
  id: string;
  email: string;
  admin: boolean;
  handle: string | null;
  created_by: string;
  created_at: number;
  expires_at: number;
  used_at: number | null;
  used_by: string | null;
}

export type SessionState =
  | { status: "anonymous" }
  | { status: "mfa_required"; methods: ("totp" | "recovery" | "passkey")[]; csrf: string }
  | { status: "signed_in"; user: User; session: Session; csrf: string; second_factor_required?: boolean; passkey_required?: boolean };

export type SignInAnswer = Exclude<SessionState, { status: "anonymous" }>;

/** An error from the accounts API, with its `code` (`bad_code`, `invite_invalid`, `csrf`…). */
export class AccountsError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code?: string,
  ) {
    super(message);
  }
}

export { onReauthRequired } from "./reauth";

/** A call, asking the person to confirm it's them when sudo mode wants it (`lib/reauth.ts`). */
function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  return withReauth(() => callOnce<T>(method, path, body));
}

async function callOnce<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(
    `${ROLL_API}/v1/accounts${path}`,
    withSession({
      method,
      headers: body === undefined ? undefined : { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: "no-store",
    }),
  );
  const text = await res.text();
  let data: { error?: string; code?: string } | null = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    // Not the API's JSON (a proxy's page): the status says enough.
  }
  if (!res.ok) throw new AccountsError(data?.error ?? `${res.status} ${res.statusText}`.trim(), res.status, data?.code);
  return data as T;
}

/** One line of your security log (docs/accounts.md, "Audit"). */
export interface Activity {
  at: number;
  /** `sign_in`, `user.create`, `identity.link`, `scim.update`, or `METHOD /route`. */
  action: string;
  /** You (your id), `operator`, an admin's id, or `scim:<connection>`. */
  actor: string;
  ok: boolean;
  status: number;
  ip?: string | null;
  user_agent?: string | null;
  detail?: { method?: string; refused?: string | null; second_factor?: string | null; via?: string; set?: Record<string, unknown>; active?: boolean | null } | null;
}

const METHODS: Record<string, string> = { email: "an email code", github: "GitHub", google: "Google", totp: "your authenticator app", recovery_code: "a recovery code", passkey: "a passkey" };

/** A security log line in words: what happened, by whom (`me`: your own id). */
export function activityText(e: Activity, me: string): string {
  const d = e.detail ?? {};
  const how = (m?: string) => (m ? (METHODS[m] ?? (m.startsWith("sso:") ? "your company's sign-in" : m)) : "");
  const by = e.actor === me ? "you" : e.actor === "operator" ? "the operator" : e.actor.startsWith("scim:") ? "your company's provider (SCIM)" : "an admin";
  switch (e.action) {
    case "sign_in":
      if (!e.ok) return `A sign-in with ${how(d.method)} was refused${d.refused ? ` (${d.refused.replace(/_/g, " ")})` : ""}`;
      return d.second_factor === "owed" ? `Signed in with ${how(d.method)}, then asked for your second factor` : `Signed in with ${how(d.method)}`;
    case "user.create":
      return d.via === "provisioned" ? "Your account was made by your company's sign-in" : "Your account was made from your invite";
    case "identity.link":
      return e.ok ? `${how(d.method) || "A sign-in"} was linked to your account` : `Linking ${how(d.method)} was refused`;
    case "account.export":
      return "You exported your data";
    case "recovery.request":
      return "You asked an admin to reset your second factor";
    case "recovery.approve":
      return `Your second factor was reset by ${by}`;
    case "recovery.deny":
      return `${by === "you" ? "You" : by[0].toUpperCase() + by.slice(1)} declined to reset your second factor`;
    case "reauth":
      return e.ok ? `You confirmed it's you with ${how(d.method)}` : `Confirming it's you with ${how(d.method)} was refused`;
    case "session.end":
      return "Your company's provider signed you out (its back-channel logout)";
    case "scim.update":
      return d.active === false ? "Your company's provider deactivated your account" : d.active === true ? "Your company's provider reactivated your account" : "Your company's provider updated your account";
  }
  // A change that was refused (sudo mode asking, a wrong code) says so, never what it would have done.
  const was = (done: string, tried: string) => (e.ok ? done : `${tried} was refused`);
  const set = d.set ?? {};
  if ("disabled" in set) return was(`Your account was ${set.disabled ? "disabled" : "turned back on"} by ${by}`, `${set.disabled ? "Disabling" : "Turning back on"} your account`);
  if ("admin" in set) return was(`${by === "you" ? "You" : by[0].toUpperCase() + by.slice(1)} ${set.admin ? "made you an admin" : "removed your admin rights"}`, set.admin ? "Making you an admin" : "Removing your admin rights");
  const verb = e.action.split(" ")[0];
  if (e.action.includes("/me/totp")) return verb === "DELETE" ? was("Your authenticator app was turned off", "Turning off your authenticator app") : was("Your authenticator app was set up", "Setting up an authenticator app");
  if (e.action.includes("/me/recovery-codes")) return was("New recovery codes were made", "Making new recovery codes");
  if (e.action.includes("/me/passkeys")) return verb === "DELETE" ? was("A passkey was removed", "Removing a passkey") : verb === "PATCH" ? was("A passkey was renamed", "Renaming a passkey") : was("A passkey was added", "Adding a passkey");
  if (e.action.includes("/me/identities")) return was("A sign-in method was unlinked", "Unlinking a sign-in method");
  if (e.action.includes("/me/sessions") || e.action.endsWith("/sessions")) return was(`Sessions were ended by ${by}`, "Ending sessions");
  if (e.action.endsWith("/me/email/confirm")) return was("Your account moved to another address", "Moving your account to another address");
  if (e.action.endsWith("/me/email")) return was("A move to another address was started", "Starting a move to another address");
  if (e.action === "PATCH /v1/accounts/me") return was("Your profile was changed", "A change to your profile");
  return e.ok ? `${e.action} by ${by}` : `${e.action} by ${by} was refused`;
}

/** A CSV cell: quoted, and a formula character defused with `'` at its start and after a `;`, tab
 * or `|` (OWASP's CSV injection advice, as roll's audit export does): names and addresses are
 * people's own text. */
function csvCell(v: string): string {
  let out = "";
  let start = true;
  for (const c of v) {
    if (start && "=+-@\t\r".includes(c)) out += "'";
    out += c;
    if (";\t|".includes(c)) start = true;
    else if (c !== " ") start = false;
  }
  return `"${out.replace(/"/g, '""')}"`;
}

/** People as CSV for a spreadsheet (Okta's and Google Workspace's people export): one row each. */
export function peopleCsv(list: User[]): string {
  const iso = (ms?: number | null) => (ms == null ? "" : new Date(ms).toISOString());
  const head = ["handle", "name", "email", "admin", "turned_off", "authenticator_app", "passkeys", "dormant", "last_active", "created"];
  const rows = list.map((u) => [u.handle, u.name, u.email, String(u.admin), String(u.disabled), String(u.totp), String(u.passkeys ?? 0), String(dormant(u)), iso(u.last_active_at), iso(u.created_at)]);
  return [head.join(","), ...rows.map((r) => r.map(csvCell).join(","))].join("\n") + "\n";
}

/** People's filters in the admin list. */
export type PeopleFilter = "everyone" | "admins" | "no-second-factor" | "turned-off";

/** The people matching `query` (name, handle or address, any case) and `filter`. */
export function findPeople<U extends Pick<User, "name" | "handle" | "email" | "admin" | "disabled" | "totp" | "passkeys">>(list: U[], query: string, filter: PeopleFilter): U[] {
  const q = query.trim().toLowerCase();
  return list.filter((u) => {
    if (q && ![u.name, u.handle, u.email].some((s) => s.toLowerCase().includes(q))) return false;
    switch (filter) {
      case "admins":
        return u.admin;
      case "no-second-factor":
        return !u.disabled && !u.totp && !u.passkeys;
      case "turned-off":
        return u.disabled;
      default:
        return true;
    }
  });
}

/** How long without using roll makes someone dormant (GitHub Enterprise's 90 days). */
export const DORMANT_DAYS = 90;

/** Whether `u` hasn't used roll (a sign-in, a session or one of their tokens) in DORMANT_DAYS. Unknown
 * activity (`null`: not since roll began keeping it) isn't dormant. */
export function dormant(u: Pick<User, "last_active_at" | "created_at" | "disabled">, now = Date.now()): boolean {
  return !u.disabled && u.last_active_at != null && u.last_active_at < now - DORMANT_DAYS * 24 * 3600 * 1000;
}

/** Which sign-ins this roll offers (`sso`: a company's provider answers for some domains). */
export type Providers = { email: boolean; github: boolean; google: boolean; sso?: boolean; passkey?: boolean };

/** A second-factor reset someone asked an admin for (docs/accounts.md, "Recovery"). */
export interface ResetRequest {
  id: string;
  user: string;
  email: string;
  handle: string;
  reason: string;
  ip: string | null;
  user_agent: string | null;
  created_at: number;
  state: "pending" | "approved" | "denied";
  decided_by: string | null;
  decided_at: number | null;
}

/** The company's account policy (docs/accounts.md): the whole of it goes back on a change. */
export interface AccountPolicy {
  require_second_factor: boolean;
  /** A session unused this many minutes ends (15 to a week; absent: a week). */
  idle_minutes?: number;
  /** A session ends this many hours after its sign-in, however much it's used (1 to 720; absent: 30 days). */
  max_session_hours?: number;
  /** Whose sessions must use a passkey (or come from their SSO): phishing-resistant sign-in. */
  phishing_resistant?: "admins" | "everyone";
}

/** Allowed networks (docs/accounts.md): where people's sessions may be used from. */
export interface NetworkPolicy {
  on: boolean;
  networks: string[];
}

/** A passkey of yours (never its key): `synced` when your devices share it (iCloud, Google). */
export interface Passkey {
  id: string;
  name: string;
  created_at: number;
  last_used_at: number | null;
  synced: boolean;
  /** Its model, when a verified attestation proved it (hardware keys only). */
  model?: string;
}

/** Hardware keys only (docs/accounts.md, "Passkeys"): which authenticator models count. */
export interface PasskeyPolicy {
  require: boolean;
  models: { aaguid: string; name: string; roots: string[] }[];
}

/** A passkey ceremony's options: its challenge's `id`, and what `navigator.credentials` takes (as JSON). */
export interface PasskeyOptions<T> {
  id: string;
  publicKey: T;
}

/** A company's OpenID Connect provider (docs/accounts.md, "SSO"): never its secret. */
export interface SsoConnection {
  id: string;
  name: string;
  issuer: string;
  client_id: string;
  domains: string[];
  /** People of these domains sign in only through it. */
  enforce: boolean;
  /** A verified person of these domains with no account gets one (never an admin). */
  provision: boolean;
  created_at: number;
  /** What to register at the provider as the sign-in redirect. */
  redirect_uri: string;
  /** What to register at the provider as its back-channel logout URI (it ends sessions there). */
  backchannel_logout_uri: string;
  /** Domains a TXT record proved: only these route sign-ins, link, provision or enforce. */
  verified: string[];
  /** The TXT record that proves a domain (`name` with the domain in place of `<domain>`). */
  dns: { name: string; value: string };
  /** Its provider provisions people over SCIM (it has a live token). */
  scim: boolean;
  /** Its SCIM token was found leaked and revoked: where (until an admin makes a new one). */
  scim_leaked: string | null;
  /** How long a session it signs in lasts at most, in hours (null: roll's 30 days). */
  session_hours: number | null;
}

export const accountsApi = {
  info: () => call<{ providers: Providers }>("GET", ""),
  /** Sign in through the SSO connection for the email's domain (`no_sso` when none answers). */
  ssoStart: (email: string, opts: { invite?: string; return_to?: string } = {}) => call<{ url: string }>("POST", "/sso/start", { email, ...opts }),
  /** Link the signed-in account through the SSO connection for its own email. */
  ssoLink: () => call<{ url: string }>("POST", "/sso/start", { intent: "link", return_to: "/dashboard/account" }),
  session: () => call<SessionState>("GET", "/session"),
  peekInvite: (token: string) => call<{ email: string; expires_at: number }>("POST", "/invites/peek", { token }),
  emailStart: (email: string, invite?: string) => call<{ challenge: string }>("POST", "/email/start", { email, invite }),
  emailVerify: (challenge: string, code: string) => call<SignInAnswer>("POST", "/email/verify", { challenge, code }),
  oauthStart: (provider: Provider, opts: { intent?: "signin" | "link"; invite?: string; return_to?: string } = {}) =>
    call<{ url: string }>("POST", `/oauth/${provider}/start`, opts),
  signout: () => call<null>("POST", "/signout"),
  totpSignin: (code: string) => call<SignInAnswer>("POST", "/signin/totp", { code }),
  recoverySignin: (code: string) => call<SignInAnswer>("POST", "/signin/recovery", { code }),
  passkeySigninOptions: () => call<PasskeyOptions<{ challenge: string; rpId: string; allowCredentials?: { type: "public-key"; id: string }[] }>>("POST", "/signin/passkey/options"),
  passkeySignin: (id: string, credential: unknown) => call<SignInAnswer>("POST", "/signin/passkey", { id, credential }),

  me: () => call<User>("GET", "/me"),
  /** Your own security log: sign-ins and changes to your account, newest first. */
  activity: (before?: number) => call<{ events: Activity[] }>("GET", `/me/activity${before ? `?before=${before}` : ""}`),
  updateMe: (name: string) => call<User>("PATCH", "/me", { name }),
  /** Move your account to `email`: a code goes there (docs/accounts.md, "Your address"). */
  changeEmail: (email: string) => call<{ challenge: string }>("POST", "/me/email", { email }),
  confirmEmail: (challenge: string, code: string) => call<User>("POST", "/me/email/confirm", { challenge, code }),
  /** Delete your own account for good, `confirm` being your handle (docs/accounts.md, "Deleting your account"). */
  deleteMe: (confirm: string) => call<null>("DELETE", "/me", { confirm }),
  sessions: () => call<Session[]>("GET", "/me/sessions"),
  revokeSession: (id: string) => call<null>("DELETE", `/me/sessions/${encodeURIComponent(id)}`),
  /** End every session but this browser's. */
  endOtherSessions: () => call<{ ended: number }>("DELETE", "/me/sessions"),
  unlink: (provider: Provider | `sso:${string}`) => call<null>("DELETE", `/me/identities/${encodeURIComponent(provider)}`),
  totpBegin: () => call<{ secret: string; otpauth_url: string; qr_png: string }>("POST", "/me/totp"),
  totpConfirm: (code: string) => call<{ recovery_codes: string[] }>("POST", "/me/totp/confirm", { code }),
  totpDisable: (code: string) => call<null>("DELETE", "/me/totp", { code }),
  newRecoveryCodes: (code: string) => call<{ recovery_codes: string[] }>("POST", "/me/recovery-codes", { code }),
  passkeys: () => call<{ passkeys: Passkey[] }>("GET", "/me/passkeys").then((r) => r.passkeys),
  passkeyOptions: () =>
    call<PasskeyOptions<{ challenge: string; user: { id: string; name: string; displayName: string }; excludeCredentials?: { type: "public-key"; id: string }[] }>>("POST", "/me/passkeys/options"),
  addPasskey: (id: string, name: string, credential: unknown) => call<Passkey>("POST", "/me/passkeys", { id, name, credential }),
  renamePasskey: (id: string, name: string) => call<Passkey>("PATCH", `/me/passkeys/${encodeURIComponent(id)}`, { name }),
  passkeyPolicy: () => call<PasskeyPolicy>("GET", "/passkey-policy"),
  setPasskeyPolicy: (p: PasskeyPolicy) => call<PasskeyPolicy>("PUT", "/passkey-policy", p),
  removePasskey: (id: string) => call<null>("DELETE", `/me/passkeys/${encodeURIComponent(id)}`),
  /** Your data as one JSON document (sudo mode): a file to save. */
  exportData: () => call<Record<string, unknown>>("POST", "/me/export"),
  accountPolicy: () => call<AccountPolicy>("GET", "/policy"),
  setAccountPolicy: (p: AccountPolicy) => call<AccountPolicy>("PUT", "/policy", p),
  networkPolicy: () => call<NetworkPolicy>("GET", "/network-policy"),
  setNetworkPolicy: (p: NetworkPolicy) => call<NetworkPolicy>("PUT", "/network-policy", p),
  requestReset: (reason: string) => call<{ id: string; state: string }>("POST", "/signin/reset-request", { reason }),
  resetRequests: () => call<ResetRequest[]>("GET", "/reset-requests"),
  decideReset: (id: string, approve: boolean) => call<ResetRequest>("POST", `/reset-requests/${encodeURIComponent(id)}/${approve ? "approve" : "deny"}`),
  reauthInfo: () => call<{ fresh_until: number; methods: ("totp" | "passkey" | "email")[] }>("GET", "/me/reauth"),
  reauthEmail: () => call<{ challenge: string }>("POST", "/me/reauth/email"),
  reauthPasskeyOptions: () => call<PasskeyOptions<{ challenge: string; rpId: string; allowCredentials?: { type: "public-key"; id: string }[] }>>("POST", "/me/reauth/passkey/options"),
  reauth: (proof: { code: string } | { challenge: string; code: string } | { id: string; credential: unknown }) => call<{ fresh_until: number }>("POST", "/me/reauth", proof),

  invites: () => call<Invite[]>("GET", "/invites"),
  invite: (email: string, opts: { admin?: boolean; handle?: string } = {}) => call<Invite & { token: string; url: string }>("POST", "/invites", { email, ...opts }),
  revokeInvite: (id: string) => call<null>("DELETE", `/invites/${encodeURIComponent(id)}`),
  /** A fresh link for an unused invite (another 7 days, mailed again); the old link stops. */
  resendInvite: (id: string) => call<Invite & { token: string; url: string }>("POST", `/invites/${encodeURIComponent(id)}/resend`),
  users: () => call<User[]>("GET", "/users"),
  updateUser: (id: string, patch: { admin?: boolean; disabled?: boolean }) => call<User>("PATCH", `/users/${encodeURIComponent(id)}`, patch),
  signOutUser: (id: string) => call<null>("DELETE", `/users/${encodeURIComponent(id)}/sessions`),
  /** Admins: a person's security log and sessions, and ending one of them. */
  userActivity: (id: string, before?: number) => call<{ events: Activity[] }>("GET", `/users/${encodeURIComponent(id)}/activity${before ? `?before=${before}` : ""}`),
  userSessions: (id: string) => call<Session[]>("GET", `/users/${encodeURIComponent(id)}/sessions`),
  endUserSession: (id: string, session: string) => call<null>("DELETE", `/users/${encodeURIComponent(id)}/sessions/${encodeURIComponent(session)}`),

  ssoConnections: () => call<SsoConnection[]>("GET", "/sso"),
  addSso: (c: { name?: string; issuer: string; client_id: string; client_secret: string; domains: string[]; enforce?: boolean; provision?: boolean }) =>
    call<SsoConnection>("POST", "/sso", c),
  updateSso: (id: string, c: { name?: string; client_id?: string; client_secret?: string; domains?: string[]; enforce?: boolean; provision?: boolean; session_hours?: number }) =>
    call<SsoConnection>("PATCH", `/sso/${encodeURIComponent(id)}`, c),
  removeSso: (id: string) => call<null>("DELETE", `/sso/${encodeURIComponent(id)}`),
  /** A SCIM token for the connection (replacing its old one), shown this once. */
  scimToken: (id: string) => call<{ token: string; base_url: string }>("POST", `/sso/${encodeURIComponent(id)}/scim`),
  scimOff: (id: string) => call<null>("DELETE", `/sso/${encodeURIComponent(id)}/scim`),
  /** Check the connection's domains' TXT records now. */
  verifySso: (id: string) => call<SsoConnection>("POST", `/sso/${encodeURIComponent(id)}/verify`),
};

/** "acme.com, @acme.co.uk" as domains, or what's wrong with it (the API checks again). */
export function ssoDomains(text: string): { domains: string[] } | { problem: string } {
  const domains = [...new Set(text.split(/[\s,]+/).map((d) => d.trim().toLowerCase().replace(/^@/, "")).filter(Boolean))];
  if (domains.length === 0) return { problem: "Name the email domains it answers for" };
  const bad = domains.find((d) => !/^[a-z0-9-]+(\.[a-z0-9-]+)+$/.test(d));
  return bad ? { problem: `${bad} isn't a domain` } : { domains };
}

// ── the session, for every component ──

type Loaded = SessionState | { status: "loading" } | { status: "offline"; error: string };

let state: Loaded = { status: "loading" };
let loading: Promise<void> | null = null;
const listeners = new Set<() => void>();

function set(next: Loaded) {
  state = next;
  setCsrf("csrf" in next ? next.csrf : null);
  for (const l of listeners) l();
}

/** Ask the API who's signed in (again after a sign-in or out), and tell everything that cares. */
export function refreshSession(): Promise<void> {
  loading ??= accountsApi
    .session()
    .then(
      (s) => {
        const was = state.status === "signed_in" ? state.user.id : state.status;
        set(s);
        // The agents, the owner and roll code resolve the caller again when it changed.
        if (was !== "loading" && (s.status === "signed_in" ? s.user.id : s.status) !== was) notifyTokenChanged();
      },
      // A rolld without accounts (older) or out of reach: as signed out, with why.
      (e) => set(e instanceof AccountsError && e.status === 404 ? { status: "anonymous" } : { status: "offline", error: e instanceof Error ? e.message : String(e) }),
    )
    .finally(() => (loading = null));
  return loading;
}

/** Take an answer from a sign-in route as the session now. */
export function acceptSignIn(answer: SignInAnswer) {
  set(answer);
  notifyTokenChanged();
}

export async function signOut() {
  await accountsApi.signout().catch(() => {});
  set({ status: "anonymous" });
  notifyTokenChanged();
}

const subscribe = (l: () => void) => {
  listeners.add(l);
  return () => listeners.delete(l);
};
const server: Loaded = { status: "loading" };

/** Who's signed in (loading it the first time anything asks). */
export function useSession(): Loaded {
  const s = useSyncExternalStore(subscribe, () => state, () => server);
  useEffect(() => {
    if (state.status === "loading" && !loading) void refreshSession();
  }, []);
  return s;
}
