/** Environment variables, where the runtime has them (Node, Bun, Deno with permission). */
export function env(name: string): string | undefined {
  const g = globalThis as { process?: { env?: Record<string, string | undefined> }; Deno?: { env?: { get(n: string): string | undefined } } };
  try {
    const v = g.process?.env?.[name] ?? g.Deno?.env?.get(name);
    return v === "" ? undefined : v;
  } catch {
    return undefined;
  }
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

export function utf8(s: string): Uint8Array {
  return encoder.encode(s);
}

export function fromUtf8(b: Uint8Array): string {
  return decoder.decode(b);
}

export function toBytes(body: string | Uint8Array | ArrayBuffer | ArrayBufferView): Uint8Array {
  if (typeof body === "string") return utf8(body);
  if (body instanceof Uint8Array) return body;
  if (body instanceof ArrayBuffer) return new Uint8Array(body);
  if (ArrayBuffer.isView(body)) return new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
  throw new TypeError("body must be a string, Uint8Array or ArrayBuffer");
}

export function isBinary(v: unknown): v is Uint8Array | ArrayBuffer | ArrayBufferView {
  return v instanceof ArrayBuffer || ArrayBuffer.isView(v);
}

export function base64Encode(bytes: Uint8Array): string {
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(s);
}

export function base64Decode(s: string): Uint8Array {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function concatBytes(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

/** Compare without leaking where the inputs differ. */
export function timingSafeEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i]! ^ b[i]!;
  return diff === 0;
}

/** Drop `undefined` values (so they aren't sent). */
export function compact<T extends Record<string, unknown>>(o: T): T {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(o)) if (v !== undefined) out[k] = v;
  return out as T;
}

const camel = (s: string) => s.replace(/_([a-z0-9])/g, (_, c: string) => c.toUpperCase());

/** Values under these keys are user data and are kept exactly as sent. */
const OPAQUE = new Set(["data", "output", "error", "user", "headers", "body", "source_body", "response", "origin", "if", "match"]);
/** Objects under these keys are maps whose keys are user-chosen (step ids …). */
const MAPS = new Set(["step_attempts", "keys"]);

/** snake_case API responses → camelCase, leaving user payloads alone. */
export function camelize<T = unknown>(v: unknown): T {
  return walk(v) as T;
}

function walk(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(walk);
  if (!v || typeof v !== "object") return v;
  const out: Record<string, unknown> = {};
  for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
    if (OPAQUE.has(k)) out[camel(k)] = val;
    else if (MAPS.has(k) && val && typeof val === "object" && !Array.isArray(val)) out[camel(k)] = { ...(val as object) };
    else out[camel(k)] = walk(val);
  }
  return out;
}

export function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal?.aborted) return resolve();
    const t = setTimeout(done, ms);
    function done() {
      clearTimeout(t);
      signal?.removeEventListener("abort", done);
      resolve();
    }
    signal?.addEventListener("abort", done, { once: true });
  });
}
