/**
 * Secret requests (docs/secrets.md, "Asking for a value"): a link that asks someone outside roll
 * for a folder's values. The link carries the request's answer token (`rlq_…`), its only
 * capability; the boxes are bound to the request's id, which the page reads from the peek. The page shows the request's note and key names and its public key; the
 * browser seals each value to that key (`roll-seal-v1`, `roll-secret-request:<id>:<KEY>` as the
 * associated data, so a box only opens as that key of that request) and sends only the boxes.
 * roll opens them in the write that lands them.
 *
 * Like the share page's calls, these go to rolld without any headers or cookies: the person
 * answering has no roll sign-in, and the page must never send the operator's.
 */
import type { SealedSecret } from "./agents-api";
import { type SealFixed, sealTo } from "./agents-seal";
import { ROLL_API } from "./roll-api";

/** What a request's page shows (`GET /v1/secrets/value-requests/{token}`); `id` is what the boxes are bound to. */
export type ValuePeek = { id: string; note: string; keys: string[]; expires_at: number; public_key: string };

/** The associated data of `key`'s box for request `id` (`value_requests::answer_context`). */
export const answerContext = (id: string, key: string) => `roll-secret-request:${id}:${key}`;

/** Seals each value to the request's public key: what `answerRequest` sends. */
export async function sealAnswer(id: string, publicKey: string, values: Record<string, string>, fixed?: SealFixed): Promise<Record<string, SealedSecret>> {
  const out: Record<string, SealedSecret> = {};
  for (const [key, value] of Object.entries(values)) out[key] = await sealTo(publicKey, answerContext(id, key), value, fixed);
  return out;
}

/** The link handed to someone outside roll: the dashboard's own address when rolld doesn't know it. */
export const requestLink = (origin: string, token: string) => `${origin}/request/${encodeURIComponent(token)}`;

/** A request that can't be answered: unknown (404), or answered, expired or cancelled (410). */
export class RequestGone extends Error {
  constructor(readonly status: 404 | 410) {
    super(status === 410 ? "This request was already answered, expired or was cancelled." : "There's no such request.");
  }
}

async function call<T>(method: "GET" | "POST", path: string, base: string, body?: unknown): Promise<T | null> {
  const res = await fetch(`${base}${path}`, {
    method,
    cache: "no-store",
    credentials: "omit",
    referrerPolicy: "no-referrer",
    ...(body === undefined ? {} : { headers: { "content-type": "application/json" }, body: JSON.stringify(body) }),
  });
  if (res.status === 404 || res.status === 410) throw new RequestGone(res.status);
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`.trim();
    try {
      msg = ((await res.json()) as { error?: string }).error ?? msg;
    } catch {
      // not JSON: the status says it
    }
    throw new Error(msg);
  }
  return res.status === 204 ? null : ((await res.json()) as T);
}

/** The note, key names and public key; uses nothing up. `base` is rolld (its own address on the server). */
export const peekRequest = (token: string, base = ROLL_API) => call<ValuePeek>("GET", `/v1/secrets/value-requests/${encodeURIComponent(token)}`, base) as Promise<ValuePeek>;

/** Sends the sealed values; nothing comes back. */
export const answerRequest = async (token: string, values: Record<string, SealedSecret>) => {
  await call<null>("POST", `/v1/secrets/value-requests/${encodeURIComponent(token)}/answer`, ROLL_API, { values });
};
