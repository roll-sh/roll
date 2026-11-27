/**
 * The Relevance tab's "Evaluate" (OpenSearch's Search Relevance Workbench, Quepid, Elastic's
 * `_rank_eval`): judged queries, pasted or dropped as JSON or CSV or picked from the lists stored in
 * the cluster, score one search configuration, or two side by side, on an index; with two, the
 * queries that got better or worse, the largest change first. Pasted judgments can be kept as a
 * list, or a list built from the index's clicks (COEC). The same run is one `roll search index
 * evaluate` line for CI.
 *
 * Nothing animates but the busy line's shimmer. Scores are mono, changes signed, and the accent
 * marks only the primary action.
 */

import { useEffect, useState } from "react";
import { searchApi, type SearchCluster } from "@/lib/search-api";
import {
  changeText,
  LIST_PREFIX,
  matchConfiguration,
  MAX_QUERIES,
  parseConfiguration,
  parseJudgments,
  scoreText,
  validListName,
  type EvalChange,
  type Evaluation,
  type JudgmentList,
} from "@/lib/search-evaluate";
import { btn, CopyButton, input, Panel, primary, useCopy } from "../ui";
import { errText } from "./relevance-shared";

const METRICS = [
  { label: "nDCG@10", value: "ndcg", metric: { dcg: { k: 10, normalize: true } } },
  { label: "Precision@10", value: "precision", metric: { precision: { k: 10, relevant_rating_threshold: 1 } } },
  { label: "Recall@10", value: "recall", metric: { recall: { k: 10, relevant_rating_threshold: 1 } } },
  { label: "MRR@10", value: "mrr", metric: { mean_reciprocal_rank: { k: 10, relevant_rating_threshold: 1 } } },
  { label: "ERR@10", value: "err", metric: { expected_reciprocal_rank: { k: 10, maximum_relevance: 3 } } },
] as const;

const SAMPLE = "query,_id,rating\nred shoes,1,3\nred shoes,7,1\nrunning shoes,4,2\n";

const pretty = (v: unknown) => JSON.stringify(v, null, 2);

export function EvaluatePanel({ cluster }: { cluster: SearchCluster }) {
  const [indices, setIndices] = useState<string[] | null>(null);
  const [index, setIndex] = useState("");
  const [judgments, setJudgments] = useState("");
  const [lists, setLists] = useState<JudgmentList[]>([]);
  // The stored list the run reads ("": the pasted judgments).
  const [list, setList] = useState("");
  const [saveAs, setSaveAs] = useState("");
  const [saved, setSaved] = useState<string | null>(null);
  const [minSearches, setMinSearches] = useState(10);
  const [building, setBuilding] = useState(false);
  const [metric, setMetric] = useState<(typeof METRICS)[number]["value"]>("ndcg");
  const [base, setBase] = useState({ name: "baseline", text: pretty(matchConfiguration("baseline", "title").request) });
  const [cand, setCand] = useState<{ name: string; text: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<Evaluation | null>(null);
  const { copied, copy } = useCopy();

  useEffect(() => {
    let alive = true;
    searchApi.indices(cluster.id).then(
      (list) => {
        if (!alive) return;
        const names = list.map((i) => i.index).filter((i) => !i.startsWith(".") && !i.startsWith(LIST_PREFIX)).sort();
        setIndices(names);
        setIndex((cur) => cur || names[0] || "");
      },
      (e) => alive && setError(errText(e)),
    );
    searchApi.judgmentLists(cluster.id).then(
      (v) => alive && setLists(v.lists),
      () => {},
    );
    return () => {
      alive = false;
    };
  }, [cluster.id]);

  const run = async () => {
    setError(null);
    let body;
    try {
      const configurations = [parseConfiguration(base.name || "baseline", base.text)];
      if (cand) configurations.push(parseConfiguration(cand.name || "candidate", cand.text));
      const m = METRICS.find((x) => x.value === metric)!.metric;
      body = list ? { judgments_list: list, configurations, metric: m } : { judgments: pasted(), configurations, metric: m };
    } catch (e) {
      setError(errText(e));
      return;
    }
    setBusy(true);
    try {
      setResult(await searchApi.evaluate(cluster.id, index, body));
    } catch (e) {
      setError(errText(e));
      setResult(null);
    } finally {
      setBusy(false);
    }
  };

  const pasted = () => {
    const js = parseJudgments(judgments);
    if (js.length > MAX_QUERIES) throw new Error(`${js.length} judged queries: at most ${MAX_QUERIES} a run.`);
    return js;
  };

  const save = async () => {
    setError(null);
    setSaved(null);
    try {
      const kept = await searchApi.putJudgmentList(cluster.id, saveAs, pasted());
      setLists((await searchApi.judgmentLists(cluster.id)).lists);
      setSaved(`Kept as ${kept.name} (${kept.queries} queries, ${kept.ratings} ratings).`);
    } catch (e) {
      setError(errText(e));
    }
  };

  const fromClicks = async () => {
    setError(null);
    setSaved(null);
    setBuilding(true);
    try {
      const kept = await searchApi.judgmentsFromClicks(cluster.id, saveAs, { index, min_searches: minSearches });
      setLists((await searchApi.judgmentLists(cluster.id)).lists);
      setList(kept.name);
    } catch (e) {
      setError(errText(e));
    } finally {
      setBuilding(false);
    }
  };

  const load = async (f: File | undefined) => {
    if (f) setJudgments(await f.text());
  };
  const chosen = lists.find((l) => l.name === list);

  if (indices && indices.length === 0) return null;
  const cli = [
    `roll search index evaluate ${cluster.name} ${index || "<index>"}`,
    list ? `--judgments-list ${list}` : "--judgments @judgments.csv",
    `--config @${base.name || "baseline"}.json`,
    cand ? `--config @${cand.name || "candidate"}.json --fail-on-worse` : "--fail-below 0.8",
  ].join(" ");

  return (
    <Panel title="Evaluate relevance">
      <div className="space-y-4 px-5 py-4">
        <p className="max-w-2xl text-[13px] text-muted">
          Score a search against queries you&apos;ve judged, or two searches against each other, before a change ships. Each judged query
          runs through the configuration with <span className="font-mono text-fg">{"{{query}}"}</span> where it goes.
        </p>
        <div className="flex flex-wrap items-center gap-2">
          <select className={`${input} py-1 font-mono text-[12px]`} value={index} onChange={(e) => setIndex(e.target.value)} aria-label="Index">
            {(indices ?? []).map((i) => (
              <option key={i} value={i}>
                {i}
              </option>
            ))}
          </select>
          <select className={`${input} py-1 text-[12px]`} value={metric} onChange={(e) => setMetric(e.target.value as typeof metric)} aria-label="Metric">
            {METRICS.map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
          </select>
        </div>
        <div className="space-y-1">
          <div className="flex flex-wrap items-center justify-between gap-2 text-[12px] text-subtle">
            <span className="flex items-center gap-2">
              Judged queries
              {lists.length > 0 && (
                <select className={`${input} py-0.5 font-mono text-[12px]`} value={list} onChange={(e) => setList(e.target.value)} aria-label="Judgment list">
                  <option value="">pasted below</option>
                  {lists.map((l) => (
                    <option key={l.name} value={l.name}>
                      {l.name}
                    </option>
                  ))}
                </select>
              )}
              {!list && <span>JSON or CSV (query, _id, rating)</span>}
            </span>
            {!list && (
              <span className="flex gap-2">
                <button type="button" className="text-subtle hover:text-fg" onClick={() => setJudgments(SAMPLE)}>
                  Example
                </button>
                <label className="cursor-pointer text-subtle hover:text-fg">
                  Open a file
                  <input type="file" accept=".csv,.json,text/csv,application/json" className="hidden" onChange={(e) => void load(e.target.files?.[0])} />
                </label>
              </span>
            )}
          </div>
          {chosen ? (
            <p className="rounded-lg border border-line bg-bg px-3 py-2 text-[12px] text-muted">
              <span className="font-mono text-fg">{chosen.index}</span> · {(chosen.queries ?? 0).toLocaleString("en-US")} queries ·{" "}
              {(chosen.ratings ?? 0).toLocaleString("en-US")} ratings · {chosen.source ?? "manual"}
              {chosen.built_at ? ` · ${new Date(chosen.built_at).toISOString().slice(0, 16).replace("T", " ")} UTC` : ""}
            </p>
          ) : (
            <>
              <textarea
                className={`${input} h-28 w-full resize-y font-mono text-[12px]`}
                value={judgments}
                onChange={(e) => setJudgments(e.target.value)}
                onDragOver={(e) => e.preventDefault()}
                onDrop={(e) => {
                  e.preventDefault();
                  void load(e.dataTransfer.files[0]);
                }}
                placeholder={SAMPLE}
                spellCheck={false}
                aria-label="Judged queries"
              />
              <div className="flex flex-wrap items-center gap-2 text-[12px] text-subtle">
                Keep in the cluster as
                <input
                  className={`${input} w-40 py-0.5 font-mono text-[12px]`}
                  value={saveAs}
                  onChange={(e) => setSaveAs(e.target.value.toLowerCase())}
                  placeholder="list-name"
                  aria-label="List name"
                />
                <button className={btn} disabled={!validListName(saveAs) || !judgments.trim()} onClick={() => void save()}>
                  Save list
                </button>
                <span>or build it from clicks on {index || "the index"}, queries searched at least</span>
                <input
                  type="number"
                  min={1}
                  className={`${input} w-16 py-0.5 font-mono text-[12px]`}
                  value={minSearches}
                  onChange={(e) => setMinSearches(Math.max(1, Number(e.target.value) || 1))}
                  aria-label="Minimum searches"
                />
                <span>times</span>
                <button className={btn} disabled={building || !validListName(saveAs) || !index} onClick={() => void fromClicks()}>
                  {building ? "Building…" : "Build from clicks"}
                </button>
                {saved && <span className="text-muted">{saved}</span>}
              </div>
            </>
          )}
        </div>
        <div className={`grid gap-3 ${cand ? "md:grid-cols-2" : ""}`}>
          <ConfigEditor label="Baseline" value={base} onChange={setBase} />
          {cand ? (
            <ConfigEditor label="Candidate" value={cand} onChange={setCand} onRemove={() => setCand(null)} />
          ) : (
            <div>
              <button className={btn} onClick={() => setCand({ name: "candidate", text: base.text })}>
                Compare with a second search
              </button>
            </div>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-3">
          <button className={primary} disabled={busy || !index || (!list && !judgments.trim())} onClick={() => void run()}>
            {busy ? "Scoring…" : cand ? "Compare" : "Score"}
          </button>
          {busy && <span className="shimmer text-[13px] text-muted">Running each judged query through {cand ? "both searches" : "the search"}…</span>}
          {error && <span className="text-[13px] text-danger">{error}</span>}
        </div>
        {result && <Results result={result} />}
        <div className="rounded-lg border border-line bg-bg px-3 py-2">
          <div className="flex items-center justify-between gap-2 pb-1 text-[11px] text-subtle">
            In CI
            <CopyButton id="cli" text={cli} copied={copied} onCopy={copy} />
          </div>
          <code className="block overflow-x-auto font-mono text-[12px] whitespace-pre text-muted">{cli}</code>
        </div>
      </div>
    </Panel>
  );
}

function ConfigEditor({
  label,
  value,
  onChange,
  onRemove,
}: {
  label: string;
  value: { name: string; text: string };
  onChange: (v: { name: string; text: string }) => void;
  onRemove?: () => void;
}) {
  return (
    <div className="space-y-1">
      <div className="flex items-center gap-2 text-[12px] text-subtle">
        {label}
        <input className={`${input} py-0.5 font-mono text-[12px]`} value={value.name} onChange={(e) => onChange({ ...value, name: e.target.value })} aria-label={`${label} name`} />
        {onRemove && (
          <button className="ml-auto text-subtle hover:text-fg" onClick={onRemove}>
            Remove
          </button>
        )}
      </div>
      <textarea
        className={`${input} h-40 w-full resize-y font-mono text-[12px]`}
        value={value.text}
        onChange={(e) => onChange({ ...value, text: e.target.value })}
        spellCheck={false}
        aria-label={`${label} search`}
      />
    </div>
  );
}

function Results({ result }: { result: Evaluation }) {
  const cmp = result.comparison;
  return (
    <div className="rise space-y-4 border-t border-line pt-4">
      <div className="flex flex-wrap gap-6">
        {result.configurations.map((c) => (
          <div key={c.name}>
            <div className="text-[11px] text-subtle">{c.name}</div>
            <div className="font-mono text-[20px] font-semibold tracking-[-0.04em] tabular-nums">{scoreText(c.score)}</div>
            {c.failed > 0 && <div className="text-[11px] text-danger">{c.failed} queries failed</div>}
          </div>
        ))}
        <div className="self-end pb-1 text-[12px] text-subtle">over {result.queries.toLocaleString("en-US")} judged queries</div>
      </div>
      {cmp && (
        <>
          <p className="text-[13px] text-muted">
            <span className="font-mono text-fg">{cmp.candidate}</span> against <span className="font-mono text-fg">{cmp.baseline}</span>:{" "}
            <span className="font-mono tabular-nums text-fg">{changeText(cmp.difference)}</span> · {cmp.better} better · {cmp.worse} worse · {cmp.unchanged}{" "}
            unchanged
          </p>
          <div className="grid gap-4 md:grid-cols-2">
            <Changes title="Better" rows={cmp.better_queries} />
            <Changes title="Worse" rows={cmp.worse_queries} />
          </div>
        </>
      )}
      {result.configurations.flatMap((c) => c.queries.filter((q) => q.failure).map((q) => ({ c: c.name, q }))).slice(0, 5).map(({ c, q }) => (
        <p key={`${c}/${q.query}`} className="text-[12px] text-danger">
          <span className="font-mono">{c}</span> · {q.query}: {q.failure}
        </p>
      ))}
    </div>
  );
}

function Changes({ title, rows }: { title: string; rows: EvalChange[] }) {
  return (
    <div>
      <div className="pb-1 text-[12px] font-medium">{title}</div>
      {rows.length === 0 ? (
        <p className="text-[12px] text-subtle">None.</p>
      ) : (
        <table className="w-full table-fixed text-[12px]">
          <thead>
            <tr className="text-[11px] text-subtle">
              <th className="pb-1 text-left font-normal">Query</th>
              <th className="w-16 pb-1 text-right font-normal">Baseline</th>
              <th className="w-16 pb-1 text-right font-normal">Candidate</th>
              <th className="w-20 pb-1 text-right font-normal">Change</th>
            </tr>
          </thead>
          <tbody>
            {rows.slice(0, 10).map((r) => (
              <tr key={r.query} className="border-b border-line last:border-0">
                <td className="truncate py-1.5 pr-2">{r.query}</td>
                <td className="w-16 py-1.5 text-right font-mono tabular-nums text-subtle">{scoreText(r.baseline)}</td>
                <td className="w-16 py-1.5 text-right font-mono tabular-nums text-subtle">{scoreText(r.candidate)}</td>
                <td className="w-20 py-1.5 text-right font-mono tabular-nums">{changeText(r.difference)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
