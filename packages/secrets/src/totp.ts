/**
 * TOTP codes (RFC 6238), as crates/roll-secrets/src/totp.rs computes them (docs/secrets.md, "TOTP
 * keys"): for a seed a sealed resolve already gave, without asking roll for its code. A seed is an
 * `otpauth://totp/…` URI or bare base32 (SHA1, 6 digits, 30 s). Errors never quote the seed.
 */
import { hmac } from "@noble/hashes/hmac";
import { sha1 } from "@noble/hashes/legacy";
import { sha256, sha512 } from "@noble/hashes/sha2";
import type { Totp } from "./types.js";

const ALGORITHMS = { SHA1: sha1, SHA256: sha256, SHA512: sha512 } as const;

function base32(s: string): Uint8Array {
  const out: number[] = [];
  let buf = 0;
  let bits = 0;
  for (const c of s.replace(/ /g, "").replace(/=+$/, "").toUpperCase()) {
    const v = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567".indexOf(c);
    if (v < 0) throw new Error("a TOTP seed is an otpauth://totp/ URI or base32");
    buf = ((buf << 5) | v) & 0xffff;
    bits += 5;
    if (bits >= 8) {
      bits -= 8;
      out.push((buf >> bits) & 0xff);
    }
  }
  return Uint8Array.from(out);
}

/** The code of `seed` at `atMs` (now by default), and when it expires. */
export function totpAt(seed: string, atMs: number = Date.now()): Totp {
  seed = seed.trim();
  let alg: keyof typeof ALGORITHMS = "SHA1";
  let digits = 6;
  let period = 30;
  let raw = seed;
  if (seed.slice(0, 10).toLowerCase() === "otpauth://") {
    if (seed.slice(0, 15).toLowerCase() !== "otpauth://totp/") throw new Error("an otpauth URI must be otpauth://totp/… (HOTP isn't supported)");
    let q: URLSearchParams;
    try {
      q = new URL(seed).searchParams;
    } catch {
      throw new Error("an otpauth URI's query is badly escaped");
    }
    raw = q.get("secret") ?? "";
    if (!raw) throw new Error("an otpauth URI needs ?secret=");
    const a = (q.get("algorithm") ?? "SHA1").toUpperCase();
    if (!(a in ALGORITHMS)) throw new Error("algorithm must be SHA1, SHA256 or SHA512");
    alg = a as keyof typeof ALGORITHMS;
    digits = Number(q.get("digits") ?? 6);
    if (!Number.isInteger(digits) || digits < 6 || digits > 8) throw new Error("digits must be 6–8");
    period = Number(q.get("period") ?? 30);
    if (!Number.isInteger(period) || period < 15 || period > 120) throw new Error("period must be 15–120 seconds");
  }
  const secret = base32(raw);
  if (secret.length < 10 || secret.length > 128) throw new Error("a TOTP secret is 16–205 base32 characters (80–1024 bits)");
  const step = Math.floor(Math.floor(atMs / 1000) / period);
  const counter = new Uint8Array(8);
  new DataView(counter.buffer).setBigUint64(0, BigInt(step));
  const h = hmac(ALGORITHMS[alg], secret, counter);
  const o = h[h.length - 1] & 0x0f;
  const n = (((h[o] & 0x7f) << 24) | (h[o + 1] << 16) | (h[o + 2] << 8) | h[o + 3]) % 10 ** digits;
  return { code: String(n).padStart(digits, "0"), expires_at: (step + 1) * period * 1000, period };
}
