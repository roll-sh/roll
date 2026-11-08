/**
 * Searching settings, ported from T3 Code's `apps/web/src/components/settings/settingsSearch.ts`
 * (MIT): every word of the query must be in the setting's title, its section or one of its search
 * terms (accents and case ignored); the title matching the whole query ranks first, then a title
 * starting with it, containing it, containing every word, then the phrase anywhere; ties keep
 * the panel's order.
 */

export interface SettingItem {
  /** Unique in a panel; also the row's `data-setting` (search results scroll to it). */
  id: string;
  title: string;
  section: string;
  /** Other words people may look for it by (its description, synonyms). */
  terms?: readonly string[];
}

/** NFKD, without combining marks, lowercase, spaces collapsed (T3's `normalizeSearchText`). */
export function normalizeSearch(text: string): string {
  return text
    .normalize("NFKD")
    .replace(/\p{M}/gu, "")
    .toLowerCase()
    .replace(/\s+/g, " ")
    .trim();
}

/** The items matching `query`, best first; none for an empty query. */
export function searchSettings<T extends SettingItem>(query: string, items: readonly T[]): T[] {
  const q = normalizeSearch(query);
  if (!q) return [];
  const words = q.split(" ");
  return items
    .flatMap((item, index) => {
      const title = normalizeSearch(item.title);
      const fields = [title, normalizeSearch(item.section), ...(item.terms ?? []).map(normalizeSearch)];
      if (!words.every((w) => fields.some((f) => f.includes(w)))) return [];
      const rank =
        title === q ? 5 : title.startsWith(q) ? 4 : title.includes(q) ? 3 : words.every((w) => title.includes(w)) ? 2 : fields.some((f) => f.includes(q)) ? 1 : 0;
      return [{ item, index, rank }];
    })
    .sort((a, b) => b.rank - a.rank || a.index - b.index)
    .map((r) => r.item);
}
