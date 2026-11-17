import { ArrowDown, ArrowUp, ChevronRight, EyeOff, Pencil, Pin, Play, Plus, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { esJson, searchApi, type SearchCluster } from "@/lib/search-api";
import {
  CRITERIA_TYPES,
  CRITERIA_WORDS,
  criteriaKeys,
  draftToRule,
  emptyDraft,
  isPinned,
  matchedActions,
  moveRule,
  ruleSentence,
  ruleTestMatchBody,
  ruleTestRequest,
  ruleToDraft,
  type CriteriaType,
  type Part,
  type QueryRule,
  type RuleDraft,
  type Ruleset,
  type RulesetSummary,
} from "@/lib/search-relevance";
import { Segmented } from "../redis/shared";
import { btn, Confirm, ErrorNote, input, Panel, primary } from "../ui";
import { BackTo, Empty, errText, LoadError, useLoad } from "./relevance-shared";

const enc = encodeURIComponent;

/** The cluster's rulesets. */
export function QueryRules({ cluster, onOpen }: { cluster: SearchCluster; onOpen: (id: string) => void }) {
  const load = useCallback(
    () => esJson<{ count: number; results: RulesetSummary[] }>(cluster.id, "GET", "/_query_rules?size=1000").then((r) => r.results),
    [cluster.id],
  );
  const { value: sets, error, reload } = useLoad(load);
  const [creating, setCreating] = useState(false);
  const newButton = (
    <button onClick={() => setCreating(true)} className={btn}>
      <Plus className="size-3.5" /> New ruleset
    </button>
  );
  return (
    <Panel title="Query rules" aside={sets && sets.length > 0 && !creating && newButton}>
      {creating && <NewRuleset cluster={cluster} onCancel={() => setCreating(false)} onCreated={onOpen} />}
      {error ? (
        <LoadError error={error} onRetry={reload} />
      ) : !sets ? (
        <p className="px-5 py-4 text-[13px] text-subtle">
          <span className="shimmer">Loading rulesets…</span>
        </p>
      ) : sets.length === 0 ? (
        !creating && (
          <Empty title="No query rules yet" action={newButton}>
            Rules put chosen documents first, or hide them, when a search matches: a promotion for{" "}
            <span className="font-mono text-fg">iphone</span>, the right answer for a query people keep typing.
          </Empty>
        )
      ) : (
        <ul>
          {sets.map((s) => (
            <li key={s.ruleset_id} className="border-b border-line last:border-0">
              <button onClick={() => onOpen(s.ruleset_id)} className="flex w-full items-center gap-3 px-5 py-2.5 text-left transition hover:bg-panel-2">
                <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{s.ruleset_id}</span>
                <span className="text-[12px] text-subtle">
                  {Object.entries(s.rule_type_counts ?? {})
                    .map(([t, n]) => `${n} ${t === "exclude" ? "hide" : t}`)
                    .join(" · ")}
                </span>
                <span className="font-mono text-[12px] tabular-nums text-muted">{s.rule_total_count}</span>
                <span className="w-10 text-[12px] text-subtle">{s.rule_total_count === 1 ? "rule" : "rules"}</span>
                <ChevronRight className="size-3.5 text-subtle" />
              </button>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

function NewRuleset({ cluster, onCancel, onCreated }: { cluster: SearchCluster; onCancel: () => void; onCreated: (id: string) => void }) {
  const [name, setName] = useState("");
  const [draft, setDraft] = useState<RuleDraft>(() => emptyDraft("rule-1"));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      await esJson(cluster.id, "PUT", `/_query_rules/${enc(name.trim())}`, { rules: [draftToRule(draft)] });
      onCreated(name.trim());
    } catch (e) {
      setError(errText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="rise space-y-4 border-b border-line px-5 py-4">
      <label className="grid max-w-sm gap-1 text-[12px] text-subtle">
        Ruleset
        <input autoFocus className={`${input} font-mono text-[12px]`} value={name} onChange={(e) => setName(e.target.value)} placeholder="promotions" spellCheck={false} />
      </label>
      <p className="text-[12px] text-subtle">A ruleset starts with its first rule.</p>
      <RuleEditor draft={draft} onChange={setDraft} idEditable />
      {error && <p className="text-[13px] text-danger">{error}</p>}
      <div className="flex gap-2">
        <button className={primary} onClick={create} disabled={busy || !name.trim() || !draft.rule_id.trim()}>
          {busy ? <span className="shimmer">Creating…</span> : "Create ruleset"}
        </button>
        <button onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </div>
  );
}

/** A rule's sentence, its metadata, values and ids in Geist Mono. */
function Sentence({ parts }: { parts: Part[] }) {
  return (
    <span className="text-[13px]">
      {parts.map((p, i) =>
        p.code ? (
          <span key={i} className="rounded bg-panel-2 px-1 py-px font-mono text-[12px]">
            {p.text}
          </span>
        ) : (
          <span key={i} className="text-muted">
            {p.text}
          </span>
        ),
      )}
    </span>
  );
}

function sentenceOf(d: RuleDraft): Part[] | null {
  try {
    return ruleSentence(draftToRule(d));
  } catch {
    return null;
  }
}

/** One ruleset: its rules in the order they apply, editing, reordering and a test. */
export function RulesetView({ cluster, id, onBack }: { cluster: SearchCluster; id: string; onBack: () => void }) {
  const load = useCallback(() => esJson<Ruleset>(cluster.id, "GET", `/_query_rules/${enc(id)}`), [cluster.id, id]);
  const { value, error, reload, setValue } = useLoad(load);
  // The rule being edited: its id, or "" for a new one.
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState<RuleDraft>(() => emptyDraft());
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState(false);

  const act = async (key: string, f: () => Promise<void>) => {
    setBusy(key);
    setActionError(null);
    try {
      await f();
    } catch (e) {
      setActionError(errText(e));
    } finally {
      setBusy(null);
    }
  };
  const fresh = async () => setValue(await load());

  const rules = value?.rules ?? [];
  const startEdit = (r: QueryRule | null) => {
    setActionError(null);
    setEditing(r ? r.rule_id : "");
    setDraft(r ? ruleToDraft(r) : emptyDraft(`rule-${rules.length + 1}`));
  };
  const saveRule = () =>
    act("save", async () => {
      const rule = draftToRule(draft);
      const { rule_id, ...body } = rule;
      await esJson(cluster.id, "PUT", `/_query_rules/${enc(id)}/_rule/${enc(rule_id)}`, body);
      // A renamed rule is a new rule: drop the old one.
      if (editing && editing !== rule_id) await esJson(cluster.id, "DELETE", `/_query_rules/${enc(id)}/_rule/${enc(editing)}`);
      setEditing(null);
      await fresh();
    });
  const move = (from: number, to: number) =>
    act(`move-${from}`, async () => {
      await esJson(cluster.id, "PUT", `/_query_rules/${enc(id)}`, { rules: moveRule(rules, from, to) });
      await fresh();
    });
  const deleteRule = (r: QueryRule) =>
    act(`del-${r.rule_id}`, async () => {
      await esJson(cluster.id, "DELETE", `/_query_rules/${enc(id)}/_rule/${enc(r.rule_id)}`);
      // Deleting a ruleset's last rule deletes the ruleset.
      if (rules.length === 1) onBack();
      else await fresh();
    });
  const deleteSet = () =>
    act("delete", async () => {
      await esJson(cluster.id, "DELETE", `/_query_rules/${enc(id)}`);
      onBack();
    });

  const editor = (
    <div className="rise space-y-4 border-b border-line bg-bg/40 px-5 py-4">
      <RuleEditor draft={draft} onChange={setDraft} idEditable />
      <div className="flex gap-2">
        <button className={primary} onClick={saveRule} disabled={busy !== null || !draft.rule_id.trim()}>
          {busy === "save" ? <span className="shimmer">Saving…</span> : "Save rule"}
        </button>
        <button onClick={() => setEditing(null)} className={btn}>
          Cancel
        </button>
      </div>
    </div>
  );

  return (
    <div className="space-y-6">
      <BackTo onBack={onBack} />
      <Panel
        title={
          <span className="flex items-baseline gap-2">
            <span className="font-mono">{id}</span>
            {value && <span className="text-[12px] font-normal text-subtle">{rules.length === 1 ? "1 rule" : `${rules.length} rules`} in the order they apply</span>}
          </span>
        }
        aside={
          value && (
            <div className="flex items-center gap-2">
              <button onClick={() => startEdit(null)} disabled={editing !== null} className={`${btn} whitespace-nowrap`}>
                <Plus className="size-3.5" /> Add rule
              </button>
              <button onClick={() => setDeleting(true)} className={btn} title="Delete ruleset">
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
                <Confirm word={id} action="Delete ruleset" busy={busy === "delete"} onConfirm={deleteSet} onCancel={() => setDeleting(false)}>
                  Deletes the ruleset and its rules. Searches that name it in a <span className="font-mono">rule</span> retriever then fail.
                </Confirm>
              </div>
            )}
            {actionError && (
              <div className="px-5 pb-1">
                <ErrorNote error={actionError} />
              </div>
            )}
            {editing === "" && editor}
            <ol>
              {rules.map((r, i) =>
                editing === r.rule_id ? (
                  <li key={r.rule_id}>{editor}</li>
                ) : (
                  <li key={r.rule_id} className="group flex items-start gap-3 border-b border-line px-5 py-3 last:border-0">
                    <span className="w-5 pt-0.5 text-right font-mono text-[12px] tabular-nums text-subtle">{i + 1}</span>
                    {r.type === "exclude" ? (
                      <EyeOff className="mt-0.5 size-3.5 shrink-0 text-subtle" aria-label="hide" />
                    ) : (
                      <Pin className="mt-0.5 size-3.5 shrink-0 text-muted" aria-label="pin" />
                    )}
                    <div className="min-w-0 flex-1 space-y-1">
                      <Sentence parts={ruleSentence(r)} />
                      <p className="font-mono text-[11px] text-subtle">
                        {r.rule_id}
                        {r.priority != null && <> · priority {r.priority}</>}
                      </p>
                    </div>
                    <div className="flex items-center gap-0.5">
                      <IconButton title="Move up" disabled={busy !== null || i === 0} onClick={() => move(i, i - 1)}>
                        <ArrowUp className="size-3.5" />
                      </IconButton>
                      <IconButton title="Move down" disabled={busy !== null || i === rules.length - 1} onClick={() => move(i, i + 1)}>
                        <ArrowDown className="size-3.5" />
                      </IconButton>
                      <IconButton title="Edit" disabled={busy !== null || editing !== null} onClick={() => startEdit(r)}>
                        <Pencil className="size-3.5" />
                      </IconButton>
                      <IconButton title={rules.length === 1 ? "Delete (the last rule: deletes the ruleset)" : "Delete"} disabled={busy !== null} onClick={() => deleteRule(r)} danger>
                        <Trash2 className="size-3.5" />
                      </IconButton>
                    </div>
                  </li>
                ),
              )}
            </ol>
            <p className="border-t border-line px-5 py-2.5 text-[12px] text-subtle">
              Every matching rule applies: pins keep this order, and a hidden document is never pinned. Moving a rule sets each rule&apos;s{" "}
              <span className="font-mono">priority</span> to its place.
            </p>
          </>
        )}
      </Panel>
      {value && rules.length > 0 && <RuleTest cluster={cluster} ruleset={id} rules={rules} />}
    </div>
  );
}

function IconButton({ title, disabled, onClick, danger, children }: { title: string; disabled?: boolean; onClick: () => void; danger?: boolean; children: ReactNode }) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      title={title}
      aria-label={title}
      className={`rounded-md p-1.5 text-subtle transition disabled:opacity-30 ${danger ? "hover:text-danger" : "hover:text-fg"}`}
    >
      {children}
    </button>
  );
}

/** The rule form: criteria (type, metadata, values) and the action (pin or hide; ids or docs). */
function RuleEditor({ draft, onChange, idEditable }: { draft: RuleDraft; onChange: (d: RuleDraft) => void; idEditable?: boolean }) {
  const set = (patch: Partial<RuleDraft>) => onChange({ ...draft, ...patch });
  const setCriterion = (i: number, patch: Partial<RuleDraft["criteria"][number]>) =>
    set({ criteria: draft.criteria.map((c, j) => (j === i ? { ...c, ...patch } : c)) });
  const setDoc = (i: number, patch: Partial<RuleDraft["docs"][number]>) => set({ docs: draft.docs.map((d, j) => (j === i ? { ...d, ...patch } : d)) });
  const preview = sentenceOf(draft);
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-end gap-3">
        {idEditable && (
          <label className="grid gap-1 text-[12px] text-subtle">
            Rule id
            <input className={`${input} w-48 font-mono text-[12px]`} value={draft.rule_id} onChange={(e) => set({ rule_id: e.target.value })} spellCheck={false} />
          </label>
        )}
        <label className="grid gap-1 text-[12px] text-subtle">
          Priority
          <input
            className={`${input} w-28 font-mono text-[12px]`}
            value={draft.priority}
            onChange={(e) => set({ priority: e.target.value })}
            placeholder="none"
            inputMode="numeric"
          />
        </label>
      </div>

      <fieldset className="min-w-0 space-y-2">
        <legend className="mb-1 text-[12px] text-subtle">When every criterion matches</legend>
        {draft.criteria.map((c, i) => (
          <div key={i} className="flex flex-wrap items-center gap-2">
            <input
              className={`${input} w-40 font-mono text-[12px]`}
              value={c.type === "always" ? "" : c.metadata}
              disabled={c.type === "always"}
              onChange={(e) => setCriterion(i, { metadata: e.target.value })}
              placeholder={c.type === "always" ? "—" : "query_string"}
              aria-label="Metadata"
              spellCheck={false}
            />
            <select className={input} value={c.type} onChange={(e) => setCriterion(i, { type: e.target.value as CriteriaType })} aria-label="Criteria type">
              {CRITERIA_TYPES.map((t) => (
                <option key={t} value={t}>
                  {CRITERIA_WORDS[t]} ({t})
                </option>
              ))}
            </select>
            {c.type !== "always" && (
              <input
                className={`${input} min-w-48 flex-1 font-mono text-[12px]`}
                value={c.values}
                onChange={(e) => setCriterion(i, { values: e.target.value })}
                placeholder="iphone, iphone 15"
                aria-label="Values, comma separated"
                spellCheck={false}
              />
            )}
            {draft.criteria.length > 1 && (
              <IconButton title="Remove criterion" onClick={() => set({ criteria: draft.criteria.filter((_, j) => j !== i) })}>
                <X className="size-3.5" />
              </IconButton>
            )}
          </div>
        ))}
        <button onClick={() => set({ criteria: [...draft.criteria, { type: "exact", metadata: "", values: "" }] })} className={btn}>
          <Plus className="size-3.5" /> Criterion
        </button>
        <p className="text-[11px] text-subtle">Values are comma separated; any one of them matching is enough.</p>
      </fieldset>

      <fieldset className="min-w-0 space-y-2">
        <legend className="mb-1 text-[12px] text-subtle">Then</legend>
        <div className="flex flex-wrap items-center gap-2">
          <Segmented
            value={draft.type}
            options={[
              { label: "pin", value: "pinned" },
              { label: "hide", value: "exclude" },
            ]}
            onChange={(type) => set({ type })}
          />
          <Segmented
            value={draft.mode}
            options={[
              { label: "ids", value: "ids" },
              { label: "docs", value: "docs" },
            ]}
            onChange={(mode) => set({ mode })}
          />
        </div>
        {draft.mode === "ids" ? (
          <input
            className={`${input} w-full font-mono text-[12px]`}
            value={draft.ids}
            onChange={(e) => set({ ids: e.target.value })}
            placeholder="doc-1, doc-7"
            aria-label="Document ids, comma separated"
            spellCheck={false}
          />
        ) : (
          <div className="space-y-2">
            {draft.docs.map((d, i) => (
              <div key={i} className="flex items-center gap-2">
                <input className={`${input} w-40 font-mono text-[12px]`} value={d._index} onChange={(e) => setDoc(i, { _index: e.target.value })} placeholder="_index" aria-label="Index" spellCheck={false} />
                <input className={`${input} min-w-0 flex-1 font-mono text-[12px]`} value={d._id} onChange={(e) => setDoc(i, { _id: e.target.value })} placeholder="_id" aria-label="Document id" spellCheck={false} />
                {draft.docs.length > 1 && (
                  <IconButton title="Remove document" onClick={() => set({ docs: draft.docs.filter((_, j) => j !== i) })}>
                    <X className="size-3.5" />
                  </IconButton>
                )}
              </div>
            ))}
            <button onClick={() => set({ docs: [...draft.docs, { _index: "", _id: "" }] })} className={btn}>
              <Plus className="size-3.5" /> Document
            </button>
          </div>
        )}
        <p className="text-[11px] text-subtle">
          <span className="font-mono">ids</span> match in every index searched; <span className="font-mono">docs</span> name the index too.
        </p>
      </fieldset>
      {preview && (
        <p className="border-l border-line-2 pl-3">
          <Sentence parts={preview} />
        </p>
      )}
    </div>
  );
}

interface Hit {
  _index: string;
  _id: string;
  _score: number | null;
  _source?: Record<string, unknown>;
}

/** A short label for a hit: its first string field. */
function hitLabel(h: Hit): string {
  for (const v of Object.values(h._source ?? {})) if (typeof v === "string" && v.trim()) return v;
  return "";
}

/** Runs a `rule` retriever over a plain query with chosen match criteria and marks the pins. */
function RuleTest({ cluster, ruleset, rules }: { cluster: SearchCluster; ruleset: string; rules: QueryRule[] }) {
  const [indices, setIndices] = useState<string[] | null>(null);
  const [index, setIndex] = useState("");
  const [query, setQuery] = useState("");
  // The first criterion follows the query text until it's edited.
  const [criteria, setCriteria] = useState(() => (criteriaKeys(rules).length ? criteriaKeys(rules) : ["query_string"]).map((key) => ({ key, value: "" })));
  const [linked, setLinked] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<{ hits: Hit[]; total: number; matched: string[]; pinned: { index?: string; id: string }[]; hidden: { index?: string; id: string }[] } | null>(null);

  useEffect(() => {
    searchApi.indices(cluster.id).then(
      (rows) => {
        const names = rows.map((r) => r.index).filter((n) => !n.startsWith("."));
        setIndices(names);
        // An index the rules name (a `docs` pin's `_index`) before the first one.
        const named = rules.flatMap((r) => (r.actions.docs ?? []).map((d) => d._index)).find((n) => names.includes(n));
        setIndex((i) => i || named || names[0] || "");
      },
      () => setIndices([]),
    );
    // Only on open: the rules' later edits don't move the picked index.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cluster.id]);

  const setQueryText = (q: string) => {
    setQuery(q);
    if (linked) setCriteria((cs) => cs.map((c, i) => (i === 0 ? { ...c, value: q } : c)));
  };

  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      const t = { index, ruleset, query, criteria };
      const req = ruleTestRequest(t);
      const [search, test] = await Promise.all([
        esJson<{ hits: { total?: { value: number } | number; hits: Hit[] } }>(cluster.id, "POST", req.path, req.body),
        esJson<{ matched_rules: { rule_id: string }[] }>(cluster.id, "POST", `/_query_rules/${enc(ruleset)}/_test`, ruleTestMatchBody(t)),
      ]);
      const matched = test.matched_rules.map((m) => m.rule_id);
      const total = search.hits.total;
      setResult({ hits: search.hits.hits, total: typeof total === "number" ? total : (total?.value ?? search.hits.hits.length), matched, ...matchedActions(rules, matched) });
    } catch (e) {
      setResult(null);
      setError(errText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Panel title="Test">
      <div className="space-y-4 px-5 py-4">
        <p className="max-w-2xl text-[13px] text-muted">
          Searches an index with a <span className="font-mono text-fg">rule</span> retriever around a plain query, sending the match criteria a
          client would. Pinned documents come first.
        </p>
        {indices && indices.length === 0 ? (
          <p className="text-[13px] text-muted">No index to search yet: create one in Indices.</p>
        ) : (
          <>
            <div className="flex flex-wrap items-end gap-3">
              <label className="grid gap-1 text-[12px] text-subtle">
                Index
                <select className={`${input} font-mono text-[12px]`} value={index} onChange={(e) => setIndex(e.target.value)}>
                  {(indices ?? []).map((n) => (
                    <option key={n} value={n}>
                      {n}
                    </option>
                  ))}
                </select>
              </label>
              <label className="grid min-w-56 flex-1 gap-1 text-[12px] text-subtle">
                Query
                <input
                  className={input}
                  value={query}
                  onChange={(e) => setQueryText(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && index && run()}
                  placeholder="iphone"
                />
              </label>
            </div>
            <fieldset className="min-w-0 space-y-2">
              <legend className="mb-1 text-[12px] text-subtle">match_criteria</legend>
              {criteria.map((c, i) => (
                <div key={i} className="flex items-center gap-2">
                  <input
                    className={`${input} w-28 shrink-0 font-mono text-[12px] sm:w-40`}
                    value={c.key}
                    onChange={(e) => setCriteria((cs) => cs.map((x, j) => (j === i ? { ...x, key: e.target.value } : x)))}
                    aria-label="Key"
                    spellCheck={false}
                  />
                  <input
                    className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
                    value={c.value}
                    onChange={(e) => {
                      if (i === 0) setLinked(false);
                      setCriteria((cs) => cs.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)));
                    }}
                    placeholder={i === 0 && linked ? "same as the query" : "value"}
                    aria-label={`Value of ${c.key}`}
                    spellCheck={false}
                  />
                  {criteria.length > 1 && (
                    <IconButton title="Remove" onClick={() => setCriteria((cs) => cs.filter((_, j) => j !== i))}>
                      <X className="size-3.5" />
                    </IconButton>
                  )}
                </div>
              ))}
              <button onClick={() => setCriteria((cs) => [...cs, { key: "", value: "" }])} className={btn}>
                <Plus className="size-3.5" /> Criterion
              </button>
            </fieldset>
            <button className={primary} onClick={run} disabled={busy || !index}>
              <Play className="size-3.5" /> {busy ? <span className="shimmer">Searching…</span> : "Run test"}
            </button>
          </>
        )}
        {error && <p className="text-[13px] text-danger">{error}</p>}
        {result && <TestResult result={result} />}
      </div>
    </Panel>
  );
}

function TestResult({ result }: { result: { hits: Hit[]; total: number; matched: string[]; pinned: { index?: string; id: string }[]; hidden: { index?: string; id: string }[] } }) {
  const { hits, total, matched, pinned, hidden } = result;
  return (
    <div className="rise space-y-3">
      <p className="text-[13px] text-muted">
        {matched.length === 0 ? (
          "No rule matched these criteria: the results are the plain query's."
        ) : (
          <>
            {matched.length === 1 ? "1 rule matched: " : `${matched.length} rules matched: `}
            <span className="font-mono text-[12px] text-fg">{matched.join(", ")}</span>
            {hidden.length > 0 && (
              <>
                {" "}
                · hidden <span className="font-mono text-[12px] text-fg">{hidden.map((h) => (h.index ? `${h.index}/${h.id}` : h.id)).join(", ")}</span>
              </>
            )}
          </>
        )}
      </p>
      {hits.length === 0 ? (
        <p className="text-[13px] text-muted">No documents found.</p>
      ) : (
        <ol className="overflow-hidden rounded-lg border border-line">
          {hits.map((h, i) => {
            const pin = isPinned(h, pinned);
            return (
              <li key={`${h._index}/${h._id}`} className="flex items-center gap-3 border-b border-line px-3 py-2 last:border-0">
                <span className="w-5 text-right font-mono text-[12px] tabular-nums text-subtle">{i + 1}</span>
                <span className="w-4">{pin && <Pin className="size-3.5 text-accent" aria-label="pinned" />}</span>
                <span className="w-36 truncate font-mono text-[12px]" title={h._id}>
                  {h._id}
                </span>
                <span className="min-w-0 flex-1 truncate text-[13px] text-muted">{hitLabel(h)}</span>
                {pin ? <span className="text-[11px] text-accent">pinned</span> : <span className="font-mono text-[11px] text-subtle">{h._index}</span>}
              </li>
            );
          })}
        </ol>
      )}
      <p className="text-[12px] text-subtle">
        {hits.length} of {total.toLocaleString("en-US")} shown.
      </p>
    </div>
  );
}
