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
 * WebCrypto only (X25519 is in every current browser); nothing here keeps the key.
 */
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

/** Whether this browser can seal (X25519 in WebCrypto). */
export async function canSeal(): Promise<boolean> {
  try {
    await crypto.subtle.generateKey({ name: "X25519" }, false, ["deriveBits"]);
    return true;
  } catch {
    return false;
  }
}

/** For tests: a fixed ephemeral key pair and nonce instead of fresh ones. */
export interface SealFixed {
  ephemeral: CryptoKeyPair;
  nonce: Uint8Array<ArrayBuffer>;
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
  const subtle = crypto.subtle;
  const daemon = fromB64url(sealKey);
  if (daemon.length !== 32) throw new Error("the machine's seal key isn't an X25519 key");
  const peer = await subtle.importKey("raw", daemon, { name: "X25519" }, false, []);
  const eph = fixed?.ephemeral ?? ((await subtle.generateKey({ name: "X25519" }, true, ["deriveBits"])) as CryptoKeyPair);
  const epk = new Uint8Array(await subtle.exportKey("raw", eph.publicKey));
  const shared = await subtle.deriveBits({ name: "X25519", public: peer }, eph.privateKey, 256);
  const salt = new Uint8Array(64);
  salt.set(epk, 0);
  salt.set(daemon, 32);
  const ikm = await subtle.importKey("raw", shared, "HKDF", false, ["deriveKey"]);
  const aes = await subtle.deriveKey({ name: "HKDF", hash: "SHA-256", salt, info: INFO }, ikm, { name: "AES-GCM", length: 256 }, false, ["encrypt"]);
  const nonce = fixed?.nonce ?? crypto.getRandomValues(new Uint8Array(12));
  const aad = new TextEncoder().encode(context);
  const ct = new Uint8Array(await subtle.encrypt({ name: "AES-GCM", iv: nonce, additionalData: aad }, aes, new TextEncoder().encode(key)));
  return { v: 1, epk: b64url(epk), nonce: b64url(nonce), ct: b64url(ct) };
}
