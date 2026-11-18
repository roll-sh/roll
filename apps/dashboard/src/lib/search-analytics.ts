/**
 * Search analytics (`GET /v1/search/clusters/{id}/analytics`): what an index is searched for,
 * which searches find nothing and which results nobody clicks, and the Relevance tab's actions
 * that answer them (a synonym, a query rule).
 */
import type { RuleDraft } from "./search-relevance";
import { emptyDraft } from "./search-relevance";

export const ANALYTICS_RANGES = ["24h", "7d", "30d"] as const;
export type AnalyticsRange = (typeof ANALYTICS_RANGES)[number];

/** The index setting that turns analytics on (roll's own; off by default). */
export const ANALYTICS_SETTING = "index.roll.analytics.enabled";

export interface QueryRow {
  query: string;
  searches: number;
  no_hits: number;
  /** Hits per search that found any. */
  avg_hits?: number | null;
  avg_latency_ms?: number | null;
  clicks?: number;
  /** Clicks per search (0–1); null without searches. */
  ctr?: number | null;
  /** 1-based, over the clicks that said where. */
  avg_position?: number | null;
}

export interface SearchAnalytics {
  range: AnalyticsRange;
  index: string | null;
  from: number;
  step_s: number;
  indices: string[];
  points: { at: number; searches: number; no_hits: number; clicks: number }[];
  totals: { searches: number; no_hits: number; no_hit_rate: number | null; clicks: number; ctr: number | null };
  top: QueryRow[];
  no_results: QueryRow[];
  low_ctr: QueryRow[];
}

export type AnalyticsList = "top" | "no_results" | "low_ctr";

/** What a query row's action opens in the Relevance tab. */
export type Prefill = { kind: "synonym"; query: string } | { kind: "rule"; query: string };

/** A share as a whole percent; a share above zero but under 1% is "<1%", never "0%". */
export function percent(v: number | null | undefined): string {
  if (v == null) return "—";
  const p = v * 100;
  if (p > 0 && p < 1) return "<1%";
  return `${Math.round(p)}%`;
}

/** The analytics route's query string. */
export function analyticsQuery(index: string | null, range: AnalyticsRange): string {
  const q = new URLSearchParams({ range });
  if (index) q.set("index", index);
  return q.toString();
}

/** Indices with analytics on, from `GET /_settings/<setting>?flat_settings=true`. */
export function enabledIndices(settings: Record<string, { settings?: Record<string, unknown> }>): string[] {
  return Object.entries(settings)
    .filter(([, v]) => String(v.settings?.[ANALYTICS_SETTING] ?? "false").toLowerCase() === "true")
    .map(([k]) => k)
    .sort();
}

/** One per hour (or day) of the range, zero where nothing was recorded, oldest first. */
export function fillPoints(a: Pick<SearchAnalytics, "from" | "step_s" | "points">, now = Date.now() / 1000): SearchAnalytics["points"] {
  const by = new Map(a.points.map((p) => [p.at, p]));
  const out: SearchAnalytics["points"] = [];
  for (let at = a.from; at <= now; at += a.step_s) out.push(by.get(at) ?? { at, searches: 0, no_hits: 0, clicks: 0 });
  return out;
}

/** A synonym rule to start from: the query, then the term it should also match. */
export function synonymPrefill(query: string): string {
  return `${query}, `;
}

/** A rule id from a query: lower-case words joined by dashes, at most 40 characters. */
export function ruleIdFor(query: string): string {
  const id = query
    .toLowerCase()
    .replace(/[^\p{L}\p{N}]+/gu, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40)
    .replace(/-+$/, "");
  return id ? `query-${id}` : "query-rule";
}

/** A query rule that pins documents when a search's `query_string` is exactly the query. */
export function rulePrefill(query: string): RuleDraft {
  return { ...emptyDraft(ruleIdFor(query)), criteria: [{ type: "exact", metadata: "query_string", values: query }] };
}

/** What the empty state says, by why the list is empty. */
export function emptyText(list: AnalyticsList, a: SearchAnalytics): string {
  if (a.totals.searches === 0) return "No searches recorded in this range.";
  if (list === "no_results") return "Every search found something.";
  if (list === "low_ctr")
    return a.totals.clicks === 0
      ? "No clicks reported yet: send search_click events to see which results people pick."
      : "No query with results has a click-through under 20%.";
  return "No searches recorded in this range.";
}
