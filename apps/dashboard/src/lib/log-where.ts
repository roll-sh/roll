/**
 * Log filters by a line's fields, as the API's `where` reads them (crates/apps-control/src/log_where.rs):
 * `level:error user_id:42`, `key:*` (there), `-key:value` (not that), a dotted key into nested
 * JSON, a quoted value with spaces. A line's fields are its JSON object's or its logfmt pairs;
 * values compare as text in any case. The filter box splits what it's given: field terms go to
 * `where`, the other words stay a text search.
 */

export interface Term {
  key: string[];
  /** null: the field is there. */
  value: string | null;
  not: boolean;
}

const KEY = /^[A-Za-z0-9_-]+(\.[A-Za-z0-9_-]+)*$/;

/** Words, spaces inside double quotes kept. */
function tokens(s: string): string[] {
  return s.match(/(?:[^\s"]+|"[^"]*")+/g) ?? [];
}

const unquote = (v: string) => (v.length >= 2 && v.startsWith('"') && v.endsWith('"') ? v.slice(1, -1) : v);

/** A field term (`level:error`, `-user_id:42`, `@msg:"a b"`), or null for a word (a URL's `https://…` stays a word). */
export function term(token: string): Term | null {
  const not = token.startsWith("-");
  let t = not ? token.slice(1) : token;
  if (t.startsWith("@")) t = t.slice(1);
  const i = t.indexOf(":");
  if (i <= 0) return null;
  const key = t.slice(0, i);
  const value = unquote(t.slice(i + 1));
  // A field's name starts with a letter or `_`, so `127.0.0.1:5432` and `12:30` stay words; a
  // value that starts with `/` (a URL's `//`) or a digit after a colon-led word stays one too
  // unless it's asked for with `@`.
  const explicit = token.replace(/^-/, "").startsWith("@");
  if (!KEY.test(key) || key.length > 64 || value === "" || value.startsWith("/")) return null;
  if (!explicit && !/^[A-Za-z_]/.test(key)) return null;
  return { key: key.split("."), value: value === "*" ? null : value.toLowerCase(), not };
}

/** The filter box's text as field terms (the API's `where`) and the words left (its `q`). */
export function splitFilter(text: string): { where: string; words: string; terms: Term[] } {
  const fields: string[] = [];
  const words: string[] = [];
  const terms: Term[] = [];
  for (const tok of tokens(text.trim())) {
    // At most ten field terms (the API's limit): the rest are words.
    const t = terms.length < 10 ? term(tok) : null;
    if (t) {
      fields.push(tok);
      terms.push(t);
    } else words.push(tok);
  }
  return { where: fields.join(" "), words: words.join(" "), terms };
}

function fieldsOf(text: string): (key: string[]) => string | undefined {
  const t = text.trim();
  if (t.startsWith("{")) {
    try {
      const v: unknown = JSON.parse(t);
      if (v && typeof v === "object" && !Array.isArray(v))
        return (key) => {
          let at: unknown = v;
          for (const k of key) {
            if (!at || typeof at !== "object" || !(k in (at as object))) return undefined;
            at = (at as Record<string, unknown>)[k];
          }
          if (typeof at === "string") return at;
          if (at === null) return "null";
          if (typeof at === "number" || typeof at === "boolean") return String(at);
          return "";
        };
    } catch {
      // Not JSON: logfmt, below.
    }
  }
  const pairs = new Map<string, string>();
  for (const w of tokens(t)) {
    const i = w.indexOf("=");
    if (i > 0 && KEY.test(w.slice(0, i))) pairs.set(w.slice(0, i), unquote(w.slice(i + 1)));
  }
  return (key) => pairs.get(key.join("."));
}

/** Whether `text`'s fields say what every term asks. */
export function matchesTerms(terms: Term[], text: string): boolean {
  if (!terms.length) return true;
  const get = fieldsOf(text);
  return terms.every((t) => {
    const got = get(t.key);
    const hit = t.value === null ? got !== undefined : got !== undefined && got.toLowerCase() === t.value;
    return hit !== t.not;
  });
}
