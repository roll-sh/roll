/**
 * The Relevance tab's logic: synonym sets (`_synonyms`) and query rules (`_query_rules` and the
 * `rule` retriever), as Elasticsearch's APIs take and answer them. No I/O here: the component
 * sends what these build through the console (`esJson`).
 */

// --- Synonyms --------------------------------------------------------------------------------

/** A rule as `GET _synonyms/<set>` lists it. */
export interface SynonymRule {
  id: string;
  synonyms: string;
}

/** A rule to send: Elasticsearch gives a rule without an id a time-based one. */
export interface SynonymRuleInput {
  id?: string;
  synonyms: string;
}

/** `GET _synonyms`'s rows. */
export interface SynonymSetSummary {
  synonyms_set: string;
  count: number;
}

/** What a rule says: terms that mean the same, or terms rewritten to others. */
export type RuleShape = { kind: "equivalent"; terms: string[] } | { kind: "explicit"; from: string[]; to: string[] };

/** Splits on `sep` outside backslash escapes (Solr's format: `a\,b` is one term). */
function splitUnescaped(s: string, sep: string): string[] {
  const out: string[] = [];
  let cur = "";
  for (let i = 0; i < s.length; i++) {
    if (s[i] === "\\" && i + 1 < s.length) {
      cur += s[i] + s[i + 1];
      i++;
    } else if (s.startsWith(sep, i)) {
      out.push(cur);
      cur = "";
      i += sep.length - 1;
    } else {
      cur += s[i];
    }
  }
  out.push(cur);
  return out;
}

const terms = (side: string) => splitUnescaped(side, ",").map((t) => t.trim());

/** The shape of a rule's text (`"laptop, notebook"`, `"tv => television"`). */
export function ruleShape(text: string): RuleShape {
  const sides = splitUnescaped(text, "=>");
  if (sides.length === 2) return { kind: "explicit", from: terms(sides[0]), to: terms(sides[1]) };
  return { kind: "equivalent", terms: terms(text) };
}

/**
 * Why Elasticsearch would refuse a rule, in its words (`SynonymsManagementAPIService`'s checks,
 * mirrored to flag a row before it's sent), or null. Elasticsearch still decides on save.
 */
export function ruleProblem(text: string): string | null {
  const t = text.trim();
  if (!t) return "A rule can't be empty.";
  const sides = splitUnescaped(t, "=>");
  if (sides.length > 2) return "Only one => per rule.";
  const blank = (side: string) => terms(side).some((x) => !x);
  if (sides.length === 2 ? blank(sides[0]) || blank(sides[1]) : blank(t)) return "A term is empty: check the commas.";
  return null;
}

/** Rules as text, one per line, in the set's order. */
export function rulesToLines(rules: SynonymRule[]): string {
  return rules.map((r) => r.synonyms).join("\n");
}

/**
 * Text lines back to rules: blank lines and `#` comments are skipped, and a line whose text is
 * an existing rule's keeps that rule's id (each id once), so editing a set as text doesn't renumber
 * the rules it kept. New lines get no id (Elasticsearch assigns one).
 */
export function linesToRules(text: string, existing: SynonymRule[]): SynonymRuleInput[] {
  const byText = new Map<string, string[]>();
  for (const r of existing) {
    const k = r.synonyms.trim();
    byText.set(k, [...(byText.get(k) ?? []), r.id]);
  }
  const out: SynonymRuleInput[] = [];
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const id = byText.get(line)?.shift();
    out.push(id ? { id, synonyms: line } : { synonyms: line });
  }
  return out;
}

/** The body of `PUT _synonyms/<set>`. */
export function synonymSetBody(rules: SynonymRuleInput[]): { synonyms_set: SynonymRuleInput[] } {
  return { synonyms_set: rules.map((r) => (r.id ? { id: r.id, synonyms: r.synonyms.trim() } : { synonyms: r.synonyms.trim() })) };
}

/** An update's answer: `result` and, unless `refresh=false`, the reload report. */
export interface SynonymsUpdate {
  result: "created" | "updated" | "deleted" | string;
  reload_analyzers_details?: {
    _shards?: { total: number; successful: number; failed: number };
    reload_details?: { index: string; reloaded_analyzers: string[]; reloaded_node_ids: string[] }[];
  };
}

export interface ReloadSummary {
  /** Each index whose analyzers read the set, with the analyzers reloaded. */
  indices: { index: string; analyzers: string[] }[];
  failedShards: number;
  /** The update in one plain sentence. */
  text: string;
}

const list = (xs: string[]) => (xs.length <= 1 ? xs.join("") : `${xs.slice(0, -1).join(", ")} and ${xs[xs.length - 1]}`);

/** `reload_analyzers_details` in plain words. */
export function reloadSummary(set: string, u: SynonymsUpdate): ReloadSummary {
  const verb = u.result === "created" ? "Created" : u.result === "deleted" ? "Deleted a rule of" : "Saved";
  const details = u.reload_analyzers_details;
  const indices = (details?.reload_details ?? []).map((d) => ({ index: d.index, analyzers: [...d.reloaded_analyzers].sort() }));
  const failedShards = details?._shards?.failed ?? 0;
  let text: string;
  if (!details) {
    text = `${verb} ${set}. Analyzers keep the rules they loaded until they're reloaded.`;
  } else if (indices.length === 0) {
    text = `${verb} ${set}. No index's analyzers use it yet, so there was nothing to reload.`;
  } else {
    const parts = indices.map((i) => `${list(i.analyzers)} in ${i.index}`);
    const n = indices.reduce((a, i) => a + i.analyzers.length, 0);
    text = `${verb} ${set} and reloaded ${n === 1 ? "the analyzer" : `${n} analyzers`} using it: ${list(parts)}. Searches use the new rules now.`;
  }
  if (failedShards > 0) text += ` ${failedShards} shard${failedShards === 1 ? "" : "s"} failed to reload.`;
  return { indices, failedShards, text };
}

/**
 * Index settings that use a synonym set at search time: a `synonym_graph` filter reading the set,
 * `updateable` so saving the set reloads it, in an analyzer the field uses as its
 * `search_analyzer` (updateable filters are only allowed at search time).
 */
export function synonymSettingsSnippet(set: string, field = "title"): string {
  const filter = `${set}_synonyms`;
  const analyzer = `${set}_search`;
  return JSON.stringify(
    {
      settings: {
        analysis: {
          filter: { [filter]: { type: "synonym_graph", synonyms_set: set, updateable: true } },
          analyzer: { [analyzer]: { tokenizer: "standard", filter: ["lowercase", filter] } },
        },
      },
      mappings: { properties: { [field]: { type: "text", analyzer: "standard", search_analyzer: analyzer } } },
    },
    null,
    2,
  );
}

// --- Query rules -----------------------------------------------------------------------------

export const CRITERIA_TYPES = ["exact", "fuzzy", "prefix", "suffix", "contains", "lt", "lte", "gt", "gte", "always"] as const;
export type CriteriaType = (typeof CRITERIA_TYPES)[number];

/** How each criteria type reads in a sentence. */
export const CRITERIA_WORDS: Record<CriteriaType, string> = {
  exact: "is exactly",
  fuzzy: "is close to",
  prefix: "starts with",
  suffix: "ends with",
  contains: "contains",
  lt: "is less than",
  lte: "is at most",
  gt: "is more than",
  gte: "is at least",
  always: "always",
};

const NUMERIC: ReadonlySet<string> = new Set(["lt", "lte", "gt", "gte"]);

export interface Criterion {
  type: CriteriaType;
  metadata?: string;
  values?: (string | number)[];
}

export interface PinnedDoc {
  _index: string;
  _id: string;
}

export interface QueryRule {
  rule_id: string;
  type: "pinned" | "exclude";
  criteria: Criterion[];
  actions: { ids?: string[]; docs?: PinnedDoc[] };
  priority?: number;
}

export interface Ruleset {
  ruleset_id: string;
  rules: QueryRule[];
}

/** `GET _query_rules`'s rows. */
export interface RulesetSummary {
  ruleset_id: string;
  rule_total_count: number;
  rule_type_counts?: Record<string, number>;
  rule_criteria_types_counts?: Record<string, number>;
}

/** A piece of a rule's sentence: words, or something shown as code (metadata, values, ids). */
export type Part = { text: string; code?: boolean };

const quote = (v: string | number) => (typeof v === "number" ? String(v) : `'${v}'`);

/** The documents a rule's action names (`products/doc-1` for a `docs` entry). */
export function actionTargets(rule: QueryRule): string[] {
  if (rule.actions.ids) return rule.actions.ids.map(String);
  return (rule.actions.docs ?? []).map((d) => `${d._index}/${d._id}`);
}

/** A rule as a sentence: "When query_string is exactly 'iphone', pin doc-1, doc-7". */
export function ruleSentence(rule: QueryRule): Part[] {
  const parts: Part[] = [];
  const real = rule.criteria.filter((c) => c.type !== "always");
  if (real.length === 0) {
    parts.push({ text: "Always " });
  } else {
    parts.push({ text: "When " });
    real.forEach((c, i) => {
      if (i > 0) parts.push({ text: " and " });
      parts.push({ text: c.metadata ?? "", code: true });
      parts.push({ text: ` ${CRITERIA_WORDS[c.type] ?? c.type} ` });
      (c.values ?? []).forEach((v, j) => {
        if (j > 0) parts.push({ text: " or " });
        parts.push({ text: quote(v), code: true });
      });
    });
    parts.push({ text: ", " });
  }
  parts.push({ text: rule.type === "exclude" ? "hide " : "pin " });
  actionTargets(rule).forEach((t, i) => {
    if (i > 0) parts.push({ text: ", " });
    parts.push({ text: t, code: true });
  });
  return parts;
}

export const sentenceText = (parts: Part[]) => parts.map((p) => p.text).join("");

/** A rule as the editor holds it: lists as text, so typing is never fought. */
export interface RuleDraft {
  rule_id: string;
  type: "pinned" | "exclude";
  criteria: { type: CriteriaType; metadata: string; values: string }[];
  /** `ids` pins by id in every index searched; `docs` names each document's index too. */
  mode: "ids" | "docs";
  ids: string;
  docs: PinnedDoc[];
  priority: string;
}

/** Comma-separated values, trimmed, blanks dropped. */
export const splitList = (s: string) =>
  s
    .split(",")
    .map((x) => x.trim())
    .filter(Boolean);

export function emptyDraft(ruleId = ""): RuleDraft {
  return { rule_id: ruleId, type: "pinned", criteria: [{ type: "exact", metadata: "query_string", values: "" }], mode: "ids", ids: "", docs: [{ _index: "", _id: "" }], priority: "" };
}

export function ruleToDraft(rule: QueryRule): RuleDraft {
  return {
    rule_id: rule.rule_id,
    type: rule.type,
    criteria: rule.criteria.map((c) => ({ type: c.type, metadata: c.metadata ?? "", values: (c.values ?? []).map(String).join(", ") })),
    mode: rule.actions.docs ? "docs" : "ids",
    ids: (rule.actions.ids ?? []).join(", "),
    docs: rule.actions.docs?.length ? rule.actions.docs.map((d) => ({ ...d })) : [{ _index: "", _id: "" }],
    priority: rule.priority == null ? "" : String(rule.priority),
  };
}

/**
 * The body of `PUT _query_rules/<ruleset>/_rule/<rule_id>` (and a ruleset's rule). Values of the
 * numeric comparisons go as numbers; everything else as typed. Throws on a priority that isn't a
 * whole number; Elasticsearch validates the rest.
 */
export function draftToRule(d: RuleDraft): QueryRule {
  const rule: QueryRule = {
    rule_id: d.rule_id.trim(),
    type: d.type,
    criteria: d.criteria.map((c) => {
      if (c.type === "always") return { type: "always" };
      const values = splitList(c.values).map((v) => (NUMERIC.has(c.type) && v !== "" && Number.isFinite(Number(v)) ? Number(v) : v));
      return { type: c.type, metadata: c.metadata.trim(), values };
    }),
    actions:
      d.mode === "ids"
        ? { ids: splitList(d.ids) }
        : { docs: d.docs.map((x) => ({ _index: x._index.trim(), _id: x._id.trim() })).filter((x) => x._index || x._id) },
  };
  const p = d.priority.trim();
  if (p !== "") {
    if (!/^\d+$/.test(p)) throw new Error("Priority is a whole number from 0 to 1000000 (lower runs first).");
    rule.priority = Number(p);
  }
  return rule;
}

/**
 * The ruleset with rule `from` moved to `to`, each rule's `priority` its new position (0 first).
 * Elasticsearch orders rules by priority, so this is what keeps the order once saved.
 */
export function moveRule(rules: QueryRule[], from: number, to: number): QueryRule[] {
  const next = [...rules];
  const [r] = next.splice(from, 1);
  next.splice(Math.max(0, Math.min(to, next.length)), 0, r);
  return next.map((x, i) => ({ ...x, priority: i }));
}

/** The metadata keys a ruleset's criteria read, for the test form (in first-seen order). */
export function criteriaKeys(rules: QueryRule[]): string[] {
  const keys: string[] = [];
  for (const r of rules) for (const c of r.criteria) if (c.metadata && !keys.includes(c.metadata)) keys.push(c.metadata);
  return keys;
}

/** A test of a ruleset: the text to search, and the match criteria a client would send. */
export interface RuleTest {
  index: string;
  ruleset: string;
  query: string;
  criteria: { key: string; value: string }[];
  size?: number;
}

/**
 * Criteria as `match_criteria`, blank keys dropped. Values stay strings: the numeric comparisons
 * read a number from a string, while `prefix`, `contains` and the like never match a number.
 */
export function matchCriteria(criteria: RuleTest["criteria"]): Record<string, string> {
  const out: Record<string, string> = {};
  for (const { key, value } of criteria) {
    const k = key.trim();
    if (k) out[k] = value;
  }
  return out;
}

/**
 * The `_search` request a test runs: a `rule` retriever around a plain query (every document when
 * the text is empty), so the result is the organic hits with the rules' pins on top and their
 * exclusions gone.
 */
export function ruleTestRequest(t: RuleTest): { path: string; body: unknown } {
  const query = t.query.trim() ? { simple_query_string: { query: t.query.trim() } } : { match_all: {} };
  return {
    path: `/${encodeURIComponent(t.index)}/_search`,
    body: {
      size: t.size ?? 10,
      retriever: {
        rule: {
          ruleset_ids: [t.ruleset],
          match_criteria: matchCriteria(t.criteria),
          retriever: { standard: { query } },
        },
      },
    },
  };
}

/** The body of `POST _query_rules/<ruleset>/_test` for the same criteria (which rules match). */
export function ruleTestMatchBody(t: RuleTest) {
  return { match_criteria: matchCriteria(t.criteria) };
}

/**
 * Which documents the matched rules pin, in pin order, and which they hide; a pin a matched
 * `exclude` rule hides isn't pinned (Elasticsearch's order: pins of every matching rule, then
 * exclusions removed).
 */
export function matchedActions(rules: QueryRule[], matched: string[]): { pinned: { index?: string; id: string }[]; hidden: { index?: string; id: string }[] } {
  const pinned: { index?: string; id: string }[] = [];
  const hidden: { index?: string; id: string }[] = [];
  const key = (d: { index?: string; id: string }) => `${d.index ?? ""}\u0000${d.id}`;
  for (const r of rules) {
    if (!matched.includes(r.rule_id)) continue;
    const targets = r.actions.ids ? r.actions.ids.map((id) => ({ id: String(id) })) : (r.actions.docs ?? []).map((d) => ({ index: d._index, id: d._id }));
    const into = r.type === "exclude" ? hidden : pinned;
    for (const t of targets) if (!into.some((x) => key(x) === key(t))) into.push(t);
  }
  const hiddenKeys = new Set(hidden.map(key));
  return { pinned: pinned.filter((p) => !hiddenKeys.has(key(p))), hidden };
}

/** Whether a hit is one of the pins (an id pin matches in any index). */
export function isPinned(hit: { _index: string; _id: string }, pinned: { index?: string; id: string }[]): boolean {
  return pinned.some((p) => p.id === hit._id && (p.index === undefined || p.index === hit._index));
}
