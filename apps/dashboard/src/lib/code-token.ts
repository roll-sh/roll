/**
 * The token the dashboard sends (`Authorization: Bearer`), kept in this browser's localStorage:
 * rolld's operator token (`ROLL_ADMIN_TOKEN`), which every product's API takes, or a roll code
 * token (`rlc_…` or `ROLL_CODE_TOKEN`), which only roll code takes. Sign-in checks it against
 * `GET /v1/code` and remembers which it is. Components follow changes with `subscribeToken`, and
 * anything that gets a 401 calls `requestSignIn` to open the prompt.
 */

const KEY = "roll.code.token";
/** "1" while the saved token is the operator's. */
const OPERATOR = "roll.operator";
const CHANGED = "roll:code-token";
const SIGN_IN = "roll:code-sign-in";

function storage(): Storage | null {
  try {
    return typeof window === "undefined" ? null : window.localStorage;
  } catch {
    return null;
  }
}

export function getToken(): string | undefined {
  return storage()?.getItem(KEY)?.trim() || undefined;
}

export function setToken(token: string | null, operator = false) {
  const s = storage();
  if (!s) return;
  const t = token?.trim();
  if (t) s.setItem(KEY, t);
  else s.removeItem(KEY);
  markOperator(Boolean(t) && operator);
  window.dispatchEvent(new Event(CHANGED));
}

/** Record whether the saved token is the operator's (what `GET /v1/code` said of it). */
export function markOperator(operator: boolean) {
  const s = storage();
  if (!s) return;
  if (operator) s.setItem(OPERATOR, "1");
  else s.removeItem(OPERATOR);
}

/**
 * `Authorization` for the products' APIs (Postgres, Redis, streams, tasks, queues, storage,
 * nosql): the operator token when one is saved, else nothing (a daemon on loopback is open).
 */
export function operatorHeaders(): Record<string, string> {
  const t = getToken();
  return t && storage()?.getItem(OPERATOR) === "1" ? { authorization: `Bearer ${t}` } : {};
}

/** Calls `fn` when the token changes, here or in another tab. Returns the unsubscribe function. */
export function subscribeToken(fn: () => void) {
  if (typeof window === "undefined") return () => {};
  const onStorage = (e: StorageEvent) => {
    if (e.key === null || e.key === KEY) fn();
  };
  window.addEventListener(CHANGED, fn);
  window.addEventListener("storage", onStorage);
  return () => {
    window.removeEventListener(CHANGED, fn);
    window.removeEventListener("storage", onStorage);
  };
}

/** Ask the sign-in prompt to open (after a 401). */
export function requestSignIn() {
  if (typeof window !== "undefined") window.dispatchEvent(new Event(SIGN_IN));
}

export function onSignInRequest(fn: () => void) {
  if (typeof window === "undefined") return () => {};
  window.addEventListener(SIGN_IN, fn);
  return () => window.removeEventListener(SIGN_IN, fn);
}
