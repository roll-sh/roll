import { describe, expect, it } from "vitest";
import type { SealedSecret } from "./agents-api";
import { b64url, fromB64url, sealKey, sealUpstreamKey } from "./agents-seal";

// The daemon of `crates/agent-host/src/endpoints.rs`'s `opens_the_dashboards_box` (a test key).
const D = new Uint8Array([48, 209, 102, 111, 243, 150, 16, 2, 48, 117, 176, 150, 131, 15, 197, 238, 20, 27, 151, 104, 214, 63, 157, 222, 129, 143, 37, 102, 10, 254, 206, 122]);
const PUB = "KMIZzg7UJuV-Nxs3gUarEXCHzKlAkKSSSCaXrdLz4zg";
const VECTOR: SealedSecret = { v: 1, epk: "BZK0rGd_phBWP5T-hdas0-ymnZ_QtYeeaWy3uBGP2Gc", nonce: "AAECAwQFBgcICQoL", ct: "dsu-tM5kWenDNMkLzXdVKQ5AytEyazIrW93S" };

/** What the daemon does (`SealKey::open`), in WebCrypto, to check the box from this side too. */
async function open(sealed: SealedSecret, endpoint: string, context = `roll-endpoint:${endpoint}`): Promise<string> {
  const subtle = crypto.subtle;
  const priv = await subtle.importKey("jwk", { kty: "OKP", crv: "X25519", d: b64url(D), x: PUB }, { name: "X25519" }, false, ["deriveBits"]);
  const epk = fromB64url(sealed.epk);
  const peer = await subtle.importKey("raw", epk, { name: "X25519" }, false, []);
  const shared = await subtle.deriveBits({ name: "X25519", public: peer }, priv, 256);
  const salt = new Uint8Array(64);
  salt.set(epk, 0);
  salt.set(fromB64url(PUB), 32);
  const ikm = await subtle.importKey("raw", shared, "HKDF", false, ["deriveKey"]);
  const info = new TextEncoder().encode("roll-seal-v1");
  const aes = await subtle.deriveKey({ name: "HKDF", hash: "SHA-256", salt, info }, ikm, { name: "AES-GCM", length: 256 }, false, ["decrypt"]);
  const additionalData = new TextEncoder().encode(context);
  const plain = await subtle.decrypt({ name: "AES-GCM", iv: fromB64url(sealed.nonce), additionalData }, aes, fromB64url(sealed.ct));
  return new TextDecoder().decode(plain);
}

describe("sealing endpoint keys (roll-seal-v1)", () => {
  it("seals without WebCrypto (a page over plain http has no crypto.subtle)", async () => {
    const subtle = Object.getOwnPropertyDescriptor(Crypto.prototype, "subtle");
    Object.defineProperty(Crypto.prototype, "subtle", { get: () => undefined, configurable: true });
    let sealed: SealedSecret;
    try {
      expect(crypto.subtle).toBeUndefined();
      sealed = await sealKey(PUB, "gw", "sk-over-http");
    } finally {
      Object.defineProperty(Crypto.prototype, "subtle", subtle!);
    }
    expect(await open(sealed, "gw")).toBe("sk-over-http");
  });

  it("seals, byte for byte, the box the daemon's test opens (a fixed ephemeral key and nonce)", async () => {
    // `crates/agent-host/src/endpoints.rs`'s `opens_the_dashboards_box`, its second box.
    const fixed = { ephemeral: Uint8Array.from({ length: 32 }, (_, i) => i + 1), nonce: fromB64url("AAECAwQFBgcICQoL") };
    expect(await sealKey(PUB, "cliproxy", "sk-test-123", fixed)).toEqual({ v: 1, epk: "B6N8vBQgk8i3VdwbEOhstCY3StFqqFPtC9_AsrhtHHw", nonce: "AAECAwQFBgcICQoL", ct: "w3RVbnNeH9p-sAeRCazhVaXWz4la-gDXQZOD" });
  });

  it("opens the vector the daemon's test opens", async () => {
    expect(await open(VECTOR, "cliproxy")).toBe("sk-test-123");
  });

  it("seals a box of the right shape that opens only for its endpoint", async () => {
    const sealed = await sealKey(PUB, "gw", "sk-live-abc");
    expect(sealed.v).toBe(1);
    expect(sealed.epk).toMatch(/^[A-Za-z0-9_-]{43}$/);
    expect(sealed.nonce).toMatch(/^[A-Za-z0-9_-]{16}$/);
    expect(JSON.stringify(sealed)).not.toContain("sk-live");
    expect(await open(sealed, "gw")).toBe("sk-live-abc");
    await expect(open(sealed, "other")).rejects.toThrow();
    // Fresh each time.
    expect((await sealKey(PUB, "gw", "sk-live-abc")).ct).not.toBe(sealed.ct);
  });

  it("refuses a seal key that isn't one", async () => {
    await expect(sealKey("abc", "gw", "k")).rejects.toThrow(/seal key/);
  });
});

describe("sealing the gateway's upstream keys (roll-gateway-upstream:<id>)", () => {
  it("opens only as that upstream's key, never as an endpoint's or another upstream's", async () => {
    const sealed = await sealUpstreamKey(PUB, "anthropic", "sk-ant-live");
    expect(JSON.stringify(sealed)).not.toContain("sk-ant");
    expect(await open(sealed, "", "roll-gateway-upstream:anthropic")).toBe("sk-ant-live");
    await expect(open(sealed, "", "roll-gateway-upstream:bedrock")).rejects.toThrow();
    await expect(open(sealed, "anthropic")).rejects.toThrow();
    // An endpoint's box doesn't open as an upstream's either.
    await expect(open(await sealKey(PUB, "anthropic", "k"), "", "roll-gateway-upstream:anthropic")).rejects.toThrow();
  });
});
