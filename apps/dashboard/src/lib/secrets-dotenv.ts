/**
 * The bulk paste's preview: a `.env` file read the way the server reads it
 * (crates/roll-secrets/src/dotenv.rs, which this follows rule for rule), and what importing it
 * would do to the folder's current secrets. The import itself is the server's (`POST …/import`).
 */

import { SECRET_KEY, type Secret } from "./secrets-api";

export type DotenvEntry = { key: string; value: string; comment: string; line: number };

const VALUE_MAX = 64 * 1024;

/**
 * The entries in order (a key set twice keeps its first place and its last value), or the first
 * bad line (1-based) and why, with the entries before it.
 */
export function parseDotenv(text: string): { entries: DotenvEntry[]; error: string | null } {
  const out: DotenvEntry[] = [];
  const lines = text.split("\n").map((l) => (l.endsWith("\r") ? l.slice(0, -1) : l));
  if (lines.length && lines[lines.length - 1] === "") lines.pop();
  let comment = "";
  let i = 0;
  const fail = (error: string) => ({ entries: out, error });
  while (i < lines.length) {
    const line = lines[i].replace(/^﻿+/, "");
    const n = ++i;
    let t = line.trim();
    if (!t) {
      comment = "";
      continue;
    }
    if (t.startsWith("#")) {
      comment = t.slice(1).trim();
      continue;
    }
    if (t.startsWith("export ")) t = t.slice(7).trimStart();
    const eq = t.indexOf("=");
    if (eq < 0) return fail(`line ${n}: expected KEY=value`);
    const key = t.slice(0, eq).trim();
    if (!SECRET_KEY.test(key)) return fail(`line ${n}: a key is a letter or _ then letters, digits and _ (got ${JSON.stringify(key.slice(0, 40))})`);
    const rest = t.slice(eq + 1).trimStart();
    let value: string;
    if (rest.startsWith('"')) {
      let acc = "";
      let cur = rest.slice(1);
      for (;;) {
        const end = closing(cur);
        if (end !== null) {
          acc += cur.slice(0, end);
          break;
        }
        if (i >= lines.length) return fail(`line ${n}: the double quote isn't closed`);
        acc += `${cur}\n`;
        cur = lines[i++];
      }
      value = unescape(acc);
    } else if (rest.startsWith("'")) {
      const end = rest.indexOf("'", 1);
      if (end < 0) return fail(`line ${n}: the single quote isn't closed`);
      value = rest.slice(1, end);
    } else {
      const at = rest.indexOf(" #");
      value = (at < 0 ? rest : rest.slice(0, at)).trim();
    }
    if (new TextEncoder().encode(value).length > VALUE_MAX) return fail(`line ${n}: a value is at most 64 KiB`);
    const entry = { key, value, comment, line: n };
    comment = "";
    const at = out.findIndex((e) => e.key === key);
    if (at >= 0) out[at] = entry;
    else out.push(entry);
  }
  return { entries: out, error: null };
}

/** The position of the first `"` not escaped by a backslash. */
function closing(s: string): number | null {
  let escaped = false;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (c === "\\" && !escaped) escaped = true;
    else if (c === '"' && !escaped) return i;
    else escaped = false;
  }
  return null;
}

function unescape(s: string): string {
  const map: Record<string, string> = { n: "\n", r: "\r", t: "\t" };
  let out = "";
  for (let i = 0; i < s.length; i++) {
    if (s[i] !== "\\") {
      out += s[i];
      continue;
    }
    const next = s[++i];
    out += next === undefined ? "\\" : (map[next] ?? next);
  }
  return out;
}

/**
 * What an entry does on import: `new` adds the key (`over` names where the key showed up from
 * before, when it was imported or inherited and the folder gets its own value), `replace` writes a
 * new version of the folder's own key (with `overwrite`), `skip` leaves it (without).
 */
export type ImportRow = { key: string; line: number; action: "new" | "replace" | "skip"; over?: string };

/** The preview of importing `entries` into a folder whose secrets are `current`, as the server decides it. */
export function previewImport(entries: DotenvEntry[], current: Secret[], overwrite: boolean): ImportRow[] {
  const own = new Set(current.filter((s) => s.source.kind === "own").map((s) => s.key));
  const inherited = new Map(
    current
      .filter((s) => s.source.kind !== "own")
      .map((s) => [s.key, s.source.kind === "import" ? `imported from ${s.source.environment}${s.source.path === "/" ? "" : s.source.path}` : `from ${s.source.kind === "base" ? s.source.environment : ""}`] as const),
  );
  return entries.map((e) => {
    if (own.has(e.key)) return { key: e.key, line: e.line, action: overwrite ? "replace" : "skip" };
    const over = inherited.get(e.key);
    return over ? { key: e.key, line: e.line, action: "new", over } : { key: e.key, line: e.line, action: "new" };
  });
}
