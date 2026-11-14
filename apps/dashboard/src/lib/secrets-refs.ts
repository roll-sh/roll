/**
 * Secret references in values (docs/secrets.md, "References"): `${KEY}`, `${env.KEY}`,
 * `${env.folder.sub.KEY}`, and `$${` for a literal `${`. Anything else between `${` and `}` is
 * literal text. `parse` mirrors crates/roll-secrets/src/refs.rs exactly; change both together.
 *
 * On top of it, what the value editors need: where a reference is being typed (`typing`), what it
 * could complete to (`suggest`), and which references point at nothing (`check`).
 */

import { ENV_SLUG, SECRET_KEY } from "./secrets-api";

const DEPTH_MAX = 8;
const SEGMENT = /^[A-Za-z0-9_-]{1,64}$/;

/** `env` null: the same environment and folder. `path` is `/` for `${env.KEY}`. */
export type Ref = { env: string | null; path: string; key: string };

/** `brace`: a `${…}` that isn't a reference, kept as text. */
export type Part = { kind: "text"; text: string; brace?: true } | { kind: "escaped" } | { kind: "ref"; ref: Ref; raw: string };

export function display(r: Ref): string {
  if (r.env === null) return r.key;
  return r.path === "/" ? `${r.env}.${r.key}` : `${r.env}.${r.path.slice(1).replace(/\//g, ".")}.${r.key}`;
}

function reference(inner: string): Ref | null {
  const segs = inner.split(".");
  const key = segs[segs.length - 1];
  if (!SECRET_KEY.test(key)) return null;
  if (segs.length === 1) return { env: null, path: "/", key };
  if (!ENV_SLUG.test(segs[0])) return null;
  const folders = segs.slice(1, -1);
  if (folders.length > DEPTH_MAX || !folders.every((s) => SEGMENT.test(s))) return null;
  return { env: segs[0], path: folders.length ? `/${folders.join("/")}` : "/", key };
}

/** A value's parts, as the server reads it. */
export function parse(v: string): Part[] {
  const out: Part[] = [];
  let rest = v;
  for (let i = rest.indexOf("${"); i >= 0; i = rest.indexOf("${")) {
    if (i > 0 && rest[i - 1] === "$") {
      if (i > 1) out.push({ kind: "text", text: rest.slice(0, i - 1) });
      out.push({ kind: "escaped" });
      rest = rest.slice(i + 2);
      continue;
    }
    const end = rest.indexOf("}", i + 2);
    if (end < 0) break;
    const r = reference(rest.slice(i + 2, end));
    if (i > 0) out.push({ kind: "text", text: rest.slice(0, i) });
    out.push(r ? { kind: "ref", ref: r, raw: rest.slice(i, end + 1) } : { kind: "text", text: rest.slice(i, end + 1), brace: true });
    rest = rest.slice(end + 1);
  }
  if (rest) out.push({ kind: "text", text: rest });
  return out;
}

/** The references in a value, in order, without repeats. */
export function refs(v: string): Ref[] {
  const out: Ref[] = [];
  for (const p of parse(v)) if (p.kind === "ref" && !out.some((r) => display(r) === display(p.ref))) out.push(p.ref);
  return out;
}

/** A reference being typed at `caret`: where its `${` starts and what's typed after it. */
export function typing(v: string, caret: number): { start: number; query: string } | null {
  if (caret < 2) return null;
  const start = v.lastIndexOf("${", caret - 2);
  if (start < 0 || (start > 0 && v[start - 1] === "$")) return null;
  const query = v.slice(start + 2, caret);
  return /^[A-Za-z0-9_.-]*$/.test(query) ? { start, query } : null;
}

/** One folder's names: its keys and its subfolders' names. */
export type Listing = { keys: string[]; folders: string[] };
export type Lookup = (env: string, path: string) => Listing | undefined;

export type Suggestion = {
  /** What goes between `${` and `}` (or stays open, for an environment or a folder). */
  insert: string;
  kind: "key" | "env" | "folder";
  label: string;
};

/**
 * What `query` (the text after `${`) can complete to: the keys a bare `${KEY}` sees (`here`),
 * then the other environments (`envs`), then, inside one, its folders' keys and subfolders.
 * `need` is the folder to load when `listing` doesn't have it yet.
 */
export function suggest(query: string, here: string[], envs: string[], listing: Lookup): { items: Suggestion[]; need?: { env: string; path: string } } {
  const segs = query.split(".");
  const last = segs[segs.length - 1].toLowerCase();
  const match = (name: string) => name.toLowerCase().includes(last);
  const rank = (a: string, b: string) => Number(!a.toLowerCase().startsWith(last)) - Number(!b.toLowerCase().startsWith(last)) || a.localeCompare(b);
  if (segs.length === 1)
    return {
      items: [
        ...here.filter(match).sort(rank).map((k): Suggestion => ({ insert: k, kind: "key", label: k })),
        ...envs.filter(match).sort(rank).map((e): Suggestion => ({ insert: `${e}.`, kind: "env", label: `${e}.` })),
      ],
    };
  const [env, ...folders] = segs.slice(0, -1);
  if (!envs.includes(env) || folders.length > DEPTH_MAX || !folders.every((s) => SEGMENT.test(s))) return { items: [] };
  const path = folders.length ? `/${folders.join("/")}` : "/";
  const l = listing(env, path);
  if (!l) return { items: [], need: { env, path } };
  const prefix = segs.slice(0, -1).join(".");
  return {
    items: [
      ...l.keys.filter(match).sort(rank).map((k): Suggestion => ({ insert: `${prefix}.${k}`, kind: "key", label: k })),
      ...(folders.length < DEPTH_MAX ? l.folders.filter(match).sort(rank).map((f): Suggestion => ({ insert: `${prefix}.${f}.`, kind: "folder", label: `${f}.` })) : []),
    ],
  };
}

/** `v` with the reference typed at `caret` completed to `s`: the new value and caret. A key closes it with `}`. */
export function accept(v: string, caret: number, s: Suggestion): { value: string; caret: number } {
  const t = typing(v, caret);
  if (!t) return { value: v, caret };
  const after = v.slice(caret);
  const head = `${v.slice(0, t.start)}\${${s.insert}`;
  if (s.kind !== "key") return { value: head + after, caret: head.length };
  return after.startsWith("}") ? { value: head + after, caret: head.length + 1 } : { value: `${head}}${after}`, caret: head.length + 1 };
}

/**
 * Warnings before saving: references to keys that aren't there, a key referring to itself, and a
 * `${…}` that isn't a reference (it stays text). `need` are the folders to load to check the rest.
 */
export function check(v: string, self: string, here: Set<string>, listing: Lookup): { warnings: string[]; need: { env: string; path: string }[] } {
  const warnings: string[] = [];
  const need: { env: string; path: string }[] = [];
  for (const p of parse(v)) {
    if (p.kind === "text" && p.brace) warnings.push(`${p.text} isn't a reference, so it stays text (write $\${ for a literal \${)`);
    if (p.kind !== "ref") continue;
    const r = p.ref;
    if (r.env === null) {
      if (r.key === self) warnings.push(`${self} references itself`);
      else if (!here.has(r.key)) warnings.push(`\${${r.key}}: ${r.key} isn't in this folder`);
      continue;
    }
    const l = listing(r.env, r.path);
    if (!l) need.push({ env: r.env, path: r.path });
    else if (!l.keys.includes(r.key)) warnings.push(`\${${display(r)}}: ${r.env}${r.path === "/" ? "" : r.path} has no ${r.key}`);
  }
  return { warnings: [...new Set(warnings)], need };
}
