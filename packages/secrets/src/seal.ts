/**
 * `roll-seal-v1` boxes, as crates/roll-secrets/src/crypto.rs makes and opens them (`seal_to`,
 * `open_sealed`): X25519 between a fresh key pair and the recipient's key, HKDF-SHA256 with salt
 * `epk ‖ recipient` and info `roll-seal-v1` into an AES-256-GCM key, a random 12-byte nonce, and the
 * context as the associated data, so a box opens only for the purpose it was sealed for.
 *
 * Pure JS (`@noble`), so it runs in Node, workers and browsers on plain http alike.
 */
import { gcm } from "@noble/ciphers/aes";
import { x25519 } from "@noble/curves/ed25519";
import { hkdf } from "@noble/hashes/hkdf";
import { sha256 } from "@noble/hashes/sha2";
import { randomBytes } from "@noble/hashes/utils";
import type { Sealed } from "./types.js";

const INFO = new TextEncoder().encode("roll-seal-v1");

export function b64url(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function fromB64url(s: string): Uint8Array {
  const bin = atob(s.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - (s.length % 4)) % 4));
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** An X25519 key pair: `publicKey` (base64url) is what `resolve`'s `sealTo` takes; keep `secretKey` in the process. */
export type KeyPair = { publicKey: string; secretKey: Uint8Array };

export function generateKeyPair(): KeyPair {
  const secretKey = x25519.utils.randomSecretKey();
  return { publicKey: b64url(x25519.getPublicKey(secretKey)), secretKey };
}

/** The base64url public key of an X25519 secret key. */
export const publicKey = (secretKey: Uint8Array) => b64url(x25519.getPublicKey(secretKey));

function key(shared: Uint8Array, epk: Uint8Array, recipient: Uint8Array): Uint8Array {
  if (shared.every((b) => b === 0)) throw new Error("not a usable X25519 key");
  const salt = new Uint8Array(64);
  salt.set(epk);
  salt.set(recipient, 32);
  return hkdf(sha256, shared, salt, INFO, 32);
}

/** Opens a box sealed to `secretKey` for `context`; throws when it doesn't open. */
export function openSealed(box: Sealed, secretKey: Uint8Array, context: string): Uint8Array {
  try {
    if (box.v !== 1) throw new Error();
    const epk = fromB64url(box.epk);
    const nonce = fromB64url(box.nonce);
    if (epk.length !== 32 || nonce.length !== 12) throw new Error();
    const k = key(x25519.getSharedSecret(secretKey, epk), epk, x25519.getPublicKey(secretKey));
    return gcm(k, nonce, new TextEncoder().encode(context)).decrypt(fromB64url(box.ct));
  } catch {
    throw new Error("the box doesn't open");
  }
}

/** Seals `plain` to `recipient` (a base64url X25519 public key) for `context`, as the server does. */
export function sealTo(recipient: string, context: string, plain: Uint8Array | string): Sealed {
  const to = fromB64url(recipient);
  if (to.length !== 32) throw new Error("the recipient is a base64url X25519 public key (32 bytes)");
  const esk = x25519.utils.randomSecretKey();
  const epk = x25519.getPublicKey(esk);
  const nonce = randomBytes(12);
  const msg = typeof plain === "string" ? new TextEncoder().encode(plain) : plain;
  const ct = gcm(key(x25519.getSharedSecret(esk, to), epk, to), nonce, new TextEncoder().encode(context)).encrypt(msg);
  return { v: 1, epk: b64url(epk), nonce: b64url(nonce), ct: b64url(ct) };
}
