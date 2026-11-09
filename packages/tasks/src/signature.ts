import { SignatureError } from "./errors.js";
import { base64Decode, base64Encode, concatBytes, env, timingSafeEqual, toBytes, utf8 } from "./util.js";

/**
 * Request headers: a Fetch `Headers`, a Node `IncomingHttpHeaders`-style record, or entries.
 * Lookups are case-insensitive.
 */
export type HeadersLike = Headers | Record<string, string | string[] | undefined | null> | Iterable<[string, string]>;

export type BodyLike = string | Uint8Array | ArrayBuffer | ArrayBufferView;

const PREFIX = "whsec_";

export function getHeader(headers: HeadersLike, name: string): string | undefined {
  const want = name.toLowerCase();
  if (typeof (headers as Headers).get === "function" && typeof (headers as Headers).has === "function") {
    return (headers as Headers).get(want) ?? undefined;
  }
  if (Symbol.iterator in (headers as object) && !isRecord(headers)) {
    for (const [k, v] of headers as Iterable<[string, string]>) if (k.toLowerCase() === want) return v;
    return undefined;
  }
  for (const [k, v] of Object.entries(headers as Record<string, string | string[] | undefined | null>)) {
    if (k.toLowerCase() !== want || v == null) continue;
    return Array.isArray(v) ? v.join(", ") : String(v);
  }
  return undefined;
}

function isRecord(v: unknown): boolean {
  return !!v && typeof v === "object" && Object.getPrototypeOf(v) === Object.prototype;
}

type Subtle = SubtleCrypto;
let subtlePromise: Promise<Subtle> | undefined;

/** Web Crypto: global on Node 19+, Bun, Deno and edge runtimes; `node:crypto` on Node 18. */
function subtle(): Promise<Subtle> {
  subtlePromise ??= (async () => {
    const g = globalThis as { crypto?: Crypto };
    if (g.crypto?.subtle) return g.crypto.subtle;
    const specifier = "node:crypto";
    const mod = (await import(/* webpackIgnore: true */ /* @vite-ignore */ specifier)) as { webcrypto: Crypto };
    return mod.webcrypto.subtle;
  })();
  return subtlePromise;
}

const keyCache = new Map<string, Promise<CryptoKey>>();

function secretBytes(secret: string): Uint8Array {
  const raw = secret.startsWith(PREFIX) ? secret.slice(PREFIX.length) : secret;
  try {
    return base64Decode(raw);
  } catch {
    throw new SignatureError("invalid", "bad signing secret: expected whsec_<base64>");
  }
}

function hmacKey(secret: string): Promise<CryptoKey> {
  let k = keyCache.get(secret);
  if (!k) {
    const bytes = secretBytes(secret);
    k = subtle().then((s) => s.importKey("raw", bytes as BufferSource, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]));
    k.catch(() => keyCache.delete(secret));
    if (keyCache.size > 64) keyCache.clear();
    keyCache.set(secret, k);
  }
  return k;
}

async function mac(secret: string, id: string, timestamp: string, body: BodyLike): Promise<Uint8Array> {
  const key = await hmacKey(secret);
  const data = concatBytes(utf8(`${id}.${timestamp}.`), toBytes(body));
  const sig = await (await subtle()).sign("HMAC", key, data as BufferSource);
  return new Uint8Array(sig);
}

/** Sign a payload the way roll does: `v1,<base64 HMAC-SHA256(key, "{id}.{timestamp}.{body}")>`. */
export async function sign(opts: { secret: string; id: string; timestamp: number | string; body: BodyLike }): Promise<string> {
  return `v1,${base64Encode(await mac(opts.secret, opts.id, String(opts.timestamp), opts.body))}`;
}

/** The three Standard Webhooks headers for a payload, signed with every secret given. */
export async function signatureHeaders(opts: {
  secrets: string | string[];
  id: string;
  timestamp?: number;
  body: BodyLike;
}): Promise<Record<string, string>> {
  const ts = String(opts.timestamp ?? Math.floor(Date.now() / 1000));
  const secrets = Array.isArray(opts.secrets) ? opts.secrets : [opts.secrets];
  const sigs = await Promise.all(secrets.map((secret) => sign({ secret, id: opts.id, timestamp: ts, body: opts.body })));
  return { "webhook-id": opts.id, "webhook-timestamp": ts, "webhook-signature": sigs.join(" ") };
}

export interface VerifyOptions {
  /** `whsec_…` secrets to accept (current and next while rotating). */
  secrets: string | string[];
  headers: HeadersLike;
  /** The raw request body, exactly as received. */
  body: BodyLike;
  /** Largest allowed clock difference. Default 300. */
  toleranceSeconds?: number;
  /** Override "now" (unix seconds), for tests. */
  now?: number;
}

/**
 * Verify a request roll signed with Standard Webhooks. Resolves when any signature matches any
 * secret; throws a {@link SignatureError} otherwise.
 */
export async function verifySignature(opts: VerifyOptions): Promise<void> {
  const secrets = (Array.isArray(opts.secrets) ? opts.secrets : [opts.secrets]).filter(Boolean);
  if (secrets.length === 0) throw new SignatureError("invalid", "no signing secrets configured");
  const id = getHeader(opts.headers, "webhook-id");
  const ts = getHeader(opts.headers, "webhook-timestamp");
  const header = getHeader(opts.headers, "webhook-signature");
  if (!id || !ts || !header) throw new SignatureError("missing", "missing webhook-id, webhook-timestamp or webhook-signature header");
  if (!/^\d+$/.test(ts.trim())) throw new SignatureError("invalid", "bad webhook-timestamp header");
  const tolerance = opts.toleranceSeconds ?? 300;
  const now = opts.now ?? Math.floor(Date.now() / 1000);
  if (Math.abs(now - Number(ts)) > tolerance) throw new SignatureError("expired", "webhook-timestamp is outside the tolerance");

  const candidates: Uint8Array[] = [];
  for (const part of header.split(" ")) {
    const comma = part.indexOf(",");
    if (comma < 0 || part.slice(0, comma) !== "v1") continue;
    try {
      candidates.push(base64Decode(part.slice(comma + 1)));
    } catch {
      // Not base64: can't match.
    }
  }
  for (const secret of secrets) {
    const expected = await mac(secret, id, ts.trim(), opts.body);
    // Compare against every candidate so timing doesn't reveal which matched.
    let ok = false;
    for (const c of candidates) ok = timingSafeEqual(c, expected) || ok;
    if (ok) return;
  }
  throw new SignatureError("mismatch", "no signature matches");
}

/** The signing secrets from `ROLL_TASKS_SIGNING_KEY` and `ROLL_TASKS_SIGNING_KEY_NEXT`. */
export function signingKeysFromEnv(): string[] {
  return [env("ROLL_TASKS_SIGNING_KEY"), env("ROLL_TASKS_SIGNING_KEY_NEXT")].filter((s): s is string => !!s);
}

export interface ReceiverOptions {
  /** Default: `ROLL_TASKS_SIGNING_KEY` and `ROLL_TASKS_SIGNING_KEY_NEXT`. */
  secrets?: string | string[];
  toleranceSeconds?: number;
}

/** Facts roll sends with each delivery. */
export interface DeliveryInfo {
  /** `webhook-id`: the message id, stable across retries (use it to deduplicate). */
  id: string | undefined;
  messageId: string | undefined;
  /** How many times this delivery was retried before (0 on the first attempt). */
  retried: number;
  /** Retries allowed in total. */
  retries: number | undefined;
  queue: string | undefined;
  topic: string | undefined;
  scheduleId: string | undefined;
  label: string | undefined;
  /** Set on success/failure callbacks: the message the callback is about. */
  callbackFor: string | undefined;
}

export function deliveryInfo(headers: HeadersLike): DeliveryInfo {
  const n = (v: string | undefined) => (v === undefined || v === "" ? undefined : Number(v));
  return {
    id: getHeader(headers, "webhook-id"),
    messageId: getHeader(headers, "roll-message-id"),
    retried: n(getHeader(headers, "roll-retried")) ?? 0,
    retries: n(getHeader(headers, "roll-retries")),
    queue: getHeader(headers, "roll-queue"),
    topic: getHeader(headers, "roll-topic"),
    scheduleId: getHeader(headers, "roll-schedule-id"),
    label: getHeader(headers, "roll-label"),
    callbackFor: getHeader(headers, "roll-callback-for"),
  };
}

/**
 * Verifies requests from roll.
 *
 * ```ts
 * const receiver = new Receiver({ secrets: [current, next] });
 * export async function POST(req: Request) {
 *   const body = await receiver.verifyRequest(req); // throws SignatureError
 *   ...
 * }
 * ```
 */
export class Receiver {
  readonly secrets: string[];
  readonly toleranceSeconds: number;

  constructor(opts: ReceiverOptions = {}) {
    const s = opts.secrets ?? signingKeysFromEnv();
    this.secrets = (Array.isArray(s) ? s : [s]).filter(Boolean);
    if (this.secrets.length === 0) throw new TypeError("Receiver needs signing secrets (or ROLL_TASKS_SIGNING_KEY)");
    this.toleranceSeconds = opts.toleranceSeconds ?? 300;
  }

  /** Throws a {@link SignatureError} unless the request is signed by roll. */
  verify(opts: { headers: HeadersLike; body: BodyLike; now?: number }): Promise<void> {
    return verifySignature({ ...opts, secrets: this.secrets, toleranceSeconds: this.toleranceSeconds });
  }

  /** Verify a Fetch `Request` and return its body as text. */
  async verifyRequest(req: Request): Promise<string> {
    const bytes = new Uint8Array(await req.arrayBuffer());
    await this.verify({ headers: req.headers, body: bytes });
    return new TextDecoder().decode(bytes);
  }

  /** Boolean form of {@link verify}. */
  async isValid(opts: { headers: HeadersLike; body: BodyLike; now?: number }): Promise<boolean> {
    try {
      await this.verify(opts);
      return true;
    } catch (e) {
      if (e instanceof SignatureError) return false;
      throw e;
    }
  }
}
