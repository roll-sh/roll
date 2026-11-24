/**
 * Browser keys derived without a call to roll (docs/search/guides/site-search.md, "A key per end
 * customer"; Algolia's `generateSecuredApiKey`): a backend signs a short-lived key that narrows a
 * derivable browser key, with that key's signing secret, and hands it to the page, which sends it
 * as any key (`Authorization: ApiKey <key>`). The cluster refuses one that would widen its parent.
 *
 * The key is `rdk1.<payload>.<mac>`: base64url (no padding) of the payload's JSON, and of
 * HMAC-SHA256 over `rdk1.<payload>` keyed with the secret as written. Web Crypto only, so the same
 * code runs in Node 20+, Deno, Bun and Workers. Never in a page: the secret signs any key.
 */

/** What a derived key may do: never more than its parent. */
export interface DerivedKeyParams {
  /** The derivable browser key's id. */
  key: string;
  /** The cluster's name. */
  cluster: string;
  /** When it stops working, in unix seconds: at most 24 hours ahead. */
  exp: number;
  /** Some of the parent's indices (names, or patterns within its own). */
  indices?: string[];
  /** The parent's search application (the default for a key of one). */
  application?: string;
  /** One query, ANDed with the parent's filter. */
  filter?: Record<string, unknown>;
  /** The end customer it's for: its own share of the parent's rate limit. */
  user?: string;
  /** Pages it may be sent from: some of the cluster's allowed origins. */
  origins?: string[];
  /** The user's requests per minute, below its share (needs `user`). */
  rate_limit?: { per_minute: number };
}

function b64url(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** The derived key `secret` signs for `payload`, one JSON object as written. */
export async function signDerivedKey(secret: string, payload: string): Promise<string> {
  const enc = new TextEncoder();
  const signed = `rdk1.${b64url(enc.encode(payload))}`;
  const key = await crypto.subtle.importKey("raw", enc.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  const mac = new Uint8Array(await crypto.subtle.sign("HMAC", key, enc.encode(signed)));
  return `${signed}.${b64url(mac)}`;
}

/** A derived key for `params`, signed with the parent's signing secret. */
export function deriveBrowserKey(secret: string, params: DerivedKeyParams): Promise<string> {
  return signDerivedKey(secret, JSON.stringify(params));
}
