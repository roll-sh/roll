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

/** One `KEY=value` line of a dotenv file, offsets within the line; the value without its quotes. */
export type DotenvLine = { line: number; key: string; keyStart: number; value: string; valueStart: number; valueEnd: number; reference: boolean };

/** A dotenv file's entries (`export KEY=value`, quoted or not); comments and blank lines skipped. */
export function dotenvLines(text: string): DotenvLine[] {
  const out: DotenvLine[] = [];
  text.split(/\r?\n/).forEach((raw, line) => {
    const m = /^(\s*(?:export\s+)?)([A-Za-z_][A-Za-z0-9_]*)\s*=\s*/.exec(raw);
    if (!m || raw.trimStart().startsWith("#")) return;
    const keyStart = m[1].length;
    let valueStart = m[0].length;
    let value = raw.slice(valueStart).trimEnd();
    let valueEnd = valueStart + value.length;
    const quote = value[0];
    if ((quote === '"' || quote === "'") && value.length >= 2 && value.endsWith(quote)) {
      value = value.slice(1, -1);
      valueStart += 1;
      valueEnd -= 1;
      // As the CLI reads it: double quotes unescape `\n`, `\"` and `\\`; single quotes are as written.
      if (quote === '"') value = value.replace(/\\([n"\\])/g, (_, c: string) => (c === "n" ? "\n" : c));
    } else {
      // An unquoted value ends at a ` #` comment.
      const hash = value.search(/\s#/);
      if (hash >= 0) {
        value = value.slice(0, hash);
        valueEnd = valueStart + value.length;
      }
    }
    out.push({ line, key: m[2], keyStart, value, valueStart, valueEnd, reference: value.startsWith("roll://") });
  });
  return out;
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
