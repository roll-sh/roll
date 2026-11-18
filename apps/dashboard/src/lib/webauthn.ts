/**
 * Passkeys in the browser (docs/accounts.md, "Passkeys"): roll's options arrive as JSON with
 * base64url strings; `navigator.credentials` wants ArrayBuffers and answers with them. These
 * convert both ways, so nothing here depends on the newer `parse…FromJSON`/`toJSON` helpers.
 */

export function fromB64url(s: string): ArrayBuffer {
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/").padEnd(Math.ceil(s.length / 4) * 4, "=");
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out.buffer;
}

export function toB64url(b: ArrayBuffer | ArrayBufferView): string {
  const bytes = b instanceof ArrayBuffer ? new Uint8Array(b) : new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
  let bin = "";
  for (const x of bytes) bin += String.fromCharCode(x);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

type Descriptor = { type: "public-key"; id: string };

/** roll's registration options (`publicKey`) as `navigator.credentials.create` takes them. */
export function creationOptions(o: {
  challenge: string;
  user: { id: string; name: string; displayName: string };
  excludeCredentials?: Descriptor[];
  [k: string]: unknown;
}): PublicKeyCredentialCreationOptions {
  return {
    ...(o as unknown as PublicKeyCredentialCreationOptions),
    challenge: fromB64url(o.challenge),
    user: { ...o.user, id: fromB64url(o.user.id) },
    excludeCredentials: (o.excludeCredentials ?? []).map((c) => ({ type: c.type, id: fromB64url(c.id) })),
  };
}

/** roll's sign-in options (`publicKey`) as `navigator.credentials.get` takes them. */
export function requestOptions(o: { challenge: string; allowCredentials?: Descriptor[]; [k: string]: unknown }): PublicKeyCredentialRequestOptions {
  return {
    ...(o as unknown as PublicKeyCredentialRequestOptions),
    challenge: fromB64url(o.challenge),
    allowCredentials: (o.allowCredentials ?? []).map((c) => ({ type: c.type, id: fromB64url(c.id) })),
  };
}

/** A credential as roll reads it: base64url fields, no buffers. */
export function credentialJSON(c: PublicKeyCredential): Record<string, unknown> {
  const r = c.response as AuthenticatorAttestationResponse & AuthenticatorAssertionResponse;
  const response: Record<string, string> = { clientDataJSON: toB64url(r.clientDataJSON) };
  if ("attestationObject" in r && r.attestationObject) response.attestationObject = toB64url(r.attestationObject);
  if ("authenticatorData" in r && r.authenticatorData) response.authenticatorData = toB64url(r.authenticatorData);
  if ("signature" in r && r.signature) response.signature = toB64url(r.signature);
  return { id: c.id, rawId: toB64url(c.rawId), type: c.type, response };
}

/** Whether this browser can make and use passkeys at all. */
export function passkeysSupported(): boolean {
  return typeof window !== "undefined" && typeof window.PublicKeyCredential === "function" && !!navigator.credentials;
}

/** What a browser's refusal means to a person (they closed the prompt, or the key was there already). */
export function passkeyProblem(e: unknown): string {
  const name = e instanceof DOMException ? e.name : "";
  if (name === "NotAllowedError") return "The passkey prompt was closed or timed out.";
  if (name === "InvalidStateError") return "This device already has a passkey for your account.";
  if (name === "SecurityError") return "Passkeys only work on roll's own address.";
  return e instanceof Error ? e.message : String(e);
}
