/**
 * Small JSON decoders for wire types mirrored from Rust (serde). A decoder checks the shape at the
 * boundary and fills serde's defaults, so the rest of the app works with complete records.
 *
 * `strict` also rejects keys and tags the TypeScript side doesn't know: the fixture tests decode
 * strictly so a field added in Rust fails until it's mirrored here. The app decodes leniently and
 * ignores what it doesn't know, so an older dashboard keeps working against a newer server.
 */

export interface DecodeCtx {
  strict: boolean;
  /** A key the enclosing tagged union owns (`type`, `kind`); not an unknown key. */
  tag?: string;
}

export type Decoder<T> = ((v: unknown, path: string, ctx: DecodeCtx) => T) & {
  /** What a missing key decodes to; absent means the key is required. */
  missing?: () => T;
};

export class DecodeError extends Error {
  constructor(
    readonly path: string,
    message: string,
  ) {
    super(`${path || "$"}: ${message}`);
  }
}

const fail = (path: string, msg: string): never => {
  throw new DecodeError(path, msg);
};

const kind = (v: unknown) => (v === null ? "null" : Array.isArray(v) ? "array" : typeof v);

export const str: Decoder<string> = (v, p) => (typeof v === "string" ? v : fail(p, `expected a string, got ${kind(v)}`));

export const bool: Decoder<boolean> = (v, p) => (typeof v === "boolean" ? v : fail(p, `expected a boolean, got ${kind(v)}`));

export const num: Decoder<number> = (v, p) =>
  typeof v === "number" && Number.isFinite(v) ? v : fail(p, `expected a number, got ${kind(v)}`);

/** Rust integers (u16, u32, u64, i32). */
export const int: Decoder<number> = (v, p) =>
  typeof v === "number" && Number.isInteger(v) ? v : fail(p, `expected an integer, got ${kind(v)}`);

/** `serde_json::Value` with `#[serde(default)]`: anything, `null` when missing. */
export const json: Decoder<unknown> = Object.assign((v: unknown) => v, { missing: () => null });

export function literal<const T extends string>(...values: T[]): Decoder<T> {
  const set = new Set<string>(values);
  return (v, p) =>
    typeof v === "string" && set.has(v) ? (v as T) : fail(p, `expected one of ${values.join(", ")}, got ${JSON.stringify(v)}`);
}

export function array<T>(d: Decoder<T>): Decoder<T[]> {
  return (v, p, ctx) => (Array.isArray(v) ? v.map((x, i) => d(x, `${p}[${i}]`, ctx)) : fail(p, `expected an array, got ${kind(v)}`));
}

export function map<T>(d: Decoder<T>): Decoder<Record<string, T>> {
  return (v, p, ctx) => {
    if (typeof v !== "object" || v === null || Array.isArray(v)) return fail(p, `expected an object, got ${kind(v)}`);
    const out: Record<string, T> = {};
    for (const [k, x] of Object.entries(v)) out[k] = d(x, `${p}.${k}`, ctx);
    return out;
  };
}

/** `Option<T>`: missing or `null` decode to `null`. */
export function nullable<T>(d: Decoder<T>): Decoder<T | null> {
  return Object.assign((v: unknown, p: string, ctx: DecodeCtx) => (v === null || v === undefined ? null : d(v, p, ctx)), {
    missing: () => null,
  });
}

/** `Option<T>` on what the dashboard sends: missing or `null` decode to `undefined`, so it serializes back the same. */
export function optional<T>(d: Decoder<T>): Decoder<T | undefined> {
  return Object.assign((v: unknown, p: string, ctx: DecodeCtx) => (v === null || v === undefined ? undefined : d(v, p, ctx)), {
    missing: () => undefined,
  });
}

/** `#[serde(default)]`. */
export function withDefault<T>(d: Decoder<T>, def: () => T): Decoder<T> {
  return Object.assign((v: unknown, p: string, ctx: DecodeCtx) => d(v, p, ctx), { missing: def });
}

export type Shape<T> = { [K in keyof T]-?: Decoder<T[K]> };

/** A struct: every key of `T` has a decoder. */
export function obj<T>(shape: Shape<T>): Decoder<T> {
  const keys = Object.keys(shape) as (keyof T & string)[];
  const known = new Set<string>(keys);
  return (v, p, ctx) => {
    if (typeof v !== "object" || v === null || Array.isArray(v)) return fail(p, `expected an object, got ${kind(v)}`);
    const rec = v as Record<string, unknown>;
    if (ctx.strict) {
      for (const k of Object.keys(rec)) if (!known.has(k) && k !== ctx.tag) fail(`${p}.${k}`, "unknown key");
    }
    const inner: DecodeCtx = { strict: ctx.strict };
    const out = {} as T;
    for (const k of keys) {
      const d = shape[k];
      const at = `${p}.${k}`;
      if (!(k in rec) || rec[k] === undefined) {
        if (!d.missing) fail(at, "missing");
        out[k] = d.missing!();
      } else out[k] = d(rec[k], at, inner);
    }
    return out;
  };
}

type Out<D> = D extends Decoder<infer T> ? T : never;
type Tagged<Tag extends string, V> = { [K in keyof V & string]: { [P in Tag]: K } & Out<V[K]> }[keyof V & string];

/**
 * An internally tagged enum (`#[serde(tag = "…")]`): each variant decodes the same object, minus
 * the tag. Unknown tags always fail; lenient callers drop the record.
 */
export function tagged<const Tag extends string, V extends Record<string, Decoder<object>>>(tag: Tag, variants: V): Decoder<Tagged<Tag, V>> {
  return (v, p, ctx) => {
    if (typeof v !== "object" || v === null || Array.isArray(v)) return fail(p, `expected an object, got ${kind(v)}`);
    const t = (v as Record<string, unknown>)[tag];
    if (typeof t !== "string" || !Object.hasOwn(variants, t)) return fail(`${p}.${tag}`, `unknown ${tag} ${JSON.stringify(t)}`);
    const body = variants[t](v, p, { strict: ctx.strict, tag });
    return { [tag]: t, ...body } as Tagged<Tag, V>;
  };
}

/** Decode at a boundary. */
export function decode<T>(d: Decoder<T>, v: unknown, opts: { strict?: boolean } = {}): T {
  return d(v, "", { strict: opts.strict ?? false });
}
