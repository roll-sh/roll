/**
 * Sudo mode (docs/accounts.md): a change to how an account is reached, or a token made from a
 * session, answers 403 `reauth_required` without a recent proof. The `ReauthDialog` the app shell
 * mounts registers here; a call that hits it waits for the person to confirm it's them, then runs
 * once more. Nobody to ask (no dialog mounted): the error stands.
 */
let confirmIdentity: (() => Promise<void>) | null = null;

export function onReauthRequired(f: (() => Promise<void>) | null) {
  confirmIdentity = f;
}

/** Whether `e` is sudo mode's refusal (an error carrying the API's `code`). */
export function needsReauth(e: unknown): boolean {
  return typeof e === "object" && e !== null && (e as { code?: unknown }).code === "reauth_required";
}

/** Run `once`; on sudo mode's refusal, ask the person to confirm, then run it once more. */
export async function withReauth<T>(once: () => Promise<T>): Promise<T> {
  try {
    return await once();
  } catch (e) {
    if (!needsReauth(e) || !confirmIdentity) throw e;
    await confirmIdentity();
    return once();
  }
}
