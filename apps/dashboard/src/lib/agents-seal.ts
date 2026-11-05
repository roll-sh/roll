/**
 * Sealing an endpoint's key to one daemon (docs/code/agents.md, "Endpoints"): the browser is the
 * only place the key is ever in the clear on roll's side. The box is `roll-seal-v1`, opened by
 * `crates/agent-host/src/endpoints.rs` (`SealKey::open`):
 *
 * - X25519 between a fresh key pair and the daemon's `seal_key` (`MachineInfo.seal_key`);
 * - HKDF-SHA256 with salt `epk ‖ seal_key` and info `roll-seal-v1` → an AES-256-GCM key;
 * - a random 12-byte nonce, and `roll-endpoint:<endpoint id>` as the associated data, so the box
 *   opens only on that daemon and only for that endpoint.
 *
 * The model gateway's upstream keys (docs/code/gateway.md) are the same box with
 * `roll-gateway-upstream:<upstream id>` as the associated data (agent-proto `upstream_key_context`),
 * so an endpoint's box never opens as an upstream's key, nor one upstream's as another's.
 *
 * In audited pure JS (`@noble/curves`, `@noble/hashes`, `@noble/ciphers`), not WebCrypto: a page
 * served over plain http (a dashboard reached over a tailnet by its name) has no `crypto.subtle`,
 * and older Safari has no X25519 in it. Only the randomness is the browser's
 * (`crypto.getRandomValues`, there in every context). Nothing here keeps the key.
 */
import { gcm } from "@noble/ciphers/aes";
import { x25519 } from "@noble/curves/ed25519";
import { hkdf } from "@noble/hashes/hkdf";
import { sha256 } from "@noble/hashes/sha2";
import type { SealedSecret } from "./agents-api";

const INFO = new TextEncoder().encode("roll-seal-v1");

export function b64url(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function fromB64url(s: string): Uint8Array<ArrayBuffer> {
  const bin = atob(s.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - (s.length % 4)) % 4));
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** For tests: a fixed ephemeral private key and nonce instead of fresh ones. */
export interface SealFixed {
  ephemeral: Uint8Array;
  nonce: Uint8Array;
}

/** Seals `key` to the daemon whose public key is `sealKey` (base64url), for `endpoint`. */
export function sealKey(sealKey: string, endpoint: string, key: string, fixed?: SealFixed): Promise<SealedSecret> {
  return seal(sealKey, `roll-endpoint:${endpoint}`, key, fixed);
}

/** Seals an upstream's key to the gateway's daemon (`gateway.key`), for that upstream only. */
export function sealUpstreamKey(sealKey: string, upstream: string, key: string, fixed?: SealFixed): Promise<SealedSecret> {
  return seal(sealKey, `roll-gateway-upstream:${upstream}`, key, fixed);
}

async function seal(sealKey: string, context: string, key: string, fixed?: SealFixed): Promise<SealedSecret> {
  const daemon = fromB64url(sealKey);
  if (daemon.length !== 32) throw new Error("the machine's seal key isn't an X25519 key");
  const eph = fixed?.ephemeral ?? crypto.getRandomValues(new Uint8Array(32));
  const epk = x25519.getPublicKey(eph);
  const shared = x25519.getSharedSecret(eph, daemon);
  // The daemon refuses an all-zero secret (a low-order key): so does this side.
  if (shared.every((b) => b === 0)) throw new Error("the machine's seal key isn't an X25519 key");
  const salt = new Uint8Array(64);
  salt.set(epk, 0);
  salt.set(daemon, 32);
  const aes = hkdf(sha256, shared, salt, INFO, 32);
  const nonce = fixed?.nonce ?? crypto.getRandomValues(new Uint8Array(12));
  const aad = new TextEncoder().encode(context);
  const ct = gcm(aes, nonce, aad).encrypt(new TextEncoder().encode(key));
  return { v: 1, epk: b64url(epk), nonce: b64url(nonce), ct: b64url(ct) };
}
