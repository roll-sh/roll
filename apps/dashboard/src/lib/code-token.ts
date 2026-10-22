/**
 * The roll code token the dashboard sends (`Authorization: Bearer`), kept in this browser's
 * localStorage. Only needed when the server runs with `ROLL_CODE_TOKEN`: an owner token (`rlc_…`)
 * or the admin token. Components follow changes with `subscribeToken`, and anything that gets a
 * 401 calls `requestSignIn` to open the prompt.
 */

const KEY = "roll.code.token";
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

export function setToken(token: string | null) {
  const s = storage();
  if (!s) return;
  const t = token?.trim();
  if (t) s.setItem(KEY, t);
  else s.removeItem(KEY);
  window.dispatchEvent(new Event(CHANGED));
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
