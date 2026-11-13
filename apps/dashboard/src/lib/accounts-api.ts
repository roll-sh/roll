/**
 * Client for roll's accounts (`crates/roll-accounts`, docs/accounts.md): sign-in by email code,
 * GitHub or Google, the second factor, your account, and (admins) invites and users. The session
 * is an httpOnly cookie the API sets; this side only ever sees its CSRF token. Change both together.
 */

import { useEffect, useSyncExternalStore } from "react";
import { notifyTokenChanged, setCsrf, withSession } from "./code-token";
import { ROLL_API } from "./roll-api";

export type Provider = "github" | "google";

export interface User {
  id: string;
  email: string;
  handle: string;
  name: string;
  admin: boolean;
  disabled: boolean;
  created_at: number;
  totp: boolean;
  identities: { provider: Provider; login: string; email: string }[];
}

export interface Session {
  id: string;
  created_at: number;
  seen_at: number;
  expires_at: number;
  user_agent: string;
  current: boolean;
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
  | { status: "mfa_required"; methods: ("totp" | "recovery")[]; csrf: string }
  | { status: "signed_in"; user: User; session: Session; csrf: string };

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

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
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

export const accountsApi = {
  info: () => call<{ providers: { email: boolean; github: boolean; google: boolean } }>("GET", ""),
  session: () => call<SessionState>("GET", "/session"),
  peekInvite: (token: string) => call<{ email: string; expires_at: number }>("POST", "/invites/peek", { token }),
  emailStart: (email: string, invite?: string) => call<{ challenge: string }>("POST", "/email/start", { email, invite }),
  emailVerify: (challenge: string, code: string) => call<SignInAnswer>("POST", "/email/verify", { challenge, code }),
  oauthStart: (provider: Provider, opts: { intent?: "signin" | "link"; invite?: string; return_to?: string } = {}) =>
    call<{ url: string }>("POST", `/oauth/${provider}/start`, opts),
  signout: () => call<null>("POST", "/signout"),
  totpSignin: (code: string) => call<SignInAnswer>("POST", "/signin/totp", { code }),
  recoverySignin: (code: string) => call<SignInAnswer>("POST", "/signin/recovery", { code }),

  me: () => call<User>("GET", "/me"),
  updateMe: (name: string) => call<User>("PATCH", "/me", { name }),
  sessions: () => call<Session[]>("GET", "/me/sessions"),
  revokeSession: (id: string) => call<null>("DELETE", `/me/sessions/${encodeURIComponent(id)}`),
  unlink: (provider: Provider) => call<null>("DELETE", `/me/identities/${provider}`),
  totpBegin: () => call<{ secret: string; otpauth_url: string; qr_png: string }>("POST", "/me/totp"),
  totpConfirm: (code: string) => call<{ recovery_codes: string[] }>("POST", "/me/totp/confirm", { code }),
  totpDisable: (code: string) => call<null>("DELETE", "/me/totp", { code }),
  newRecoveryCodes: (code: string) => call<{ recovery_codes: string[] }>("POST", "/me/recovery-codes", { code }),

  invites: () => call<Invite[]>("GET", "/invites"),
  invite: (email: string, opts: { admin?: boolean; handle?: string } = {}) => call<Invite & { token: string; url: string }>("POST", "/invites", { email, ...opts }),
  revokeInvite: (id: string) => call<null>("DELETE", `/invites/${encodeURIComponent(id)}`),
  users: () => call<User[]>("GET", "/users"),
  updateUser: (id: string, patch: { admin?: boolean; disabled?: boolean }) => call<User>("PATCH", `/users/${encodeURIComponent(id)}`, patch),
  signOutUser: (id: string) => call<null>("DELETE", `/users/${encodeURIComponent(id)}/sessions`),
};

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
