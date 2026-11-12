/** Loading the agents went wrong or is slow: how long to wait, and what to tell the person. */

import { ApiError } from "./code-api";
import { ROLL_API } from "./roll-api";

/** How long "Connecting…" waits before it says what it's waiting on. */
export const STUCK_MS = 10_000;

/** roll's API, as a person would recognise it (`roll.example.com`, `100.67.193.19:3421`). */
export const apiHost = () => {
  try {
    return new URL(ROLL_API).host;
  } catch {
    return ROLL_API;
  }
};

/** Why loading failed, in words: a rejected token, an unreachable server, or what it answered. */
export function loadError(e: unknown): { error: string; need?: "sign-in" } {
  const status = e instanceof ApiError ? e.status : (e as { status?: unknown } | null)?.status;
  if (status === 401 || status === 403)
    return { error: `roll didn't accept the saved token (${status}): it may have been revoked, or be for another owner.`, need: "sign-in" };
  // fetch's network failure: "Failed to fetch" (Chromium), "Load failed" (Safari), "NetworkError…" (Firefox).
  if (e instanceof TypeError) return { error: `Can't reach roll's API at ${apiHost()}. Check the network, or whether the server is up.` };
  return { error: `roll's API answered: ${e instanceof Error ? e.message : String(e)}` };
}
