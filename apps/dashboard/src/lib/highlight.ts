/**
 * Syntax highlighting with shiki on the server. The theme is shiki's css-variables theme; each
 * token comes back as a class (`sh-keyword`, `sh-string`, …) that globals.css maps onto the
 * monochrome palette, so code follows light and dark mode with no inline colours.
 */

import {
  bundledLanguagesInfo,
  createHighlighter,
  createJavaScriptRegexEngine,
  type BundledLanguage,
  type Highlighter,
} from "shiki";
import { createCssVariablesTheme } from "shiki/core";

/** One token: text and its class (none for plain text). */
export type Token = { t: string; c?: string };

/** Above this, files are shown without highlighting. */
export const HIGHLIGHT_MAX_BYTES = 400_000;

const theme = createCssVariablesTheme({ name: "roll", variablePrefix: "--sh-", fontStyle: false });

let instance: Promise<Highlighter> | null = null;
const highlighter = () =>
  (instance ??= createHighlighter({ themes: [theme], langs: [], engine: createJavaScriptRegexEngine({ forgiving: true }) }));

const known = new Map<string, string>();
for (const l of bundledLanguagesInfo) {
  known.set(l.id, l.id);
  for (const a of l.aliases ?? []) known.set(a, l.id);
}

const byExtension: Record<string, string> = {
  mjs: "javascript",
  cjs: "javascript",
  mts: "typescript",
  cts: "typescript",
  h: "c",
  hh: "cpp",
  hpp: "cpp",
  cc: "cpp",
  cxx: "cpp",
  yml: "yaml",
  md: "markdown",
  mdx: "mdx",
  lock: "toml",
  conf: "ini",
  cfg: "ini",
  env: "dotenv",
  sql: "sql",
  proto: "proto",
  tf: "hcl",
  svg: "xml",
  plist: "xml",
};

const byName: Record<string, string> = {
  dockerfile: "docker",
  makefile: "make",
  "cmakelists.txt": "cmake",
  "cargo.lock": "toml",
  ".gitignore": "ini",
  ".dockerignore": "ini",
  ".editorconfig": "ini",
  ".env": "dotenv",
  justfile: "just",
  gemfile: "ruby",
  rakefile: "ruby",
  "go.mod": "go",
};

/** The shiki language for a file path, or null for plain text. */
export function languageFor(path: string): string | null {
  const base = path.split("/").pop()!.toLowerCase();
  if (byName[base]) return byName[base];
  if (base.startsWith("dockerfile")) return "docker";
  const ext = base.includes(".") ? base.split(".").pop()! : "";
  const id = byExtension[ext] ?? ext;
  return known.get(id) ?? null;
}

/** Tokenized lines, or null when the language is unknown or the file too big. */
export async function highlight(code: string, lang: string | null): Promise<Token[][] | null> {
  if (!lang || code.length > HIGHLIGHT_MAX_BYTES) return null;
  try {
    const h = await highlighter();
    if (!h.getLoadedLanguages().includes(lang)) await h.loadLanguage(lang as BundledLanguage);
    const { tokens } = h.codeToTokens(code, { lang: lang as BundledLanguage, theme: "roll" });
    return tokens.map((line) => {
      const out: Token[] = [];
      for (const tok of line) {
        const m = /--sh-token-([a-z-]+)/.exec(tok.color ?? "");
        const c = m ? `sh-${m[1]}` : undefined;
        const last = out[out.length - 1];
        // Merge neighbours with the same class to keep the payload small.
        if (last && last.c === c) last.t += tok.content;
        else out.push(c ? { t: tok.content, c } : { t: tok.content });
      }
      return out;
    });
  } catch {
    return null;
  }
}
