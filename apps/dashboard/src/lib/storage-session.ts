/**
 * Who the storage dashboard acts as. Two modes:
 *
 * - **Operator** (no sign-in): rolld's control plane, acting in the default account, or in the
 *   account picked here (`x-roll-account`, kept in localStorage).
 * - **Console session**: an IAM user signed in with their password (`/v1/storage/console/signin`).
 *   Every `/v1/storage/...` request then carries `Authorization: Bearer <token>` and acts as that
 *   user with their IAM permissions; operator-only routes answer 403 `OperatorOnly`, and a 401 means
 *   the session is over (it's forgotten here, and the pages send you to the sign-in page).
 *
 * `storageCall` is `roll-api`'s `call` with those headers; other products keep using `call`.
 */

import { ROLL_API } from "./roll-api";

// --- Errors ------------------------------------------------------------------------------------

/** An error answer from the storage API: its status, and IAM's / the console's `code` when it has one. */
export class StorageError extends Error {
  readonly status: number;
  readonly code: string | null;
  constructor(message: string, status: number, code: string | null = null) {
    super(message);
    this.name = "StorageError";
    this.status = status;
    this.code = code;
  }
}

/** The error's `code` (`AccessDenied`, `NoSuchEntity`, `MfaRequired`, …), or `null`. */
export function errorCode(e: unknown): string | null {
  return e instanceof StorageError ? e.code : null;
}

/** An error's message, for an ErrorNote. */
export function errorMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** Whether the request was refused because operator mode is needed (accounts, roll keys and roles…). */
export function isOperatorOnly(e: unknown) {
  return e instanceof StorageError && e.status === 403 && e.code === "OperatorOnly";
}

// --- Persisted state ---------------------------------------------------------------------------

export interface ConsoleSession {
  token: string;
  /** Unix ms. */
  expires_at: number;
  account_id: string;
  account_alias: string | null;
  user: string;
  arn: string;
  /** Signed in with an MFA code. */
  mfa: boolean;
}

export const SESSION_KEY = "roll.storage.session";
export const ACCOUNT_KEY = "roll.storage.account";
/** The account and user last signed in with, to fill the sign-in form. */
export const LAST_SIGNIN_KEY = "roll.storage.last-signin";

/** localStorage, or an in-memory stand-in where there's none (server rendering, tests). */
const memory = new Map<string, string>();
function store(): Pick<Storage, "getItem" | "setItem" | "removeItem"> {
  try {
    if (typeof window !== "undefined" && window.localStorage) return window.localStorage;
  } catch {
    // Storage disabled (privacy mode): fall through.
  }
  return {
    getItem: (k) => memory.get(k) ?? null,
    setItem: (k, v) => void memory.set(k, v),
    removeItem: (k) => void memory.delete(k),
  };
}
const read = (k: string) => {
  try {
    return store().getItem(k);
  } catch {
    return null;
  }
};
const write = (k: string, v: string | null) => {
  try {
    if (v === null) store().removeItem(k);
    else store().setItem(k, v);
  } catch {
    // Quota or privacy mode: the state just won't persist.
  }
};

const listeners = new Set<() => void>();
const emit = () => listeners.forEach((l) => l());

/** Subscribe to session and account changes (here, and in other tabs). */
export function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  const onStorage = (e: StorageEvent) => {
    if (e.key === SESSION_KEY || e.key === ACCOUNT_KEY || e.key === null) listener();
  };
  if (typeof window !== "undefined") window.addEventListener("storage", onStorage);
  return () => {
    listeners.delete(listener);
    if (typeof window !== "undefined") window.removeEventListener("storage", onStorage);
  };
}

/** A stored session, if it's well-formed (anything else reads as signed out). */
export function parseSession(raw: string | null): ConsoleSession | null {
  if (!raw) return null;
  try {
    const s = JSON.parse(raw) as Partial<ConsoleSession>;
    if (typeof s.token !== "string" || !s.token || typeof s.expires_at !== "number") return null;
    if (typeof s.account_id !== "string" || typeof s.user !== "string") return null;
    return {
      token: s.token,
      expires_at: s.expires_at,
      account_id: s.account_id,
      account_alias: typeof s.account_alias === "string" ? s.account_alias : null,
      user: s.user,
      arn: typeof s.arn === "string" ? s.arn : "",
      mfa: s.mfa === true,
    };
  } catch {
    return null;
  }
}

let cached: { raw: string | null; value: ConsoleSession | null } = { raw: null, value: null };

/** The console session, unless there's none or it has expired. Stable between changes (for useSyncExternalStore). */
export function getSession(now = Date.now()): ConsoleSession | null {
  const raw = read(SESSION_KEY);
  if (raw !== cached.raw) cached = { raw, value: parseSession(raw) };
  const s = cached.value;
  return s && s.expires_at > now ? s : null;
}

export function setSession(s: ConsoleSession) {
  write(SESSION_KEY, JSON.stringify(s));
  ended = false;
  emit();
}

let ended = false;

/** Forget the session. `expired`: the server ended it (shown on the sign-in page). */
export function clearSession(expired = false) {
  const had = read(SESSION_KEY) !== null;
  write(SESSION_KEY, null);
  if (expired && had) ended = true;
  emit();
}

/** Whether the last session was ended by the server (expired or revoked) rather than signed out. */
export function sessionEnded() {
  return ended;
}

export function acknowledgeEnded() {
  if (!ended) return;
  ended = false;
  emit();
}

/** A 12-digit account id. */
export const isAccountId = (v: string) => /^\d{12}$/.test(v);

/** The account the operator acts in, or `null` for the default account. */
export function getAccount(): string | null {
  const v = read(ACCOUNT_KEY);
  return v && isAccountId(v) ? v : null;
}

/** Act in another account (`null`: the default one). */
export function setAccount(id: string | null) {
  write(ACCOUNT_KEY, id && isAccountId(id) ? id : null);
  emit();
}

/**
 * Whom requests act as, as one string that changes whenever that changes: `session:<account>:<user>`,
 * `account:<id>` or `default`. Pages key their data on it.
 */
export function getScope(): string {
  const s = getSession();
  if (s) return `session:${s.account_id}:${s.user}`;
  const a = getAccount();
  return a ? `account:${a}` : "default";
}

/**
 * A stored session past its expiry is ended (as if the server said 401) rather than quietly
 * dropped: requests must not fall back to operator mode behind the user's back. Throws then.
 */
export function assertSessionLive(now = Date.now()) {
  const stored = parseSession(read(SESSION_KEY));
  if (stored && stored.expires_at <= now) {
    clearSession(true);
    throw new StorageError("Your session has expired. Sign in again.", 401, "ExpiredToken");
  }
}

/** The headers every storage request carries: the session's bearer token, else the selected account. */
export function storageHeaders(): Record<string, string> {
  const s = getSession();
  if (s) return { authorization: `Bearer ${s.token}` };
  const a = getAccount();
  return a ? { "x-roll-account": a } : {};
}

/** A storage API answer: 401 ends a session that was sent. */
function onStatus(status: number, sentSession: boolean) {
  if (status === 401 && sentSession) clearSession(true);
}

// --- Requests ----------------------------------------------------------------------------------

/**
 * `call` for `/v1/storage/...`: the same JSON handling, plus the account / session headers.
 * Errors are `StorageError`s (with the API's `code`); a network failure rejects as fetch does.
 */
export async function storageCall<T>(method: string, path: string, body?: unknown, opts: { anonymous?: boolean } = {}): Promise<T> {
  if (!opts.anonymous) assertSessionLive();
  const auth = opts.anonymous ? {} : storageHeaders();
  const sentSession = "authorization" in auth;
  const headers: Record<string, string> = { ...auth };
  if (body !== undefined) headers["content-type"] = "application/json";
  const res = await fetch(`${ROLL_API}${path}`, {
    method,
    headers: Object.keys(headers).length ? headers : undefined,
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store",
  });
  const text = await res.text();
  let data: { error?: string; code?: string } | null = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    // Not the API (a proxy's error page, say): report the status, not a JSON syntax error.
    if (res.ok) throw new Error(`${method} ${path}: the response isn't JSON`);
  }
  if (!res.ok) {
    if (!opts.anonymous) onStatus(res.status, sentSession);
    const message = (data && typeof data === "object" && data.error) || `${res.status} ${res.statusText}`.trim();
    throw new StorageError(message, res.status, (data && typeof data === "object" && data.code) || null);
  }
  return data as T;
}

/** A query parameter naming the selected account, for plain links (downloads) that can't send headers. */
export function accountQuery(): Record<string, string> {
  if (getSession()) return {};
  const a = getAccount();
  return a ? { account: a } : {};
}

/**
 * Links can't send `Authorization`: while signed in, fetch the file with the session and save it.
 * Returns a click handler for an `<a href={downloadUrl}>`; without a session the link works as is.
 */
export function sessionDownload(url: string, filename: string, onError?: (message: string) => void) {
  return (e: { preventDefault(): void }) => {
    if (!getSession()) return;
    e.preventDefault();
    (async () => {
      assertSessionLive();
      const auth = storageHeaders();
      const res = await fetch(url, { headers: auth, cache: "no-store" });
      if (!res.ok) {
        onStatus(res.status, "authorization" in auth);
        let msg = `${res.status} ${res.statusText}`.trim();
        try {
          msg = ((await res.json()) as { error?: string }).error ?? msg;
        } catch {
          // keep the status
        }
        throw new Error(msg);
      }
      const href = URL.createObjectURL(await res.blob());
      const a = document.createElement("a");
      a.href = href;
      a.download = filename.split("/").filter(Boolean).pop() ?? "download";
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(href), 10_000);
    })().catch((err) => onError?.(errorMessage(err)));
  };
}

// --- Console sign-in ---------------------------------------------------------------------------

export interface SigninRequest {
  /** 12-digit account id or the account's alias. */
  account: string;
  user: string;
  password: string;
  mfa_code?: string;
  new_password?: string;
}

/** What the server says about the current session (no token). */
export type SessionInfo = Omit<ConsoleSession, "token">;

/** The steps a sign-in can need, from the API's 401 codes. */
export type SigninStep = "password" | "mfa" | "new-password";

/** Which step a sign-in error asks for (`null`: it's just an error). */
export function signinStepFor(code: string | null): SigninStep | null {
  if (code === "MfaRequired") return "mfa";
  if (code === "PasswordResetRequired") return "new-password";
  return null;
}

/** The body sent to `/console/signin`: trimmed account and user, the extra fields only when given. */
export function signinBody(r: SigninRequest): SigninRequest {
  return {
    account: r.account.trim(),
    user: r.user.trim(),
    password: r.password,
    ...(r.mfa_code?.trim() ? { mfa_code: r.mfa_code.trim() } : {}),
    ...(r.new_password ? { new_password: r.new_password } : {}),
  };
}

export const consoleApi = {
  /** Sign in; on success the session is kept and every storage request acts as the user. */
  signin: async (r: SigninRequest): Promise<ConsoleSession> => {
    const s = await storageCall<ConsoleSession>("POST", "/v1/storage/console/signin", signinBody(r), { anonymous: true });
    setSession(s);
    write(LAST_SIGNIN_KEY, JSON.stringify({ account: r.account.trim(), user: r.user.trim() }));
    return s;
  },
  /** The current session as the server sees it (a 401 forgets it). */
  session: () => storageCall<SessionInfo>("GET", "/v1/storage/console/session"),
  /** Sign out: the token is forgotten here (it expires on its own). */
  signout: () => clearSession(false),
  lastSignin: (): { account: string; user: string } | null => {
    try {
      const v = JSON.parse(read(LAST_SIGNIN_KEY) ?? "null") as { account?: unknown; user?: unknown } | null;
      return v && typeof v.account === "string" && typeof v.user === "string" ? { account: v.account, user: v.user } : null;
    } catch {
      return null;
    }
  },
};

// --- Accounts (operator) -----------------------------------------------------------------------

export interface StorageAccount {
  /** 12 digits. */
  id: string;
  /** 64 hex characters, as in ACLs. */
  canonical: string;
  name: string;
  email: string | null;
  created_at: number;
  default: boolean;
  alias: string | null;
}

/** The root user's access key, created with the account; the secret is only returned then. */
export interface RootKey {
  id: string;
  secret_access_key: string;
  name?: string;
  permission?: string;
  created_at?: number;
}

export type CreatedAccount = StorageAccount & { root_key?: RootKey };

const acct = (id: string) => `/v1/storage/accounts/${encodeURIComponent(id)}`;

export const accountsApi = {
  list: () => storageCall<StorageAccount[]>("GET", "/v1/storage/accounts"),
  get: (id: string) => storageCall<StorageAccount>("GET", acct(id)),
  create: (name: string, opts: { email?: string; root_key?: boolean } = {}) =>
    storageCall<CreatedAccount>("POST", "/v1/storage/accounts", {
      name,
      ...(opts.email?.trim() ? { email: opts.email.trim() } : {}),
      ...(opts.root_key === undefined ? {} : { root_key: opts.root_key }),
    }),
  /** `email: null` removes it. */
  update: (id: string, patch: { name?: string; email?: string | null }) => storageCall<StorageAccount>("PATCH", acct(id), patch),
  /** Only an account without buckets (409 otherwise). */
  remove: (id: string) => storageCall<{ deleted: string }>("DELETE", acct(id)),
};

/** How an account reads: its alias or name, then the id. */
export function accountLabel(a: Pick<StorageAccount, "name" | "alias">) {
  return a.alias ?? a.name;
}
