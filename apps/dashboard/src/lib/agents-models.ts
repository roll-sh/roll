/**
 * Favourite and recently used models, and how the model picker orders and searches them.
 * Ported from T3 Code's `apps/web/src/modelOrdering.ts` and `components/chat/modelPickerSearch.ts`
 * (MIT): favourites first in a provider's list; a search across every provider that ranks exact
 * names, then prefixes, word starts, substrings and (3+ letters) fuzzy matches, favourites boosted.
 *
 * T3 keeps favourites in its server settings so they follow the user. Here they're kept in this
 * browser (localStorage) for now; `modelPrefs` is the one seam to move to an owner-level record.
 * T3 has no recent list (only a sticky last pick); roll keeps the last few picked.
 */

import { useSyncExternalStore } from "react";

/** A model of one provider: `claude:claude-opus-5-5` (T3's `providerModelKey`). */
export type ModelKey = string;

export const modelKey = (provider: string, model: string): ModelKey => `${provider}:${model}`;

export function parseModelKey(key: ModelKey): { provider: string; model: string } | null {
  const i = key.indexOf(":");
  return i > 0 && i < key.length - 1 ? { provider: key.slice(0, i), model: key.slice(i + 1) } : null;
}

/** How many recently used models are kept. */
export const MAX_RECENT = 5;
const MAX_FAVORITES = 64;

export interface ModelPrefs {
  favorites: readonly ModelKey[];
  /** Newest first. */
  recent: readonly ModelKey[];
}

const FAVORITES = "roll:agents:favorite-models";
const RECENT = "roll:agents:recent-models";
const EMPTY: ModelPrefs = { favorites: [], recent: [] };

let cached: ModelPrefs | null = null;
const listeners = new Set<() => void>();

function readList(key: string, max: number): ModelKey[] {
  try {
    const v: unknown = JSON.parse(localStorage.getItem(key) ?? "[]");
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string" && parseModelKey(x) !== null).slice(0, max) : [];
  } catch {
    return [];
  }
}

function read(): ModelPrefs {
  if (typeof localStorage === "undefined") return EMPTY;
  cached ??= { favorites: readList(FAVORITES, MAX_FAVORITES), recent: readList(RECENT, MAX_RECENT) };
  return cached;
}

function write(next: ModelPrefs) {
  cached = next;
  try {
    localStorage.setItem(FAVORITES, JSON.stringify(next.favorites));
    localStorage.setItem(RECENT, JSON.stringify(next.recent));
  } catch {
    // Kept for this page only.
  }
  for (const l of listeners) l();
}

export const modelPrefs = {
  get: read,
  subscribe(fn: () => void) {
    listeners.add(fn);
    // Another tab changed them.
    const onStorage = (e: StorageEvent) => {
      if (e.key !== FAVORITES && e.key !== RECENT && e.key !== null) return;
      cached = null;
      fn();
    };
    window.addEventListener("storage", onStorage);
    return () => {
      listeners.delete(fn);
      window.removeEventListener("storage", onStorage);
    };
  },
  /** The state wanted, not a toggle, so two tabs can't undo each other (T3's `setModelFavorites`). */
  setFavorite(key: ModelKey, favorite: boolean) {
    const p = read();
    if (p.favorites.includes(key) === favorite) return;
    write({ ...p, favorites: favorite ? [...p.favorites, key].slice(-MAX_FAVORITES) : p.favorites.filter((k) => k !== key) });
  },
  /** A model was picked: it goes to the front of the recent ones. */
  used(key: ModelKey) {
    const p = read();
    if (p.recent[0] === key) return;
    write({ ...p, recent: [key, ...p.recent.filter((k) => k !== key)].slice(0, MAX_RECENT) });
  },
  /** Forget what was read (tests clear localStorage between cases). */
  reset() {
    cached = null;
    for (const l of listeners) l();
  },
};

export function useModelPrefs(): ModelPrefs {
  return useSyncExternalStore(modelPrefs.subscribe, modelPrefs.get, () => EMPTY);
}

/** One row of the picker: a model of an installed provider. */
export interface PickerModel {
  key: ModelKey;
  provider: string;
  providerName: string;
  model: string;
  name: string;
}

/** Favourites first, then in the order given (T3's `sortProviderModelItems` with `groupFavorites`). */
export function sortModels<T extends { key: ModelKey }>(items: readonly T[], favorites: ReadonlySet<ModelKey>): T[] {
  return items
    .map((item, i) => ({ item, i, fav: favorites.has(item.key) }))
    .sort((a, b) => Number(b.fav) - Number(a.fav) || a.i - b.i)
    .map((x) => x.item);
}

const FAVORITE_BOOST = 24;
const EXACT_NAME = -1000;

const normalize = (s: string) => s.toLowerCase().trim().replace(/\s+/g, " ");
const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/** Lower is better; null when `token` isn't in `field` (T3's `scoreQueryMatch` with `ignorePosition`). */
function scoreToken(field: string, token: string, base: number): number | null {
  if (!field) return null;
  if (field === token) return base;
  if (field.startsWith(token)) return base + 2;
  if (new RegExp(`[\\s._/:-]${escape(token)}`).test(field)) return base + 4;
  if (field.includes(token)) return base + 6;
  if (token.length >= 3 && subsequence(field, token)) return base + 100;
  return null;
}

function subsequence(field: string, token: string) {
  let i = 0;
  for (const c of field) if (c === token[i] && ++i === token.length) return true;
  return false;
}

/** T3's `scoreModelPickerSearch`: every word of the query has to match some field. */
export function scoreModel(m: PickerModel, query: string, favorite: boolean): number | null {
  const tokens = normalize(query).split(" ").filter(Boolean);
  if (tokens.length === 0) return 0;
  if (tokens.join(" ") === normalize(m.name)) return EXACT_NAME;
  const fields = [m.name, m.model, m.provider, m.providerName, `${m.name} ${m.model} ${m.providerName}`].map(normalize);
  let score = 0;
  for (const t of tokens) {
    let best: number | null = null;
    for (let i = 0; i < fields.length; i++) {
      const s = scoreToken(fields[i], t, i * 10);
      if (s !== null && (best === null || s < best)) best = s;
    }
    if (best === null) return null;
    score += best;
  }
  return favorite ? score - FAVORITE_BOOST : score;
}

/** The models matching `query`, best first; ties go to favourites, then the order given. */
export function searchModels(items: readonly PickerModel[], query: string, favorites: ReadonlySet<ModelKey>): PickerModel[] {
  const ranked: { item: PickerModel; i: number; fav: boolean; score: number }[] = [];
  items.forEach((item, i) => {
    const fav = favorites.has(item.key);
    const score = scoreModel(item, query, fav);
    if (score !== null) ranked.push({ item, i, fav, score });
  });
  return ranked.sort((a, b) => a.score - b.score || Number(b.fav) - Number(a.fav) || a.i - b.i).map((x) => x.item);
}
