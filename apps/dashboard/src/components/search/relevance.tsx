import { ArrowRight, Check, ChevronRight, Plus, RefreshCw, SquareTerminal, Trash2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { esJson, type SearchCluster } from "@/lib/search-api";
import {
  draftToRule,
  linesToRules,
  reloadSummary,
  ruleProblem,
  ruleShape,
  rulesToLines,
  synonymSetBody,
  synonymSettingsSnippet,
  type ReloadSummary,
  type RuleDraft,
  type RulesetSummary,
  type SynonymRule,
  type SynonymSetSummary,
  type SynonymsUpdate,
} from "@/lib/search-relevance";
import { rulePrefill, synonymPrefill } from "@/lib/search-analytics";
import { Segmented } from "../redis/shared";
import { btn, Confirm, CopyButton, ErrorNote, input, Panel, primary, useCopy } from "../ui";
import { SearchAnalyticsPanel } from "./analytics";
import { AutocompletePanel } from "./autocomplete";
import { QueryRules, RuleEditor, RulesetView } from "./query-rules";
import { BackTo, Empty, errText, LoadError, useLoad } from "./relevance-shared";

/**
 * What the tab shows: the lists (analytics, synonym sets, rulesets), one synonym set or ruleset
 * opened, or a synonym or query rule started from a query analytics listed.
 */
type View =
  | { kind: "lists" }
  | { kind: "set"; id: string }
  | { kind: "ruleset"; id: string }
  | { kind: "synonym-for"; query: string }
  | { kind: "rule-for"; query: string };

const enc = encodeURIComponent;
/** Elasticsearch caps a page of sets or rules at 10,000 (`from` + `size`). */
const PAGE = 10_000;

/**
 * The Relevance tab: synonym sets (`_synonyms`), which change what a search matches without
 * reindexing, and query rules (`_query_rules`), which pin or hide documents when a search's
 * criteria match. Everything goes through the console route as the cluster's own APIs.
 */
export function RelevanceTab({ cluster, onOpenConsole }: { cluster: SearchCluster; onOpenConsole: (text: string) => void }) {
  const [view, setView] = useState<View>({ kind: "lists" });
  const back = () => setView({ kind: "lists" });

  if (cluster.flavor === "opensearch") {
    return (
      <Panel title="Relevance">
        <p className="px-5 py-4 text-[13px] text-muted">
          Synonym sets and query rules are Elasticsearch APIs. This cluster speaks OpenSearch&apos;s, which keeps synonyms in
          each index&apos;s analysis settings instead.
        </p>
      </Panel>
    );
  }
  if (view.kind === "set") return <SynonymSetView key={view.id} cluster={cluster} id={view.id} onBack={back} onOpenConsole={onOpenConsole} />;
  if (view.kind === "ruleset") return <RulesetView key={view.id} cluster={cluster} id={view.id} onBack={back} />;
  if (view.kind === "synonym-for") return <SynonymFor cluster={cluster} query={view.query} onBack={back} onSaved={(id) => setView({ kind: "set", id })} />;
  if (view.kind === "rule-for") return <RuleFor cluster={cluster} query={view.query} onBack={back} onSaved={(id) => setView({ kind: "ruleset", id })} />;
  return (
    <div className="space-y-6">
      <SearchAnalyticsPanel cluster={cluster} onAct={(p) => setView(p.kind === "synonym" ? { kind: "synonym-for", query: p.query } : { kind: "rule-for", query: p.query })} />
      <AutocompletePanel cluster={cluster} />
      <SynonymSets cluster={cluster} onOpen={(id) => setView({ kind: "set", id })} />
      <QueryRules cluster={cluster} onOpen={(id) => setView({ kind: "ruleset", id })} />
    </div>
  );
}

// --- From a query analytics listed -------------------------------------------------------------

const NEW = "\u0000new";

/** Where a synonym or rule started from a query goes: an existing set, or a new one by name. */
function Target({ label, names, value, onChange, newName, onNewName, placeholder }: { label: string; names: string[]; value: string; onChange: (v: string) => void; newName: string; onNewName: (v: string) => void; placeholder: string }) {
  return (
    <div className="flex flex-wrap items-end gap-2">
      <label className="grid gap-1 text-[12px] text-subtle">
        {label}
        <select className={`${input} min-w-40 py-1.5 font-mono text-[12px]`} value={value} onChange={(e) => onChange(e.target.value)}>
          {names.map((n) => (
            <option key={n} value={n}>
              {n}
            </option>
          ))}
          <option value={NEW}>New…</option>
        </select>
      </label>
      {value === NEW && (
        <label className="grid gap-1 text-[12px] text-subtle">
          Name
          <input autoFocus className={`${input} font-mono text-[12px]`} value={newName} onChange={(e) => onNewName(e.target.value)} placeholder={placeholder} spellCheck={false} />
        </label>
      )}
    </div>
  );
}

function SynonymFor({ cluster, query, onBack, onSaved }: { cluster: SearchCluster; query: string; onBack: () => void; onSaved: (set: string) => void }) {
  const load = useCallback(
    () => esJson<{ results: SynonymSetSummary[] }>(cluster.id, "GET", `/_synonyms?size=${PAGE}`).then((r) => r.results.map((s) => s.synonyms_set)),
    [cluster.id],
  );
  const { value: sets, error } = useLoad(load);
  const [target, setTarget] = useState<string | null>(null);
  const [name, setName] = useState("search-analytics");
  const [text, setText] = useState(() => synonymPrefill(query));
  const [busy, setBusy] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const chosen = target ?? (sets && sets.length ? sets[0] : NEW);
  const problem = ruleProblem(text);
  // As prefilled, the rule waits for the term the query should also find: a hint, not an error.
  const untouched = text === synonymPrefill(query);
  const save = async () => {
    setBusy(true);
    setSaveError(null);
    try {
      if (chosen === NEW) {
        await esJson(cluster.id, "PUT", `/_synonyms/${enc(name.trim())}`, synonymSetBody(linesToRules(text, [])));
        onSaved(name.trim());
      } else {
        await esJson(cluster.id, "PUT", `/_synonyms/${enc(chosen)}/${enc(newRuleId())}`, { synonyms: text.trim() });
        onSaved(chosen);
      }
    } catch (e) {
      setSaveError(errText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="space-y-6">
      <BackTo onBack={onBack} />
      <Panel
        title={
          <span>
            Add a synonym for <span className="font-mono">{query}</span>
          </span>
        }
      >
        {error ? (
          <p className="px-5 py-4 text-[13px] text-danger">{error}</p>
        ) : !sets ? (
          <p className="px-5 py-4 text-[13px] text-subtle">
            <span className="shimmer">Loading synonym sets…</span>
          </p>
        ) : (
          <form
            className="space-y-4 px-5 py-4"
            onSubmit={(e) => {
              e.preventDefault();
              if (!problem) save();
            }}
          >
            <Target label="Synonym set" names={sets} value={chosen} onChange={setTarget} newName={name} onNewName={setName} placeholder="products" />
            <label className="grid gap-1 text-[12px] text-subtle">
              Rule
              <input autoFocus={chosen !== NEW} className={`${input} font-mono text-[12px]`} value={text} onChange={(e) => setText(e.target.value)} spellCheck={false} aria-label="Synonym rule" />
            </label>
            {untouched ? (
              <p className="text-[12px] text-muted">
                Add the term <span className="font-mono text-fg">{query}</span> should also find after the comma, or rewrite it with{" "}
                <span className="font-mono text-fg">=&gt;</span>.
              </p>
            ) : problem ? (
              <p className="text-[12px] text-danger">{problem}</p>
            ) : (
              text.trim() && <RuleText text={text} />
            )}
            <RuleSyntax />
            {saveError && <p className="text-[13px] text-danger">{saveError}</p>}
            <div className="flex gap-2">
              <button type="submit" className={primary} disabled={busy || !!problem || (chosen === NEW && !name.trim())}>
                {busy ? <span className="shimmer">Saving…</span> : "Add synonym"}
              </button>
              <button type="button" onClick={onBack} className={btn}>
                Cancel
              </button>
            </div>
          </form>
        )}
      </Panel>
    </div>
  );
}

function RuleFor({ cluster, query, onBack, onSaved }: { cluster: SearchCluster; query: string; onBack: () => void; onSaved: (ruleset: string) => void }) {
  const load = useCallback(
    () => esJson<{ results: RulesetSummary[] }>(cluster.id, "GET", "/_query_rules?size=1000").then((r) => r.results.map((s) => s.ruleset_id)),
    [cluster.id],
  );
  const { value: sets, error } = useLoad(load);
  const [target, setTarget] = useState<string | null>(null);
  const [name, setName] = useState("search-analytics");
  const [draft, setDraft] = useState<RuleDraft>(() => rulePrefill(query));
  const [busy, setBusy] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const chosen = target ?? (sets && sets.length ? sets[0] : NEW);
  const save = async () => {
    setBusy(true);
    setSaveError(null);
    try {
      const set = chosen === NEW ? name.trim() : chosen;
      const { rule_id, ...body } = draftToRule(draft);
      // Putting a rule into a ruleset that doesn't exist creates it.
      await esJson(cluster.id, "PUT", `/_query_rules/${enc(set)}/_rule/${enc(rule_id)}`, body);
      onSaved(set);
    } catch (e) {
      setSaveError(errText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="space-y-6">
      <BackTo onBack={onBack} />
      <Panel
        title={
          <span>
            Add a query rule for <span className="font-mono">{query}</span>
          </span>
        }
      >
        {error ? (
          <p className="px-5 py-4 text-[13px] text-danger">{error}</p>
        ) : !sets ? (
          <p className="px-5 py-4 text-[13px] text-subtle">
            <span className="shimmer">Loading rulesets…</span>
          </p>
        ) : (
          <div className="space-y-4 px-5 py-4">
            <Target label="Ruleset" names={sets} value={chosen} onChange={setTarget} newName={name} onNewName={setName} placeholder="promotions" />
            <p className="text-[12px] text-subtle">
              The rule matches when a search sends <span className="font-mono">query_string</span> equal to the query in its{" "}
              <span className="font-mono">rule</span> retriever&apos;s <span className="font-mono">match_criteria</span>. Name the documents to pin or hide.
            </p>
            <RuleEditor draft={draft} onChange={setDraft} idEditable />
            {saveError && <p className="text-[13px] text-danger">{saveError}</p>}
            <div className="flex gap-2">
              <button className={primary} onClick={save} disabled={busy || !draft.rule_id.trim() || (chosen === NEW && !name.trim())}>
                {busy ? <span className="shimmer">Saving…</span> : "Add rule"}
              </button>
              <button onClick={onBack} className={btn}>
                Cancel
              </button>
            </div>
          </div>
        )}
      </Panel>
    </div>
  );
}

// --- Synonym sets -----------------------------------------------------------------------------

function SynonymSets({ cluster, onOpen }: { cluster: SearchCluster; onOpen: (id: string) => void }) {
  const load = useCallback(
    () => esJson<{ count: number; results: SynonymSetSummary[] }>(cluster.id, "GET", `/_synonyms?size=${PAGE}`).then((r) => r.results),
    [cluster.id],
  );
  const { value: sets, error, reload } = useLoad(load);
  const [creating, setCreating] = useState(false);

  const newButton = (
    <button onClick={() => setCreating(true)} className={btn}>
      <Plus className="size-3.5" /> New set
    </button>
  );
  return (
    <Panel title="Synonym sets" aside={sets && sets.length > 0 && !creating && newButton}>
      {creating && <NewSynonymSet cluster={cluster} onCancel={() => setCreating(false)} onCreated={onOpen} />}
      {error ? (
        <LoadError error={error} onRetry={reload} />
      ) : !sets ? (
        <p className="px-5 py-4 text-[13px] text-subtle">
          <span className="shimmer">Loading synonym sets…</span>
        </p>
      ) : sets.length === 0 ? (
        !creating && (
          <Empty title="No synonym sets yet" action={newButton}>
            A set makes a search for <span className="font-mono text-fg">laptop</span> also find{" "}
            <span className="font-mono text-fg">notebook</span>, and you change its rules without reindexing anything.
          </Empty>
        )
      ) : (
        <ul>
          {sets.map((s) => (
            <li key={s.synonyms_set} className="border-b border-line last:border-0">
              <button onClick={() => onOpen(s.synonyms_set)} className="flex w-full items-center gap-3 px-5 py-2.5 text-left transition hover:bg-panel-2">
                <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{s.synonyms_set}</span>
                <span className="font-mono text-[12px] tabular-nums text-muted">{s.count.toLocaleString("en-US")}</span>
                <span className="w-10 text-[12px] text-subtle">{s.count === 1 ? "rule" : "rules"}</span>
                <ChevronRight className="size-3.5 text-subtle" />
              </button>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

function NewSynonymSet({ cluster, onCancel, onCreated }: { cluster: SearchCluster; onCancel: () => void; onCreated: (id: string) => void }) {
  const [name, setName] = useState("");
  const [text, setText] = useState("laptop, notebook\ntv => television");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      await esJson(cluster.id, "PUT", `/_synonyms/${enc(name.trim())}`, synonymSetBody(linesToRules(text, [])));
      onCreated(name.trim());
    } catch (e) {
      setError(errText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form
      className="rise space-y-3 border-b border-line px-5 py-4"
      onSubmit={(e) => {
        e.preventDefault();
        create();
      }}
    >
      <label className="grid max-w-sm gap-1 text-[12px] text-subtle">
        Name
        <input autoFocus className={`${input} font-mono text-[12px]`} value={name} onChange={(e) => setName(e.target.value)} placeholder="products" spellCheck={false} />
      </label>
      <label className="grid gap-1 text-[12px] text-subtle">
        Rules, one per line
        <textarea className={`${input} min-h-24 font-mono text-[12px]`} value={text} onChange={(e) => setText(e.target.value)} spellCheck={false} />
      </label>
      <RuleSyntax />
      {error && <p className="text-[13px] text-danger">{error}</p>}
      <div className="flex gap-2">
        <button type="submit" className={primary} disabled={busy || !name.trim()}>
          {busy ? <span className="shimmer">Creating…</span> : "Create set"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function RuleSyntax() {
  return (
    <p className="text-[12px] text-subtle">
      <span className="font-mono text-muted">laptop, notebook</span> makes the terms mean the same;{" "}
      <span className="font-mono text-muted">tv =&gt; television</span> rewrites the left side to the right.
    </p>
  );
}

/** A rule's text with its shape shown: commas as separators, `=>` as an arrow. */
function RuleText({ text }: { text: string }) {
  const s = ruleShape(text);
  const join = (ts: string[]) =>
    ts.map((t, i) => (
      <span key={i}>
        {i > 0 && <span className="text-subtle">, </span>}
        {t}
      </span>
    ));
  if (s.kind === "equivalent") return <span className="font-mono text-[12px]">{join(s.terms)}</span>;
  return (
    <span className="inline-flex flex-wrap items-center gap-x-1.5 font-mono text-[12px]">
      <span>{join(s.from)}</span>
      <ArrowRight className="size-3.5 text-subtle" aria-label="becomes" />
      <span>{join(s.to)}</span>
    </span>
  );
}

/** One row while editing: the stored rule (if any) and its text now. */
interface Row {
  key: string;
  id: string;
  text: string;
  saved: string | null;
}

const ruleCount = (n: number) => (n === 1 ? "1 rule" : `${n.toLocaleString("en-US")} rules`);

let rowSeq = 0;
const newRuleId = () => `rule-${Date.now().toString(36)}${(rowSeq++).toString(36)}`;

function SynonymSetView({ cluster, id, onBack, onOpenConsole }: { cluster: SearchCluster; id: string; onBack: () => void; onOpenConsole: (text: string) => void }) {
  const load = useCallback(
    () => esJson<{ count: number; synonyms_set: SynonymRule[] }>(cluster.id, "GET", `/_synonyms/${enc(id)}?size=${PAGE}`),
    [cluster.id, id],
  );
  const { value, error, reload } = useLoad(load);
  const [rows, setRows] = useState<Row[]>([]);
  const [mode, setMode] = useState<"rows" | "text">("rows");
  const [text, setText] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [reloaded, setReloaded] = useState<ReloadSummary | null>(null);
  const [deleting, setDeleting] = useState(false);

  useEffect(() => {
    if (!value) return;
    setRows(value.synonyms_set.map((r) => ({ key: r.id, id: r.id, text: r.synonyms, saved: r.synonyms })));
    setText(rulesToLines(value.synonyms_set));
  }, [value]);

  const complete = value ? value.count <= value.synonyms_set.length : false;

  const act = async (key: string, f: () => Promise<SynonymsUpdate | void>) => {
    setBusy(key);
    setActionError(null);
    try {
      const u = await f();
      if (u) setReloaded(reloadSummary(id, u));
      return true;
    } catch (e) {
      setActionError(errText(e));
      return false;
    } finally {
      setBusy(null);
    }
  };

  const saveRow = (row: Row) =>
    act(row.key, async () => {
      const u = await esJson<SynonymsUpdate>(cluster.id, "PUT", `/_synonyms/${enc(id)}/${enc(row.id.trim())}`, { synonyms: row.text.trim() });
      setRows((rs) => rs.map((r) => (r.key === row.key ? { ...r, id: row.id.trim(), text: row.text.trim(), saved: row.text.trim() } : r)));
      return u;
    });

  const deleteRow = (row: Row) =>
    row.saved === null
      ? setRows((rs) => rs.filter((r) => r.key !== row.key))
      : act(row.key, async () => {
          const u = await esJson<SynonymsUpdate>(cluster.id, "DELETE", `/_synonyms/${enc(id)}/${enc(row.id)}`);
          setRows((rs) => rs.filter((r) => r.key !== row.key));
          return u;
        });

  const saveText = () =>
    act("text", async () => {
      const u = await esJson<SynonymsUpdate>(cluster.id, "PUT", `/_synonyms/${enc(id)}`, synonymSetBody(linesToRules(text, value?.synonyms_set ?? [])));
      reload();
      return u;
    });

  const deleteSet = () =>
    act("delete", async () => {
      await esJson(cluster.id, "DELETE", `/_synonyms/${enc(id)}`);
      onBack();
    });

  const snippet = synonymSettingsSnippet(id);
  return (
    <div className="space-y-6">
      <BackTo onBack={onBack} />
      <Panel
        title={
          <span className="flex items-baseline gap-2">
            <span className="font-mono">{id}</span>
            {value && <span className="text-[12px] font-normal text-subtle">{ruleCount(complete ? rows.filter((r) => r.saved !== null).length : value.count)}</span>}
          </span>
        }
        aside={
          value && (
            <div className="flex items-center gap-2">
              <Segmented
                value={mode}
                options={[
                  { label: "rows", value: "rows" },
                  { label: "text", value: "text" },
                ]}
                onChange={(m) => {
                  setMode(m);
                  if (m === "text") setText(rulesToLines(rows.filter((r) => r.saved !== null).map((r) => ({ id: r.id, synonyms: r.saved ?? "" }))));
                }}
              />
              <button onClick={() => setDeleting(true)} className={btn} title="Delete set">
                <Trash2 className="size-3.5" />
              </button>
            </div>
          )
        }
      >
        {error ? (
          <LoadError error={error} onRetry={reload} />
        ) : !value ? (
          <p className="px-5 py-4 text-[13px] text-subtle">
            <span className="shimmer">Loading rules…</span>
          </p>
        ) : (
          <>
            {deleting && (
              <div className="border-b border-line px-5 py-4">
                <Confirm word={id} action="Delete set" busy={busy === "delete"} onConfirm={deleteSet} onCancel={() => setDeleting(false)}>
                  Deletes the set and its rules. Elasticsearch refuses while an index&apos;s analyzer uses it.
                </Confirm>
              </div>
            )}
            {reloaded && <ReloadNote summary={reloaded} />}
            {actionError && (
              <div className="px-5">
                <ErrorNote error={actionError} />
              </div>
            )}
            {mode === "rows" ? (
              <SynonymRows rows={rows} setRows={setRows} busy={busy} onSave={saveRow} onDelete={deleteRow} />
            ) : (
              <div className="space-y-3 px-5 py-4">
                <textarea
                  className={`${input} block min-h-48 w-full font-mono text-[12px] leading-relaxed`}
                  value={text}
                  onChange={(e) => setText(e.target.value)}
                  spellCheck={false}
                  aria-label="Rules, one per line"
                />
                <p className="text-[12px] text-subtle">
                  One rule per line; <span className="font-mono">#</span> starts a comment. A line that&apos;s unchanged keeps its
                  rule id. Saving replaces the whole set in one update.
                </p>
                {!complete && (
                  <p className="text-[12px] text-danger">
                    This set has more rules than one page holds ({value.count.toLocaleString("en-US")}): edit it rule by rule.
                  </p>
                )}
                <button className={primary} onClick={saveText} disabled={busy !== null || !complete}>
                  {busy === "text" ? <span className="shimmer">Saving…</span> : "Save set"}
                </button>
              </div>
            )}
          </>
        )}
      </Panel>
      <UseInIndex id={id} snippet={snippet} onOpenConsole={onOpenConsole} />
    </div>
  );
}

function SynonymRows({
  rows,
  setRows,
  busy,
  onSave,
  onDelete,
}: {
  rows: Row[];
  setRows: (f: (rs: Row[]) => Row[]) => void;
  busy: string | null;
  onSave: (r: Row) => void;
  onDelete: (r: Row) => void;
}) {
  const edit = (key: string, patch: Partial<Row>) => setRows((rs) => rs.map((r) => (r.key === key ? { ...r, ...patch } : r)));
  return (
    <div>
      {rows.length === 0 && <p className="px-5 py-4 text-[13px] text-muted">No rules in this set. Add one: an empty set changes nothing.</p>}
      <ul>
        {rows.map((r) => {
          const dirty = r.saved !== r.text;
          const problem = dirty ? ruleProblem(r.text) : null;
          return (
            <li key={r.key} className="grid grid-cols-[minmax(0,1fr)_auto] items-start gap-x-3 gap-y-1 border-b border-line px-5 py-2.5 sm:grid-cols-[minmax(0,9rem)_minmax(0,1fr)_auto] sm:gap-y-3">
              {r.saved === null ? (
                <input
                  className={`${input} col-span-2 w-full py-1 font-mono text-[11px] sm:col-span-1`}
                  value={r.id}
                  onChange={(e) => edit(r.key, { id: e.target.value })}
                  aria-label="Rule id"
                  spellCheck={false}
                />
              ) : (
                <span className="col-span-2 truncate font-mono text-[11px] text-subtle sm:col-span-1 sm:pt-1.5" title={r.id}>
                  {r.id}
                </span>
              )}
              <div className="min-w-0 space-y-1">
                <input
                  className={`${input} w-full py-1 font-mono text-[12px]`}
                  value={r.text}
                  onChange={(e) => edit(r.key, { text: e.target.value })}
                  onKeyDown={(e) => e.key === "Enter" && dirty && !problem && onSave(r)}
                  placeholder="laptop, notebook"
                  aria-label={`Rule ${r.id}`}
                  spellCheck={false}
                />
                {problem ? <p className="text-[11px] text-danger">{problem}</p> : r.text.trim() && <RuleText text={r.text} />}
              </div>
              <div className="flex items-center gap-1 pt-0.5">
                {dirty && (
                  <button onClick={() => onSave(r)} disabled={busy !== null || !!problem || !r.id.trim()} className={btn} title="Save rule">
                    {busy === r.key ? <span className="shimmer">Saving</span> : <Check className="size-3.5" />}
                  </button>
                )}
                <button onClick={() => onDelete(r)} disabled={busy !== null} className="rounded-md p-1.5 text-subtle transition hover:text-danger disabled:opacity-40" title="Delete rule">
                  <Trash2 className="size-3.5" />
                </button>
              </div>
            </li>
          );
        })}
      </ul>
      <div className="flex flex-wrap items-center gap-3 px-5 py-3">
        <button onClick={() => setRows((rs) => [...rs, { key: `new-${rowSeq}`, id: newRuleId(), text: "", saved: null }])} className={`${btn} whitespace-nowrap`}>
          <Plus className="size-3.5" /> Add rule
        </button>
        <RuleSyntax />
      </div>
    </div>
  );
}

/** What a save reloaded, in plain words. */
function ReloadNote({ summary }: { summary: ReloadSummary }) {
  return (
    <div className="rise flex items-start gap-2 border-b border-line px-5 py-3 text-[13px]">
      <RefreshCw className={`mt-0.5 size-3.5 shrink-0 ${summary.indices.length ? "text-accent" : "text-subtle"}`} />
      <div className="space-y-1.5">
        <p className={summary.failedShards ? "text-danger" : "text-muted"}>{summary.text}</p>
        {summary.indices.length > 0 && (
          <ul className="flex flex-wrap gap-1.5">
            {summary.indices.map((i) => (
              <li key={i.index} className="rounded-md border border-line px-2 py-0.5 font-mono text-[11px]">
                {i.index} <span className="text-subtle">· {i.analyzers.join(", ")}</span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

function UseInIndex({ id, snippet, onOpenConsole }: { id: string; snippet: string; onOpenConsole: (text: string) => void }) {
  const { copied, copy } = useCopy();
  return (
    <Panel title="Use in an index">
      <div className="grid gap-5 px-5 py-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.1fr)]">
        <ol className="list-decimal space-y-2 pl-4 text-[13px] text-muted marker:text-subtle">
          <li>
            Add a <span className="font-mono text-fg">synonym_graph</span> filter that reads the set
            (<span className="font-mono text-fg">synonyms_set</span>) with <span className="font-mono text-fg">updateable: true</span>, so
            saving the set reloads it.
          </li>
          <li>
            Put it in an analyzer that fields use as their <span className="font-mono text-fg">search_analyzer</span>. Updateable filters
            run only at search time: documents are indexed without synonyms, so changing the rules never needs a reindex.
          </li>
          <li>
            A new index takes the settings when it&apos;s created. An existing index can only gain an analyzer while it&apos;s closed:
            close it, <span className="font-mono text-fg">PUT /&lt;index&gt;/_settings</span> with the analysis part, open it, then set
            the field&apos;s <span className="font-mono text-fg">search_analyzer</span> with{" "}
            <span className="font-mono text-fg">PUT /&lt;index&gt;/_mapping</span>.
          </li>
        </ol>
        <div className="min-w-0 space-y-2">
          <div className="relative rounded-lg border border-line bg-bg">
            <pre className="max-h-72 overflow-auto px-3 py-2.5 font-mono text-[11px] leading-relaxed">{snippet}</pre>
            <div className="absolute right-1.5 top-1.5">
              <CopyButton id="snippet" text={snippet} copied={copied} onCopy={copy} />
            </div>
          </div>
          <button onClick={() => onOpenConsole(`PUT /my-index\n${snippet}`)} className={btn}>
            <SquareTerminal className="size-3.5" /> Create an index with it in the console
          </button>
          <p className="text-[12px] text-subtle">
            The filter is named <span className="font-mono">{id}_synonyms</span> and the analyzer <span className="font-mono">{id}_search</span>;
            rename them freely.
          </p>
        </div>
      </div>
    </Panel>
  );
}
