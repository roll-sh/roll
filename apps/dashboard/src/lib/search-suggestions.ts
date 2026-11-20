/**
 * Query suggestions from search analytics (`GET /v1/search/clusters/{id}/indices/{index}/suggestions`,
 * `POST …/suggestions/rebuild`): an index's autocomplete, built by roll from the queries people
 * actually search for, and the Relevance tab's Autocomplete section that manages it.
 */

/** roll's index settings for suggestions (off by default; analytics must be on too). */
export const SUGGEST = {
  enabled: "index.roll.suggestions.enabled",
  minCount: "index.roll.suggestions.min_count",
  block: "index.roll.suggestions.block",
  blockPrefix: "index.roll.suggestions.block_prefix",
} as const;

export interface SuggestionsConfig {
  /** The alias clients query: always `<index>-suggestions`. */
  index: string;
  min_count: number;
  min_hits: number;
  lookback_ms: number;
  interval_ms: number;
  max: number;
  block: string[];
  block_prefix: string[];
  click_boost: number;
}

export interface SuggestionRow {
  query: string;
  weight: number;
  searches: number;
  clicks: number;
}

export interface SuggestionsStatus {
  index: string;
  /** Null when the cluster is asleep and has no record of the index. */
  analytics: boolean | null;
  enabled: boolean;
  config: SuggestionsConfig;
  last_build: {
    /** Unix ms; null when no build has finished. */
    at: number | null;
    index: string | null;
    count: number;
    took_ms: number;
    checked_at: number | null;
    error: string | null;
  } | null;
  next_at: number | null;
  suggestions: SuggestionRow[];
  /** Rebuild answers only: whether anything was written. */
  changed?: boolean;
}

/** A query as analytics keep it (search-es `analytics::normalize`): lower case, single spaces, 128 characters. */
export function normalizeQuery(text: string): string {
  return [...text.trim().split(/\s+/).join(" ").toLowerCase()].slice(0, 128).join("").trimEnd();
}

/** The settings update that blocks `term` (exactly, or every query starting with it). */
export function blockUpdate(cfg: Pick<SuggestionsConfig, "block" | "block_prefix">, term: string, prefix = false): Record<string, string[]> {
  const key = prefix ? SUGGEST.blockPrefix : SUGGEST.block;
  const list = prefix ? cfg.block_prefix : cfg.block;
  const t = normalizeQuery(term);
  return { [key]: [...new Set([...list, t].filter(Boolean))].sort() };
}

/** The settings update that unblocks `term`. */
export function unblockUpdate(cfg: Pick<SuggestionsConfig, "block" | "block_prefix">, term: string, prefix = false): Record<string, string[] | null> {
  const key = prefix ? SUGGEST.blockPrefix : SUGGEST.block;
  const rest = (prefix ? cfg.block_prefix : cfg.block).filter((b) => b !== term);
  return { [key]: rest.length ? rest : null };
}

/** The completion suggester request a client's autocomplete sends. */
export function completionRequest(prefix: string, size = 8) {
  return { _source: false, suggest: { q: { prefix, completion: { field: "suggest", size, skip_duplicates: true } } } };
}

/** The options of {@link completionRequest}'s answer. */
export function completionOptions(answer: unknown): { text: string; weight: number }[] {
  const opts = (answer as { suggest?: { q?: { options?: { text?: string; _score?: number }[] }[] } })?.suggest?.q?.[0]?.options ?? [];
  return opts.filter((o) => typeof o.text === "string").map((o) => ({ text: o.text as string, weight: Math.round(o._score ?? 0) }));
}

/** A whole number of days, hours or minutes, the largest unit that divides it ("30d", "12h"). */
export function span(ms: number): string {
  for (const [unit, size] of [
    ["d", 86_400_000],
    ["h", 3_600_000],
    ["m", 60_000],
  ] as const) {
    if (ms >= size && ms % size === 0) return `${ms / size}${unit}`;
  }
  return `${Math.round(ms / 1000)}s`;
}

/** When something happens relative to `now` (Unix ms): "in 3h", "5m ago", "now". */
export function relative(at: number, now = Date.now()): string {
  const d = at - now;
  const a = Math.abs(d);
  const v = a < 60_000 ? null : a < 3_600_000 ? `${Math.floor(a / 60_000)}m` : a < 86_400_000 ? `${Math.floor(a / 3_600_000)}h` : `${Math.floor(a / 86_400_000)}d`;
  if (!v) return d > 0 ? "in a moment" : "just now";
  return d > 0 ? `in ${v}` : `${v} ago`;
}

/** Indices with analytics on and whether each has suggestions on, from `GET /_settings/index.roll.*?flat_settings=true`. */
export function suggestionIndices(settings: Record<string, { settings?: Record<string, unknown> }>): { index: string; suggestions: boolean }[] {
  const on = (v: unknown) => String(v ?? "false").toLowerCase() === "true";
  return Object.entries(settings)
    .filter(([k, v]) => !k.startsWith(".") && on(v.settings?.["index.roll.analytics.enabled"]) && v.settings?.["index.roll.suggestions.source"] === undefined)
    .map(([index, v]) => ({ index, suggestions: on(v.settings?.[SUGGEST.enabled]) }))
    .sort((a, b) => a.index.localeCompare(b.index));
}

/** The weight as a share of the heaviest suggestion's (for the bar beside it). */
export function share(weight: number, rows: SuggestionRow[]): number {
  const top = Math.max(1, ...rows.map((r) => r.weight));
  return Math.max(0.02, Math.min(1, weight / top));
}
