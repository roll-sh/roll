/**
 * Search params as plain strings, the way the URL has them. TanStack Router's default codec reads
 * values as JSON (`?n=1` is a number) and writes them quoted; roll's URLs are hand-built query
 * strings, so this keeps them byte-for-byte: `?demo` stays `?demo`, `%20` stays `%20`, and the
 * first of a repeated key wins (as the pages always read it).
 */

/** What every route's `useSearch()` and `loaderDeps` see. */
export type Search = Partial<Record<string, string>>;

export function parseSearch(query: string): Search {
  const out: Search = {};
  for (const [k, v] of new URLSearchParams(query)) if (!(k in out)) out[k] = v;
  return out;
}

export function stringifySearch(search: Record<string, unknown>): string {
  const parts: string[] = [];
  for (const [k, v] of Object.entries(search)) {
    if (v === undefined || v === null) continue;
    const value = String(v);
    parts.push(value === "" ? encodeURIComponent(k) : `${encodeURIComponent(k)}=${encodeURIComponent(value)}`);
  }
  return parts.length ? `?${parts.join("&")}` : "";
}

/** The root route's `validateSearch`: types the params without dropping any (a dropped key would redirect). */
export const anySearch = (search: Record<string, unknown>) => search as Search;
