/**
 * `/v1/code/agents/{owner}/lookup/{source}/{kind}` (docs/code/http-api.md, "Lookups"): the rows a
 * picker shows for an integration's things (Linear's teams, states, users…, Slack's channels,
 * GitHub's labels), looked up by the server with the owner's stored token. A page may carry a
 * `problem` (not connected, reconnect, rate limited, unavailable) instead of rows: pickers say it
 * quietly and still take a typed value.
 *
 * `memoryLookup` answers the same interface from fixed rows, for the demo (`lookup-demo.ts`) and tests.
 */

import type { Call } from "./automations-api";
import { ApiError } from "./code-api";
import { array, decode, int, obj, optional, str, tagged, withDefault } from "./decode";

/** One row: `id` is what a field stores, `key` another value naming the same thing (a team's key). */
export interface LookupItem {
  id: string;
  label: string;
  key?: string;
  hint?: string;
  group?: string;
  /** An https picture (an avatar, a logo). */
  avatar?: string;
  /** `#rrggbb`: the thing's own colour. */
  color?: string;
  /** The kind of row in a mixed list (`channel`, `private`, `user`, `group`, a state's type…). */
  icon?: string;
}

export type LookupProblem =
  | { code: "not_connected"; message: string }
  | { code: "reconnect"; message: string }
  | { code: "rate_limited"; retry_after: number }
  | { code: "unavailable"; message: string };

export interface LookupPage {
  items: LookupItem[];
  next?: string;
  problem?: LookupProblem;
}

export interface LookupQuery {
  q?: string;
  /** The thing the rows belong to (a team for its states, a repo for its branches). */
  parent?: string;
  cursor?: string;
  /** Resolve these stored values (ids or keys) instead of listing. */
  ids?: readonly string[];
  limit?: number;
}

export interface NewLookup {
  name: string;
  parent?: string;
  color?: string;
}

export interface LookupApi {
  list(owner: string, source: string, kind: string, query?: LookupQuery): Promise<LookupPage>;
  /** Make one (a Linear label) and answer its row. */
  create(owner: string, source: string, kind: string, req: NewLookup): Promise<LookupItem>;
}

/** The server's limits (`lookup/mod.rs`). */
export const LOOKUP_LIMITS = { ids: 50, q: 100, limit: 100 } as const;

export const lookupItemD = obj<LookupItem>({
  id: str,
  label: str,
  key: optional(str),
  hint: optional(str),
  group: optional(str),
  avatar: optional(str),
  color: optional(str),
  icon: optional(str),
});

export const lookupProblemD = tagged("code", {
  not_connected: obj<{ message: string }>({ message: str }),
  reconnect: obj<{ message: string }>({ message: str }),
  rate_limited: obj<{ retry_after: number }>({ retry_after: int }),
  unavailable: obj<{ message: string }>({ message: str }),
});

export const lookupPageD = obj<LookupPage>({
  items: withDefault(array(lookupItemD), () => []),
  next: optional(str),
  problem: optional(lookupProblemD),
});

const enc = encodeURIComponent;

export function lookupPath(owner: string, source: string, kind: string, q: LookupQuery = {}): string {
  const params = new URLSearchParams();
  if (q.q) params.set("q", q.q.slice(0, LOOKUP_LIMITS.q));
  if (q.parent) params.set("parent", q.parent);
  if (q.cursor) params.set("cursor", q.cursor);
  if (q.ids?.length) params.set("ids", q.ids.slice(0, LOOKUP_LIMITS.ids).join(","));
  if (q.limit) params.set("limit", String(q.limit));
  const qs = params.toString();
  return `/v1/code/agents/${enc(owner)}/lookup/${enc(source)}/${enc(kind)}${qs ? `?${qs}` : ""}`;
}

export function createLookupApi(call: Call): LookupApi {
  return {
    list: async (owner, source, kind, query = {}) => decode(lookupPageD, await call("GET", lookupPath(owner, source, kind, query))),
    create: async (owner, source, kind, req) => decode(lookupItemD, await call("POST", lookupPath(owner, source, kind), req)),
  };
}

export const SOURCE_NAMES: Record<string, string> = { linear: "Linear", slack: "Slack", discord: "Discord", github: "GitHub", code: "roll code" };

/** What a picker says about a problem, and whether connecting again would help. */
export function problemText(p: LookupProblem, source: string): { text: string; reconnect: boolean } {
  const name = SOURCE_NAMES[source] ?? source;
  switch (p.code) {
    case "not_connected":
      return { text: `${name} isn't connected`, reconnect: true };
    case "reconnect":
      return { text: `Reconnect ${name} to see its list`, reconnect: true };
    case "rate_limited":
      return { text: `${name} asked roll to wait ${p.retry_after < 90 ? `${p.retry_after} s` : `${Math.round(p.retry_after / 60)} min`}`, reconnect: false };
    case "unavailable":
      return { text: `Couldn't reach ${name}`, reconnect: false };
  }
}

// ── in memory ──

/** Rows per `source/kind`; a function gets the query's `parent`. */
export type LookupRows = Record<string, LookupItem[] | ((parent: string | undefined) => LookupItem[])>;

/**
 * Lookups from fixed rows (the demo, tests), with the server's rules: `q` matches the label, key
 * or hint (case-insensitive), `ids` resolves by id or key, pages of `limit` (cursor = offset), an
 * unknown `source/kind` is a 404, a source listed in `problems` answers that problem. Creating adds
 * a row to `source/kind` (under `parent` when the rows are per parent).
 */
export function memoryLookup(rows: LookupRows, problems: Record<string, LookupProblem> = {}): LookupApi & { made: LookupItem[]; calls: string[] } {
  const made: LookupItem[] = [];
  const calls: string[] = [];
  const extra = new Map<string, LookupItem[]>();
  const all = (source: string, kind: string, parent?: string): LookupItem[] => {
    const r = rows[`${source}/${kind}`];
    if (!r) throw new ApiError(`${source} lookup ${JSON.stringify(kind)}`, 404, null);
    const base = typeof r === "function" ? r(parent) : r;
    return [...base, ...(extra.get(`${source}/${kind}/${parent ?? ""}`) ?? [])];
  };
  return {
    made,
    calls,
    list: async (owner, source, kind, q = {}) => {
      calls.push(lookupPath(owner, source, kind, q));
      const list = all(source, kind, q.parent);
      if (problems[source]) return { items: [], problem: problems[source] };
      if (q.ids?.length) return { items: list.filter((i) => q.ids!.some((v) => v === i.id || (i.key !== undefined && v.toLowerCase() === i.key.toLowerCase()))) };
      const needle = (q.q ?? "").trim().toLowerCase();
      const hits = needle ? list.filter((i) => [i.label, i.key, i.hint].some((s) => s?.toLowerCase().includes(needle))) : list;
      const from = q.cursor ? Number(q.cursor) || 0 : 0;
      const limit = Math.min(q.limit ?? 50, LOOKUP_LIMITS.limit);
      return { items: hits.slice(from, from + limit), ...(from + limit < hits.length ? { next: String(from + limit) } : {}) };
    },
    create: async (_owner, source, kind, req) => {
      all(source, kind, req.parent);
      const name = req.name.trim();
      if (!name) throw new ApiError("say the new one's name (`name`)", 400, null);
      const item: LookupItem = { id: `${kind}_${name.toLowerCase().replace(/[^a-z0-9]+/g, "_")}_${made.length + 1}`, label: name, key: name, ...(req.color ? { color: req.color } : {}) };
      const k = `${source}/${kind}/${req.parent ?? ""}`;
      extra.set(k, [...(extra.get(k) ?? []), item]);
      made.push(item);
      return item;
    },
  };
}
