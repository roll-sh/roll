// The roll secrets extension's logic, without VS Code (tested with vitest): where code reads an
// environment variable, what a `.env` line holds, which environments set a key (from the folder's
// grid: names and presence, never a value), and the CLI's per-directory setup.

/** An environment variable the code reads: its name and where the name is (offsets in the text). */
export type Read = { name: string; start: number; end: number };

const N = "([A-Za-z_][A-Za-z0-9_]*)";
const Q = `["'\`]`;
// Each pattern's first group is the name. `d`: the group's offsets, so a hover lands on the name.
const PATTERNS = {
  js: [`process\\.env\\.${N}`, `process\\.env\\[\\s*${Q}${N}${Q}\\s*\\]`, `import\\.meta\\.env\\.${N}`, `Bun\\.env\\.${N}`, `Deno\\.env\\.get\\(\\s*${Q}${N}${Q}`],
  python: [`os\\.environ\\[\\s*${Q}${N}${Q}\\s*\\]`, `os\\.environ\\.get\\(\\s*${Q}${N}${Q}`, `os\\.getenv\\(\\s*${Q}${N}${Q}`],
  go: [`os\\.(?:Getenv|LookupEnv)\\(\\s*"${N}"`],
  rust: [`env::var(?:_os)?\\(\\s*"${N}"`, `(?:option_)?env!\\(\\s*"${N}"`],
  ruby: [`ENV\\[\\s*${Q}${N}${Q}\\s*\\]`, `ENV\\.fetch\\(\\s*${Q}${N}${Q}`],
  php: [`getenv\\(\\s*${Q}${N}${Q}`, `\\$_ENV\\[\\s*${Q}${N}${Q}\\s*\\]`],
  jvm: [`System\\.getenv\\(\\s*"${N}"`],
  csharp: [`Environment\\.GetEnvironmentVariable\\(\\s*"${N}"`],
} as const;

const LANGUAGE: Record<string, keyof typeof PATTERNS> = {
  javascript: "js",
  typescript: "js",
  javascriptreact: "js",
  typescriptreact: "js",
  vue: "js",
  svelte: "js",
  astro: "js",
  python: "python",
  go: "go",
  rust: "rust",
  ruby: "ruby",
  php: "php",
  java: "jvm",
  kotlin: "jvm",
  scala: "jvm",
  csharp: "csharp",
};

/** The languages whose environment reads are found. */
export const LANGUAGES = Object.keys(LANGUAGE);

const COMPILED = Object.fromEntries(Object.entries(PATTERNS).map(([k, list]) => [k, list.map((p) => new RegExp(p, "gd"))])) as Record<keyof typeof PATTERNS, RegExp[]>;

/** The environment variables `text` (in `languageId`) reads, in order. */
export function envReads(text: string, languageId: string): Read[] {
  const kind = LANGUAGE[languageId];
  if (!kind) return [];
  const out: Read[] = [];
  for (const re of COMPILED[kind]) {
    re.lastIndex = 0;
    for (let m = re.exec(text); m; m = re.exec(text)) {
      const at = m.indices?.[1];
      if (at) out.push({ name: m[1], start: at[0], end: at[1] });
    }
  }
  return out.sort((a, b) => a.start - b.start);
}

/** Where a completion of variable names belongs: right after `process.env.` or an opening quote of a read. */
export function completesNames(linePrefix: string, languageId: string): boolean {
  const kind = LANGUAGE[languageId];
  if (!kind) return false;
  const dot = /(?:process\.env|import\.meta\.env|Bun\.env)\.[A-Za-z0-9_]*$/;
  const quoted = /(?:process\.env\[|Deno\.env\.get\(|os\.environ\[|os\.environ\.get\(|os\.getenv\(|os\.(?:Getenv|LookupEnv)\(|env::var(?:_os)?\(|env!\(|ENV\[|ENV\.fetch\(|getenv\(|\$_ENV\[|System\.getenv\(|Environment\.GetEnvironmentVariable\()\s*["'`][A-Za-z0-9_]*$/;
  return (kind === "js" && dot.test(linePrefix)) || quoted.test(linePrefix);
}

/** Whether `fileName` is a dotenv file (`.env`, `.env.local`, `.env.example`…). */
export function isDotenvFile(fileName: string): boolean {
  return /(^|[\\/])\.env(\.[A-Za-z0-9_.-]+)?$/.test(fileName);
}

/**
 * One `KEY=value` entry of a dotenv file, read as the CLI and the server read it (roll-secrets'
 * `dotenv::parse`): a double-quoted value may run over several lines and unescapes `\n`, `\r`,
 * `\t` and `\"`, with anything after its closing quote ignored; a single-quoted one is as written;
 * an unquoted one ends at ` #`. It spans `line`:`from` (its opening quote, or its first character)
 * to `endLine`:`to` (after its closing quote): replacing that range replaces all of it.
 */
export type DotenvLine = { line: number; endLine: number; key: string; value: string; from: number; to: number; reference: boolean };

/** Where a double-quoted value closes in `s` (an unescaped `"`), or -1. */
function closing(s: string): number {
  let escaped = false;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (c === "\\" && !escaped) escaped = true;
    else if (c === '"' && !escaped) return i;
    else escaped = false;
  }
  return -1;
}

function unescape(s: string): string {
  const named: Record<string, string> = { n: "\n", r: "\r", t: "\t" };
  return s.replace(/\\(.?)/gs, (_, c: string) => named[c] ?? (c === "" ? "\\" : c));
}

/** roll's limits on a key's name and a value (roll-secrets' `names::KEY_MAX`, `VALUE_MAX`). */
const KEY_MAX = 256;
const VALUE_MAX = 64 * 1024;

/**
 * A dotenv file's entries, or none at all when roll's parser would refuse the file (a line that
 * isn't `KEY=value`, an unclosed quote, a key or value over roll's limits): nothing is offered on a
 * file `roll secrets import` won't read. A key set twice is its last entry, as roll keeps it.
 */
export function dotenvLines(text: string): DotenvLine[] {
  const out: DotenvLine[] = [];
  const lines = text.split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    const raw = lines[i].replace(/^\uFEFF/, "");
    const t = raw.trim();
    if (!t || t.startsWith("#")) continue;
    // `export ` (a space, as the CLI strips it), then the key, `=`, and the value.
    const m = /^(\s*(?:export \s*)?)([A-Za-z_][A-Za-z0-9_]*)\s*=\s*/.exec(raw);
    if (!m) return [];
    const from = m[0].length;
    // The CLI reads a line trimmed: a double-quoted value's first line ends where its text does.
    const rest = raw.slice(from).trimEnd();
    const line = i;
    if (rest.startsWith('"')) {
      let cur = rest.slice(1);
      let acc = "";
      let end = closing(cur);
      while (end < 0 && i + 1 < lines.length) {
        acc += `${cur}\n`;
        cur = lines[++i];
        end = closing(cur);
      }
      if (end < 0) return [];
      acc += cur.slice(0, end);
      const to = (i === line ? from + 1 : 0) + end + 1;
      const value = unescape(acc);
      out.push({ line, endLine: i, key: m[2], value, from, to, reference: value.startsWith("roll://") });
    } else if (rest.startsWith("'")) {
      const end = rest.indexOf("'", 1);
      if (end < 0) return [];
      const value = rest.slice(1, end);
      out.push({ line, endLine: line, key: m[2], value, from, to: from + end + 1, reference: value.startsWith("roll://") });
    } else {
      const cut = rest.indexOf(" #");
      const body = cut >= 0 ? rest.slice(0, cut) : rest;
      const value = body.trim();
      const start = from + (body.length - body.trimStart().length);
      out.push({ line, endLine: line, key: m[2], value, from: start, to: start + value.length, reference: value.startsWith("roll://") });
    }
  }
  if (out.some((e) => e.key.length > KEY_MAX || new TextEncoder().encode(e.value).length > VALUE_MAX)) return [];
  return out.filter((e, i) => !out.slice(i + 1).some((later) => later.key === e.key));
}

/** A key's `roll://` reference for a folder (the CLI's grammar), or null when a part can't be written in one. */
export function rollRef(env: string, path: string, key: string): string | null {
  const parts = [env, ...path.split("/").filter(Boolean), key];
  const ok = (p: string) => p !== "." && p !== ".." && /^[A-Za-z0-9._-]+$/.test(p);
  return parts.every(ok) ? `roll://${parts.join("/")}` : null;
}

/** What the extension reads of `GET …/grid`: which environments have a cell for each key. Values never come. */
export type GridLike = { environments: string[]; rows: { key: string; cells: Record<string, unknown | null> }[] };

/** Which environments set `name` (in this folder) and which don't; `known`: any does. */
export type Presence = { set: string[]; missing: string[]; known: boolean };

export function presence(grid: GridLike, name: string): Presence {
  const row = grid.rows.find((r) => r.key === name);
  const set = grid.environments.filter((e) => row?.cells[e] != null);
  return { set, missing: grid.environments.filter((e) => !set.includes(e)), known: set.length > 0 };
}

/** The hover over a variable name: where roll sets it. Never a value. */
export function hoverText(name: string, p: Presence, at: { project: string; env: string; path: string }): string {
  const where = `roll secrets · ${at.project}${at.path === "/" ? "" : ` ${at.path}`}`;
  // Only the environments this token reads are in the grid: say which were looked at.
  if (!p.known) return `**${name}** isn't set in ${p.missing.join(" or ") || "any environment"} (${where}).`;
  const lines = [`**${name}** in ${where}`, "", `Set in ${p.set.join(", ")}`];
  if (p.missing.length) lines.push("", `Not set in ${p.missing.join(", ")}`);
  if (!p.set.includes(at.env)) lines.push("", `This workspace runs ${at.env}, which doesn't set it.`);
  return lines.join("\n");
}

/** A warning: the code reads a key roll sets elsewhere, but not in the chosen environment. */
export type Problem = Read & { message: string };

/**
 * Reads of keys roll knows (some environment sets them) that `env` lacks. Names roll doesn't know
 * at all (`PATH`, `NODE_ENV`) are left alone: they may come from anywhere.
 */
export function problems(reads: Read[], grid: GridLike, env: string): Problem[] {
  if (!grid.environments.includes(env)) return [];
  return reads.flatMap((r) => {
    const p = presence(grid, r.name);
    return p.known && !p.set.includes(env) ? [{ ...r, message: `${r.name} isn't set in ${env} (set in ${p.set.join(", ")})` }] : [];
  });
}

/** What `roll secrets setup` remembers for a directory (`~/.config/roll/secrets.json`). */
export type Setup = { project: string; name: string; env: string; path: string };

/** The setup of `dir` or its nearest parent that has one, as the CLI finds it. */
export function nearestSetup(all: Record<string, Setup>, dir: string): Setup | undefined {
  let d = dir.replace(/[\\/]+$/, "") || "/";
  for (;;) {
    if (all[d]) return all[d];
    const up = d.replace(/[\\/][^\\/]*$/, "") || "/";
    if (up === d) return undefined;
    d = up;
  }
}
