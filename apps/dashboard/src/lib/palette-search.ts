/**
 * The command palette's search, ported from T3 Code's `packages/shared/src/paletteSearch.ts`
 * (MIT): case- and accent-folded word-start matching, joined words ("vscode" for "VS Code"), a
 * typo pass only when nothing matches strongly, thread use (recent activity, opens, and what the
 * user picked for a query) reordering matches of one class, and groups ranked by their best match
 * class so the list stays put while typing. Left out: message search (T3 asks its server's index;
 * roll has none) and pull request references (a roll thread has one change, matched as `#N`).
 */

const ALPHANUMERIC = /[\p{L}\p{N}]/u;
const LOWERCASE = /\p{Ll}/u;
const UPPERCASE = /\p{Lu}/u;
const DIGIT = /\p{N}/u;
const LETTER = /\p{L}/u;
const COMBINING_MARKS = /\p{M}/gu;
// Emoji and other symbols can be searched for, so they start words too.
const SYMBOL = /\p{So}/u;
const APOSTROPHE = /['’]/;

interface SearchField {
  /** Case- and accent-folded text, one UTF-16 unit per source unit. */
  readonly text: string;
  /** 1 where a word starts in `text`. */
  readonly wordStarts: Uint8Array;
}

export interface SearchRange {
  readonly start: number;
  readonly end: number;
}

// Titles and names repeat across keystrokes; folding them once keeps a keystroke to plain scans.
const fieldCache = new Map<string, SearchField>();
const FIELD_CACHE_LIMIT = 20_000;

function foldCharacter(character: string): string {
  // A typed straight apostrophe finds a curly one: iOS keyboards type "’".
  return character.normalize("NFKD").replace(COMBINING_MARKS, "").toLowerCase().replace("ς", "σ").replace(/[‘’]/, "'");
}

/**
 * `text` with case and accents folded one UTF-16 unit at a time, so offsets and word starts still
 * read from `text` and highlights need no mapping. A letter that folds to more or less than one
 * unit ("ß", "ﬁ") stays as it is.
 */
function foldInPlace(text: string): string {
  return text
    .replace(/[\u0080-￿]/g, (character) => {
      const folded = foldCharacter(character);
      return folded.length === 1 ? folded : character;
    })
    .toLowerCase();
}

// Chinese and Japanese text has no spaces, so each ideograph or kana starts a word.
function isCjk(character: string): boolean {
  const code = character.codePointAt(0)!;
  return (code >= 0x3040 && code <= 0x30ff) || (code >= 0x3400 && code <= 0x4dbf) || (code >= 0x4e00 && code <= 0x9fff) || (code >= 0xf900 && code <= 0xfaff);
}

/** Whether `character` starts a word: after a non-letter, at a camelCase or digit boundary, or at any CJK character. */
function isWordStart(beforePrevious: string, previous: string, character: string, next: string): boolean {
  // "Julius’s": a possessive or contraction does not start a word.
  const afterApostrophe = APOSTROPHE.test(previous) && ALPHANUMERIC.test(beforePrevious);
  return (
    (ALPHANUMERIC.test(character) || SYMBOL.test(character)) &&
    !afterApostrophe &&
    (previous === "" ||
      !ALPHANUMERIC.test(previous) ||
      (LOWERCASE.test(previous) && UPPERCASE.test(character)) ||
      // The last capital of an acronym starts the next word: "JSONSchema".
      (UPPERCASE.test(previous) && UPPERCASE.test(character) && LOWERCASE.test(next)) ||
      DIGIT.test(previous) !== DIGIT.test(character) ||
      isCjk(character))
  );
}

function foldSearchField(source: string): SearchField {
  const wordStarts = new Uint8Array(source.length);
  const characters = Array.from(source);
  let index = 0;
  for (const [position, character] of characters.entries()) {
    if (isWordStart(characters[position - 2] ?? "", characters[position - 1] ?? "", character, characters[position + 1] ?? "")) wordStarts[index] = 1;
    index += character.length;
  }
  return { text: foldInPlace(source), wordStarts };
}

function prepareSearchField(source: string): SearchField {
  const cached = fieldCache.get(source);
  if (cached) return cached;
  if (fieldCache.size >= FIELD_CACHE_LIMIT) fieldCache.delete(fieldCache.keys().next().value!);
  const field = foldSearchField(source);
  fieldCache.set(source, field);
  return field;
}

// Quotes around a pasted query: "fix sidebar".
const WRAPPING_QUOTES = /^\s*["'`“”‘’]+|["'`“”‘’]+\s*$/g;

/** Folded, whitespace-collapsed query; empty when there is nothing to match. */
export function foldSearchQuery(query: string): string {
  return foldInPlace(query.replace(WRAPPING_QUOTES, "").normalize("NFC"))
    .split(/\s+/)
    .filter((token) => token.length > 0)
    .join(" ");
}

function wordEnd(field: SearchField, index: number): number {
  let end = index + 1;
  while (end < field.text.length && !field.wordStarts[end] && ALPHANUMERIC.test(field.text[end]!)) end++;
  return end;
}

// One substitution, insertion, deletion, or swap of neighbours; with `swapOnly`, only a swap.
function withinOneEdit(left: string, right: string, swapOnly: boolean): boolean {
  if (Math.abs(left.length - right.length) > (swapOnly ? 0 : 1)) return false;
  let index = 0;
  while (index < left.length && index < right.length && left[index] === right[index]) index++;
  const restLeft = left.slice(index);
  const restRight = right.slice(index);
  const swapped = restLeft.length > 1 && restLeft[0] === restRight[1] && restLeft[1] === restRight[0] && restLeft.slice(2) === restRight.slice(2);
  if (swapOnly) return swapped;
  return swapped || restLeft === restRight || restLeft.slice(1) === restRight.slice(1) || restLeft.slice(1) === restRight || restLeft === restRight.slice(1);
}

const WHOLE_WORD = 1;
// The bonus for a field equal to the whole query lifts a whole word to this.
const WHOLE_NAME = 1.5;
const MID_WORD = 0.3;
const TYPO = 0.45;
// Whole words typed together are exact, so they rank with typos and above mid-word hits.
const JOINED_WORDS = 0.6;

interface TokenMatch extends SearchRange {
  readonly score: number;
  /** Separate ranges, for words typed together. */
  readonly letters?: ReadonlyArray<SearchRange>;
}

/**
 * Mid-word hits need four or more characters and `midWord`, so "del" doesn't find "model". Typos
 * keep the first letter except for swaps; short tokens allow fewer edits; numbers never match with
 * a typo.
 */
function matchToken(field: SearchField, token: string, typos: boolean, isPrimary: boolean, midWord: boolean): TokenMatch | null {
  if (token.length === 0) return null;
  let best: TokenMatch | null = null;
  let ownBoundary: boolean | undefined;
  for (let index = field.text.indexOf(token); index !== -1; index = field.text.indexOf(token, index + 1)) {
    // A token that opens with punctuation (".ts") brings its own boundary.
    if (field.wordStarts[index] || (ownBoundary ??= !ALPHANUMERIC.test(token[0]!) && !SYMBOL.test(token[0]!))) {
      const end = index + token.length;
      const whole = end >= field.text.length || field.wordStarts[end] === 1 || !ALPHANUMERIC.test(field.text[end]!);
      let runEnd = end;
      while (runEnd < field.text.length && ALPHANUMERIC.test(field.text[runEnd]!)) runEnd++;
      const score = whole ? WHOLE_WORD : 0.7 + (0.2 * token.length) / (runEnd - index);
      if (best === null || score > best.score) best = { score, start: index, end };
      if (score === WHOLE_WORD) return best;
    } else if (midWord && token.length >= 4 && best === null) {
      best = { score: MID_WORD, start: index, end: index + token.length };
    }
  }
  // Joined words are exact, so they count without the typo pass.
  if ((best === null || best.score <= MID_WORD) && isPrimary) {
    const joined = matchJoinedWords(field, token);
    if (joined) return joined;
  }
  if ((best !== null && best.score > MID_WORD) || !typos || token.length < 3) return best;
  if (LETTER.test(token)) {
    for (let index = 0; index < field.text.length; index++) {
      if (!field.wordStarts[index]) continue;
      // People rarely mistype the first letter, unless they swap the first two of a longer word.
      const first = field.text[index];
      const firstLetterKept = first === token[0] || (token.length > 3 && first === token[1] && field.text[index + 1] === token[0]);
      if (!firstLetterKept) continue;
      let runEnd = index + 1;
      while (runEnd < field.text.length && ALPHANUMERIC.test(field.text[runEnd]!)) runEnd++;
      const word = field.text.slice(index, runEnd);
      const short = token.length <= 4;
      const lengths = short ? [token.length, token.length + 1] : [token.length, token.length - 1, token.length + 1];
      for (const length of lengths) {
        if (length > word.length) continue;
        if (withinOneEdit(token, word.slice(0, length), short && length === token.length)) return { score: TYPO, start: index, end: index + length };
      }
    }
  }
  return best;
}

/** Whole words typed together ("vscode", "t3co"); short-word noise inside a title doesn't count. */
function matchJoinedWords(field: SearchField, token: string): TokenMatch | null {
  for (let start = field.text.indexOf(token[0]!); start !== -1; start = field.text.indexOf(token[0]!, start + 1)) {
    if (!field.wordStarts[start]) continue;
    const runs: SearchRange[] = [];
    let at = start;
    let rest = token;
    while (rest.length > 0 && at < field.text.length) {
      const end = wordEnd(field, at);
      if (rest.length <= end - at && field.text.startsWith(rest, at)) {
        runs.push({ start: at, end: at + rest.length });
        rest = "";
      } else if (rest.startsWith(field.text.slice(at, end))) {
        runs.push({ start: at, end });
        rest = rest.slice(end - at);
        at = end;
        while (at < field.text.length && !field.wordStarts[at]) at++;
      } else {
        break;
      }
    }
    const last = runs.at(-1);
    if (rest.length > 0 || last === undefined || runs.length < 2) continue;
    if (last.end === wordEnd(field, last.start) || last.end - last.start >= 3 || start === field.wordStarts.indexOf(1) || runs.slice(0, -1).every((run) => run.end - run.start >= 3)) {
      return { score: JOINED_WORDS, start, end: last.end, letters: runs };
    }
  }
  return null;
}

interface WeightedSearchField {
  readonly value: string;
  prepared?: SearchField;
  readonly weight: number;
  /** Only a whole-word match counts. */
  readonly wholeWords?: boolean;
  /** Only a word-start match counts, without typos: hidden keywords that matched loosely would list results the user can't see why for. */
  readonly keyword?: boolean;
  /** Where the thread lives (project, machine): it helps a match but never makes it strong. */
  readonly context?: boolean;
  /** A token of four or more letters may match inside a word (thread titles only). */
  readonly midWord?: boolean;
}

interface SearchFieldsMatch {
  /** Roughly 0.3 (inside a word) to 1.5 (exact title). */
  readonly quality: number;
  /** Every token matched at a word start, without a typo. */
  readonly strong: boolean;
  /** Ranges matched in the first field, for highlighting. */
  readonly primaryRanges: ReadonlyArray<SearchRange>;
}

const preparedField = (weighted: WeightedSearchField) => (weighted.prepared ??= prepareSearchField(weighted.value));

const FILLER_WORDS = new Set(["a", "an", "and", "for", "in", "of", "on", "the", "to", "with"]);

function mergeRanges(ranges: ReadonlyArray<SearchRange>): SearchRange[] {
  const merged: SearchRange[] = [];
  for (const range of [...ranges].sort((left, right) => left.start - right.start)) {
    const last = merged.at(-1);
    if (last && range.start <= last.end) merged[merged.length - 1] = { start: last.start, end: Math.max(last.end, range.end) };
    else merged.push({ start: range.start, end: range.end });
  }
  return merged;
}

/**
 * Every query token must match some field. Quality is the mean of each token's best weighted
 * match, plus a bonus when the first field holds the whole query as a phrase or equals it.
 */
function matchSearchFields(normalizedQuery: string, fields: ReadonlyArray<WeightedSearchField>, options?: { readonly typos?: boolean }): SearchFieldsMatch | null {
  if (normalizedQuery.length === 0) return null;
  const primary = fields[0]?.value ? preparedField(fields[0]) : undefined;
  const tokens = normalizedQuery.split(" ").filter((token) => token.length > 0);
  if (tokens.length === 0) return null;
  // A phrase-style query ("fix the sidebar") carries filler words a name may leave out.
  const fillerOptional = tokens.some((token) => !FILLER_WORDS.has(token));
  const primaryRanges: SearchRange[] = [];
  let total = 0;
  let counted = 0;
  let strong = true;
  for (const token of tokens) {
    let best = 0;
    let bestPrimary: TokenMatch | null = null;
    let atWordStart = false;
    for (const weighted of fields) {
      if (weighted.value.length === 0) continue;
      const { weight, wholeWords, keyword, context, midWord } = weighted;
      const field = preparedField(weighted);
      if (best >= weight && field !== primary && atWordStart) continue;
      const typos = (options?.typos ?? false) && !keyword;
      const match = matchToken(field, token, typos, field === primary, midWord ?? false);
      if (match === null || match.score < (wholeWords ? WHOLE_WORD : keyword ? 0.7 : 0)) continue;
      if (field === primary) bestPrimary = match;
      if (match.score >= 0.7 && !context) atWordStart = true;
      if (match.score * weight > best) best = match.score * weight;
    }
    if (best === 0) {
      if (fillerOptional && FILLER_WORDS.has(token)) continue;
      return null;
    }
    strong &&= atWordStart;
    if (bestPrimary) primaryRanges.push(...(bestPrimary.letters ?? [bestPrimary]));
    total += best;
    counted += 1;
  }
  let quality = total / counted;
  if (primary) {
    if (primary.text === normalizedQuery) quality += WHOLE_NAME - WHOLE_WORD;
    else if (tokens.length > 1 && primary.text.includes(normalizedQuery)) quality += 0.25;
  }
  return { quality, strong, primaryRanges: mergeRanges(primaryRanges) };
}

export interface SearchTextPart {
  readonly text: string;
  readonly highlighted: boolean;
  readonly start: number;
}

/** `text` cut into plain and highlighted parts at sorted, non-overlapping `ranges`. */
export function splitSearchText(text: string, ranges: ReadonlyArray<SearchRange>): SearchTextPart[] {
  const parts: SearchTextPart[] = [];
  let cursor = 0;
  for (const range of ranges) {
    if (range.start > cursor) parts.push({ text: text.slice(cursor, range.start), highlighted: false, start: cursor });
    parts.push({ text: text.slice(range.start, range.end), highlighted: true, start: range.start });
    cursor = range.end;
  }
  if (cursor < text.length || parts.length === 0) parts.push({ text: text.slice(cursor), highlighted: false, start: cursor });
  return parts;
}

// ── thread use ──

const DAY_MS = 24 * 60 * 60 * 1000;
/** Both activity recency and open counts halve every two weeks. */
export const SEARCH_RECENCY_HALF_LIFE_MS = 14 * DAY_MS;
/** Opens closer together than this count once, so flipping between threads is not frequency. */
export const THREAD_OPEN_DEDUPE_MS = 10 * 60 * 1000;
const THREAD_SEARCH_PICK_LIMIT = 3;
const THREAD_SEARCH_PICK_MAX_LENGTH = 32;
// Picks expire from the pick time; they reorder word matches within a group, never the groups.
const PICK_WINDOW_MS = 21 * DAY_MS;
const PICK_PREFIX_MIN_LENGTH = 3;

/** `query` as a pick is stored and compared: folded, then cut without splitting an emoji. */
export function pickedQuery(query: string): string {
  const cut = foldSearchQuery(query).slice(0, THREAD_SEARCH_PICK_MAX_LENGTH);
  return (/[\ud800-\udbff]$/.test(cut) ? cut.slice(0, -1) : cut).trim();
}

const decay = (ageMs: number) => 2 ** (-Math.max(0, ageMs) / SEARCH_RECENCY_HALF_LIFE_MS);

/** How often and how lately a thread was opened, and the queries it was picked for (T3's `OrchestrationV2ThreadOpens`). */
export interface ThreadOpenStats {
  readonly openedAt: number;
  readonly score: number;
  readonly picks: ReadonlyArray<{ readonly query: string; readonly at: number }>;
}

/** Next open stats after an open at `at`; `query` is the palette query that picked it, if any. */
export function recordThreadOpen(previous: ThreadOpenStats | null, at: number, query: string | undefined): ThreadOpenStats {
  const pick = pickedQuery(query ?? "");
  const earlier = (previous?.picks ?? []).filter((entry) => at - entry.at <= PICK_WINDOW_MS);
  const picks = pick ? [{ query: pick, at }, ...earlier.filter((entry) => entry.query !== pick)].slice(0, THREAD_SEARCH_PICK_LIMIT) : earlier;
  if (previous !== null && at - previous.openedAt < THREAD_OPEN_DEDUPE_MS) return { ...previous, picks };
  const score = previous === null ? 1 : previous.score * decay(at - previous.openedAt) + 1;
  return { openedAt: at, score, picks };
}

export interface ThreadSearchSubject {
  readonly title: string;
  readonly branch?: string | null;
  /** The roll code change it was published as: `#41`. */
  readonly change?: { readonly number: number; readonly repo?: string | null } | null;
  readonly projectTitle?: string | null;
  /** Only set with two or more machines; it tells same-named threads apart. */
  readonly machineLabel?: string | null;
  /** Latest activity, in epoch ms. */
  readonly activityAt: number | null;
  readonly opens?: ThreadOpenStats | null;
}

// Use reorders matches of about the same quality but never lifts a mid-word hit over a whole word.
const USE_BOOST_LIMIT = 2.5;

/** 1 for a stale, never-opened thread, about 2 for one active today, up to 2.5 for threads opened often. */
function threadUsageBoost(thread: ThreadSearchSubject, now: number): number {
  const recency = thread.activityAt === null ? 0 : decay(now - thread.activityAt);
  const opens = thread.opens ? thread.opens.score * decay(now - thread.opens.openedAt) : 0;
  return Math.min(USE_BOOST_LIMIT, 1 + recency + 0.5 * Math.log2(1 + opens));
}

function activePick(thread: ThreadSearchSubject, normalizedQuery: string, now: number): ThreadOpenStats["picks"][number] | undefined {
  const query = pickedQuery(normalizedQuery);
  const prefix = query.length >= PICK_PREFIX_MIN_LENGTH;
  let exact: ThreadOpenStats["picks"][number] | undefined;
  let latest: ThreadOpenStats["picks"][number] | undefined;
  for (const pick of thread.opens?.picks ?? []) {
    if (now - pick.at > PICK_WINDOW_MS) continue;
    if (pick.query === query && (exact === undefined || pick.at > exact.at)) exact = pick;
    const matches = pick.query === query || (prefix && pick.query.startsWith(query));
    if (matches && (latest === undefined || pick.at > latest.at)) latest = pick;
  }
  return exact ?? latest;
}

export interface SearchMatchHint {
  readonly label: string;
  readonly text: string;
  readonly ranges: ReadonlyArray<SearchRange>;
}

export interface ThreadSearchMatch {
  /** Match quality times use; what results sort by. */
  readonly score: number;
  readonly quality: number;
  readonly strong: boolean;
  /** Matched ranges in the title. */
  readonly ranges: ReadonlyArray<SearchRange>;
  /** Why the thread matched when its title shows no match, built only for the rows shown. */
  readonly explain?: () => SearchMatchHint | undefined;
  readonly pickedAt?: number;
  readonly pickedExactly?: boolean;
  /** Below the whole-name class: a thread's title never lifts its group over a command of that name. */
  readonly groupQuality: number;
}

// roll's worktree branches all start "roll/", so "roll status" would match nearly every thread.
const WORKTREE_BRANCH_PREFIX_PATTERN = /^roll\/(?=\S)/;
const searchableBranch = (branch: string | null | undefined) => (branch ? branch.replace(WORKTREE_BRANCH_PREFIX_PATTERN, "") : null);

function matchHint(normalizedQuery: string, thread: ThreadSearchSubject, typos: boolean): SearchMatchHint | undefined {
  const candidates: Array<{ readonly label: string; readonly text: string | null | undefined }> = [
    { label: "Change", text: thread.change ? `#${thread.change.number}${thread.change.repo ? ` ${thread.change.repo}` : ""}` : null },
    { label: "Branch", text: searchableBranch(thread.branch) },
    { label: "Project", text: thread.projectTitle },
    { label: "Machine", text: thread.machineLabel },
  ];
  const holds = (text: string, query: string) => matchSearchFields(query, [{ value: text, weight: 1 }], { typos })?.primaryRanges;
  const hintFor = (query: string) => {
    for (const { label, text } of candidates) {
      const ranges = text ? holds(text, query) : undefined;
      if (text && ranges) return { label, text, ranges };
    }
    return undefined;
  };
  const whole = hintFor(normalizedQuery);
  if (whole) return whole;
  const contexts = [thread.projectTitle, thread.machineLabel].filter((text): text is string => !!text);
  const rest = normalizedQuery
    .split(" ")
    .filter((token) => !contexts.some((text) => holds(text, token)))
    .join(" ");
  return rest && rest !== normalizedQuery ? hintFor(rest) : undefined;
}

const threadFieldsCache = new WeakMap<ThreadSearchSubject, ReadonlyArray<WeightedSearchField>>();

function threadSearchFields(thread: ThreadSearchSubject): ReadonlyArray<WeightedSearchField> {
  const cached = threadFieldsCache.get(thread);
  if (cached) return cached;
  const fields: WeightedSearchField[] = [{ value: thread.title, weight: 1, midWord: true }];
  if (thread.change) fields.push({ value: `#${thread.change.number}`, weight: 1 });
  const branch = searchableBranch(thread.branch);
  if (branch) fields.push({ value: branch, weight: 0.6 });
  if (thread.projectTitle) fields.push({ value: thread.projectTitle, weight: 0.25, context: true });
  if (thread.machineLabel) fields.push({ value: thread.machineLabel, weight: 0.25, context: true });
  threadFieldsCache.set(thread, fields);
  return fields;
}

// "change 41", "#41" and "roll/…" match the stored fields.
const threadFieldQuery = (normalizedQuery: string) =>
  normalizedQuery
    .replace(/^(?:change|pr|pull request|pull)\s*(?=#?\d)/, "")
    .replace(/^#\s*/, "")
    .replace(WORKTREE_BRANCH_PREFIX_PATTERN, "");

/**
 * Scores a thread for a normalized query, or null when it doesn't match. Titles and change
 * numbers count fully, branches less, project and machine names least.
 */
export function scoreThreadSearch(normalizedQuery: string, thread: ThreadSearchSubject, now: number, options?: { readonly typos?: boolean; readonly picks?: boolean }): ThreadSearchMatch | null {
  const fieldQuery = threadFieldQuery(normalizedQuery);
  const match = matchSearchFields(fieldQuery, threadSearchFields(thread), options);
  if (match === null) return null;
  // A row whose title shows no match must say why; with several words, one that can't is a coincidence.
  const typos = options?.typos ?? false;
  const titleShowsMatch = match.primaryRanges.length > 0;
  const checkHint = !titleShowsMatch && fieldQuery.includes(" ");
  const checkedHint = checkHint ? matchHint(fieldQuery, thread, typos) : undefined;
  if (checkHint && checkedHint === undefined) return null;
  const explain = titleShowsMatch ? undefined : () => checkedHint ?? matchHint(fieldQuery, thread, typos);
  const pick = options?.picks === false ? undefined : activePick(thread, normalizedQuery, now);
  return {
    score: match.quality * threadUsageBoost(thread, now),
    quality: match.quality,
    groupQuality: Math.min(match.quality, WHOLE_NAME - 0.01),
    strong: match.strong,
    ranges: match.primaryRanges,
    ...(explain ? { explain } : {}),
    ...(pick ? { pickedAt: pick.at } : {}),
    ...(pick?.query === pickedQuery(normalizedQuery) ? { pickedExactly: true } : {}),
  };
}

/** Palette limits (T3's). */
export const PALETTE_THREAD_LIMIT = 20;
/** Next to matching threads, other groups show only their best few rows. */
export const PALETTE_SECONDARY_LIMIT = 3;

// Whole words and prefixes share a class so "Compact" doesn't lift one group over "Compaction".
function matchClass(quality: number): number {
  return quality >= WHOLE_NAME ? 3 : quality >= 0.7 ? 2 : quality >= 0.4 ? 1 : 0;
}

export interface RankedSearchItem<T, M> {
  readonly item: T;
  readonly match: M;
}

interface RankMatch {
  readonly score: number;
  readonly quality: number;
  readonly strong: boolean;
  readonly pickedAt?: number;
  readonly pickedExactly?: boolean;
  readonly groupQuality?: number;
}

/**
 * Rank groups by match class to keep typing stable and thread use from outweighing equal
 * commands. Typos are tried only without word-start matches: they add noise beside real ones.
 */
export function rankSearchGroups<G, T, M extends RankMatch>(
  groups: ReadonlyArray<{ readonly group: G; readonly items: ReadonlyArray<T> }>,
  score: (item: T, options: { readonly typos: boolean }, group: G) => M | null,
): Array<{ readonly group: G; readonly ranked: ReadonlyArray<RankedSearchItem<T, M>> }> {
  const rank = (typos: boolean) =>
    groups.map(({ group, items }) => {
      const ranked: Array<RankedSearchItem<T, M>> = [];
      for (const item of items) {
        const match = score(item, { typos }, group);
        if (match !== null) ranked.push({ item, match });
      }
      return { group, ranked };
    });
  let ranked = rank(false);
  if (!ranked.some((entry) => entry.ranked.some(({ match }) => match.strong))) ranked = rank(true);
  return ranked
    .flatMap(({ group, ranked: items }, order) => {
      if (items.length === 0) return [];
      // Use can't lift a project-only match above a title match or cross match classes; picks
      // reorder word matches, keeping typos and mid-word hits below them.
      const wordMatch = (match: M) => Math.min(matchClass(match.quality), 2);
      const exactPickAt = (match: M) => (match.pickedExactly ? (match.pickedAt ?? 0) : 0);
      const sorted = [...items].sort(
        (left, right) =>
          wordMatch(right.match) - wordMatch(left.match) ||
          exactPickAt(right.match) - exactPickAt(left.match) ||
          matchClass(right.match.quality) - matchClass(left.match.quality) ||
          (right.match.pickedAt ?? 0) - (left.match.pickedAt ?? 0) ||
          right.match.score - left.match.score,
      );
      const best = Math.max(...items.map(({ match }) => matchClass(match.groupQuality ?? match.quality)));
      return [{ group, ranked: sorted, best, order }];
    })
    .sort((left, right) => right.best - left.best || left.order - right.order)
    .map(({ group, ranked: items }) => ({ group, ranked: items }));
}

export interface SearchTermsMatch {
  readonly score: number;
  readonly quality: number;
  readonly groupQuality?: number;
  readonly strong: boolean;
  readonly ranges: ReadonlyArray<SearchRange>;
}

const KEYWORD_QUALITY = 0.65;
// Below this, keyword prefixes match too much; a short query still matches a keyword it equals.
const KEYWORD_QUERY_MIN_LENGTH = 4;

// Words people put before a command or a name ("open the shelf", "go to settings", "show roll").
const COMMAND_VERBS = new Set(["open", "show", "go", "goto", "view", "see", "display", "find", "jump"]);

/**
 * A command or name: the first term is its name, the rest are keywords that count less. A leading
 * verb the command doesn't hold ("open the shelf" for "Shelf") is dropped, so a phrase that names
 * a command runs it rather than reading as a goal.
 */
export function scoreSearchTerms(normalizedQuery: string, terms: ReadonlyArray<string>, options?: { readonly typos?: boolean }): SearchTermsMatch | null {
  const match = scoreTerms(normalizedQuery, terms, options);
  if (match) return match;
  const [verb, ...rest] = normalizedQuery.split(" ");
  while (rest.length > 1 && FILLER_WORDS.has(rest[0]!)) rest.shift();
  if (!verb || !COMMAND_VERBS.has(verb) || rest.length === 0) return null;
  return scoreTerms(rest.join(" "), terms, options);
}

function scoreTerms(normalizedQuery: string, terms: ReadonlyArray<string>, options?: { readonly typos?: boolean }): SearchTermsMatch | null {
  const meaningful = normalizedQuery
    .split(" ")
    .filter((token) => !FILLER_WORDS.has(token))
    .join(" ");
  const short = (meaningful || normalizedQuery).length < KEYWORD_QUERY_MIN_LENGTH;
  const match = matchSearchFields(
    normalizedQuery,
    terms.map((value, index) => (index === 0 ? { value, weight: 1 } : { value, weight: short ? 0.8 : 0.6, wholeWords: short, keyword: true })),
    options,
  );
  return (
    match && {
      score: match.quality,
      quality: match.quality,
      // A match only in hidden keywords must not put its group above one whose name matches.
      ...(match.primaryRanges.length > 0 ? {} : { groupQuality: Math.min(match.quality, KEYWORD_QUALITY) }),
      strong: match.strong,
      ranges: match.primaryRanges,
    }
  );
}
