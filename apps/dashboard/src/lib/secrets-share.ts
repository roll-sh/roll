/**
 * Sharing a value once (docs/secrets.md, "Sharing a value once"). The browser encrypts the value
 * with a fresh 256-bit key; the server keeps only base64url(nonce12 ‖ AES-256-GCM ciphertext) with
 * `roll-share-v1` as associated data, and the key travels in the link's `#fragment`, which browsers
 * never send. The same format as `roll secrets share` / `open` (crates/roll-cli/src/secrets.rs,
 * `share_seal`).
 *
 * Pure JS (`@noble/ciphers`), not WebCrypto: a page on plain http has no `crypto.subtle`.
 *
 * The public routes are called without any headers: the share page is for people with no roll
 * sign-in, and must never send the operator's.
 */
import { gcm } from "@noble/ciphers/aes";
import { b64url, fromB64url } from "./agents-seal";
import { ROLL_API } from "./roll-api";

const AAD = new TextEncoder().encode("roll-share-v1");

/** What a share's link says before it's opened (`GET /v1/secrets/shares/{id}`). */
export type SharePeek = { note: string; views_left: number; expires_at: number };

/** Encrypts `value` for a share: the ciphertext for the server and the key for the link. */
export function sealShare(value: string, fixed?: { key: Uint8Array; nonce: Uint8Array }): { ciphertext: string; key: string } {
  const key = fixed?.key ?? crypto.getRandomValues(new Uint8Array(32));
  const nonce = fixed?.nonce ?? crypto.getRandomValues(new Uint8Array(12));
  const ct = gcm(key, nonce, AAD).encrypt(new TextEncoder().encode(value));
  const out = new Uint8Array(12 + ct.length);
  out.set(nonce);
  out.set(ct, 12);
  return { ciphertext: b64url(out), key: b64url(key) };
}

/** The link's key (the fragment, with or without `#`), or null when it isn't a 32-byte base64url key. */
export function shareKey(fragment: string): Uint8Array | null {
  const s = fragment.replace(/^#/, "");
  if (!/^[A-Za-z0-9_-]{43}$/.test(s)) return null;
  try {
    const k = fromB64url(s);
    return k.length === 32 ? k : null;
  } catch {
    return null;
  }
}

/** Decrypts an opened share; throws when the key doesn't open it. */
export function openSealed(ciphertext: string, key: Uint8Array): string {
  const raw = fromB64url(ciphertext);
  if (raw.length < 12 + 16) throw new Error("the share is too short");
  try {
    return new TextDecoder("utf-8", { fatal: false }).decode(gcm(key, raw.subarray(0, 12), AAD).decrypt(raw.subarray(12)));
  } catch {
    throw new Error("the link's key doesn't open this value");
  }
}

/** The link handed to someone: the page, and the key after `#`. */
export const shareLink = (origin: string, id: string, key: string) => `${origin}/share/${encodeURIComponent(id)}#${key}`;

/** A share that's gone: opened as many times as it could be, expired or revoked. */
export class ShareGone extends Error {}

async function call<T>(method: "GET" | "POST", path: string, base: string): Promise<T> {
  const res = await fetch(`${base}${path}`, { method, cache: "no-store", credentials: "omit", referrerPolicy: "no-referrer" });
  if (res.status === 404) throw new ShareGone("This link was already opened, has expired or was revoked.");
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`.trim();
    try {
      msg = ((await res.json()) as { error?: string }).error ?? msg;
    } catch {
      // not JSON: the status says it
    }
    throw new Error(msg);
  }
  return (await res.json()) as T;
}

/** The note and opens left; uses nothing up. `base` is rolld (the server's own address on the server). */
export const peekShare = (id: string, base = ROLL_API) => call<SharePeek>("GET", `/v1/secrets/shares/${encodeURIComponent(id)}`, base);

/** One of the share's opens: its ciphertext. */
export const openShare = (id: string) => call<{ ciphertext: string }>("POST", `/v1/secrets/shares/${encodeURIComponent(id)}/open`, ROLL_API);
