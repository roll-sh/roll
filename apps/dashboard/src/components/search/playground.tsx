import { ChevronRight, Columns2, Search } from "lucide-react";
import type { SourceFields } from "@/lib/search-chat";
import { useEffect, useMemo, useRef, useState } from "react";
import { esJson, esRaw, type Method, type SearchCluster } from "@/lib/search-api";
import { mappingFields } from "@/lib/search-console";
import {
  buildRequest,
  copyAs,
  defaultConfig,
  emParts,
  explainQueries,
  explainRequest,
  explainRows,
  explanationOf,
  formatScore,
  hitKey,
  LANGUAGES,
  MODES,
  movementLabel,
  needsEmbedding,
  offerOf,
  QUERY_TOKEN,
  rankMovement,
  rawFrom,
  sameTarget,
  semanticTargets,
  type Config,
  type ExplainRow,
  type Inputs,
  type Language,
  type Mode,
  type Movement,
  type Offer,
  type PlaygroundRequest,
  type SemanticTarget,
} from "@/lib/search-playground";
import { useMarkState } from "../mark-state";
import { Segmented } from "../redis/shared";
import { btn, input, Panel, primary, useCopy } from "../ui";
import { Chat } from "./playground-chat";
import { RequestEditor } from "./request-editor";

/**
 * The Playground tab: one query against two configurations side by side (keyword, semantic,
 * hybrid or a raw body), each hit with its rank, score, passages, how it moved between A and B,
 * and its `_explain` tree; either side copies as curl, Python or JavaScript. Its Chat view
 * (`playground-chat.tsx`) answers questions from an index's hits through a `chat_completion`
 * endpoint. Everything is a `_search`, `_explain`, `_mapping` or `_inference` call through the
 * console routes.
 */

interface Hit {
  _index: string;
  _id: string;
  _score: number | null;
  _source?: Record<string, unknown>;
  highlight?: Record<string, string[]>;
}

type Result =
  | { status: "running"; step: string }
  | { status: "error"; reason: string }
  /** Nothing to search with yet (no field picked, none in the mapping). */
  | { status: "blocked"; reason: string }
  | { status: "done"; hits: Hit[]; total: string; took: number | null; cfg: Config; inputs: Inputs };

interface Target {
  name: string;
  kind: "index" | "data stream";
}

const SIDES = ["A", "B"] as const;
type View = "compare" | "chat";
const SIZES = [10, 20, 50];

function message(e: unknown) {
  return e instanceof Error ? e.message : String(e);
}

function targetLabel(t: SemanticTarget) {
  return t.kind === "semantic" ? `${t.field} · semantic_text` : `${t.field} · dense_vector via ${t.inferenceId}`;
}

function targetKey(t: SemanticTarget) {
  return t.kind === "semantic" ? `s:${t.field}` : `k:${t.field}:${t.inferenceId}`;
}

export function Playground({ cluster, onOpenInference }: { cluster: SearchCluster; onOpenInference: () => void }) {
  const { setState, play } = useMarkState();
  const elastic = cluster.flavor !== "opensearch";
  const [targets, setTargets] = useState<Target[] | null>(null);
  const [endpoints, setEndpoints] = useState<string[]>([]);
  const [chatEndpoints, setChatEndpoints] = useState<string[]>([]);
  const [view, setView] = useState<View>("compare");
  /** The Chat view's retrieval: hybrid with rrf where the mapping allows it. */
  const [chatConfig, setChatConfig] = useState<Config | null>(null);
  const [listError, setListError] = useState<string | null>(null);
  const [index, setIndex] = useState("");
  const [offer, setOffer] = useState<Offer | null>(null);
  const [mappingError, setMappingError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [size, setSize] = useState(10);
  const [compare, setCompare] = useState(true);
  const [configs, setConfigs] = useState<[Config, Config] | null>(null);
  const [results, setResults] = useState<[Result | null, Result | null]>([null, null]);
  /** The query's embeddings, by endpoint and query. */
  const vectors = useRef(new Map<string, number[]>());
  const runId = useRef(0);

  // Indices (not hidden ones) and data streams; listing wakes the cluster, once, when the tab opens.
  useEffect(() => {
    let alive = true;
    Promise.all([
      esJson<{ index: string }[]>(cluster.id, "GET", "/_cat/indices?format=json&h=index&s=index"),
      esJson<{ data_streams: { name: string }[] }>(cluster.id, "GET", "/_data_stream").catch(() => ({ data_streams: [] })),
      elastic ? esJson<{ endpoints: { inference_id: string; task_type: string }[] }>(cluster.id, "GET", "/_inference/_all").catch(() => ({ endpoints: [] })) : null,
    ]).then(
      ([ix, ds, inf]) => {
        if (!alive) return;
        setTargets([
          ...ix.filter((r) => !r.index.startsWith(".")).map((r): Target => ({ name: r.index, kind: "index" })),
          ...ds.data_streams.map((d): Target => ({ name: d.name, kind: "data stream" })),
        ]);
        // Built-in endpoints need ML nodes, which roll doesn't run.
        const own = (inf?.endpoints ?? []).filter((e) => !e.inference_id.startsWith("."));
        setEndpoints(own.filter((e) => e.task_type === "text_embedding").map((e) => e.inference_id));
        setChatEndpoints(own.filter((e) => e.task_type === "chat_completion").map((e) => e.inference_id));
      },
      (e) => alive && setListError(message(e)),
    );
    return () => {
      alive = false;
    };
  }, [cluster.id, elastic]);

  // The picked index's mapping decides what each mode offers.
  useEffect(() => {
    setOffer(null);
    setConfigs(null);
    setChatConfig(null);
    setResults([null, null]);
    setMappingError(null);
    if (!index) return;
    let alive = true;
    esJson(cluster.id, "GET", `/${encodeURIComponent(index)}/_mapping`).then(
      (m) => {
        if (!alive) return;
        const o = offerOf(mappingFields(m));
        const sem = elastic ? semanticTargets(o, endpoints) : [];
        setOffer(o);
        setCompare(sem.length > 0);
        setConfigs([defaultConfig("keyword", o, sem), defaultConfig(sem.length ? "semantic" : "keyword", o, sem)]);
        setChatConfig(defaultConfig(sem.length && o.text.length ? "hybrid" : sem.length ? "semantic" : "keyword", o, sem));
      },
      (e) => alive && setMappingError(message(e)),
    );
    return () => {
      alive = false;
    };
  }, [cluster.id, index, endpoints, elastic]);

  const sem = useMemo(() => (offer && elastic ? semanticTargets(offer, endpoints) : []), [offer, endpoints, elastic]);
  const sides = compare ? [0, 1] : [0];

  const setConfig = (i: number, c: Config) => setConfigs((cs) => (cs ? (i === 0 ? [c, cs[1]] : [cs[0], c]) : cs));
  const setResult = (i: number, r: Result | null) => setResults((rs) => (i === 0 ? [r, rs[1]] : [rs[0], r]));

  /** The inputs for `q`, with whatever embeddings are already known. */
  const inputsFor = (q: string): Inputs => {
    const known: Record<string, number[]> = {};
    for (const id of endpoints) {
      const v = vectors.current.get(`${id}\u0000${q}`);
      if (v) known[id] = v;
    }
    return { query: q, size, vectors: known };
  };

  const embed = async (id: string, q: string) => {
    const key = `${id}\u0000${q}`;
    const known = vectors.current.get(key);
    if (known) return known;
    const r = await esJson<{ text_embedding?: { embedding: number[] }[] }>(cluster.id, "POST", `/_inference/text_embedding/${encodeURIComponent(id)}`, {
      input: q,
    });
    const v = r.text_embedding?.[0]?.embedding;
    if (!v) throw new Error(`${id} answered without an embedding`);
    vectors.current.set(key, v);
    return v;
  };

  const runSide = async (i: number, cfg: Config, q: string, run: number): Promise<boolean> => {
    const live = () => runId.current === run;
    try {
      const id = needsEmbedding(cfg);
      if (id) {
        setResult(i, { status: "running", step: `Embedding the query with ${id}…` });
        await embed(id, q);
      }
      const inputs = inputsFor(q);
      const built = buildRequest(cfg, index, inputs);
      if (!built.ok) {
        if (live()) setResult(i, { status: cfg.mode === "raw" ? "error" : "blocked", reason: built.reason });
        return cfg.mode !== "raw";
      }
      if (live()) setResult(i, { status: "running", step: "Searching…" });
      const { method, path, body } = built.request;
      const r = await esJson<{ took?: number; hits?: { total?: { value: number; relation: string } | number; hits?: Hit[] } }>(
        cluster.id,
        method as Method,
        path,
        body,
      );
      const t = r.hits?.total;
      const total = t == null ? "" : typeof t === "number" ? String(t) : `${t.relation === "gte" ? "≥ " : ""}${t.value}`;
      if (live()) setResult(i, { status: "done", hits: r.hits?.hits ?? [], total, took: r.took ?? null, cfg, inputs });
      return true;
    } catch (e) {
      if (live()) setResult(i, { status: "error", reason: message(e) });
      return false;
    }
  };

  const run = async () => {
    if (!configs || !index || !query.trim()) return;
    const n = ++runId.current;
    setState("thinking");
    if (!compare) setResult(1, null);
    const ok = await Promise.all(sides.map((i) => runSide(i, configs[i], query, n)));
    if (runId.current === n) play([[ok.every(Boolean) ? "success" : "error", 1200]]);
  };

  const done = (r: Result | null) => (r?.status === "done" ? r : null);
  const moves = useMemo(() => {
    const a = done(results[0]);
    const b = done(results[1]);
    if (!compare || !a || !b) return null;
    return rankMovement(a.hits.map(hitKey), b.hits.map(hitKey));
  }, [results, compare]);

  const running = results.some((r) => r?.status === "running");
  const chatting = view === "chat" && elastic;

  return (
    <div className="space-y-4">
      <Panel
        title="Playground"
        aside={<span className="hidden text-[12px] text-subtle sm:inline">{chatting ? "answers from an index's hits, with citations" : "keyword, semantic and hybrid search side by side"}</span>}
      >
        <form
          className="flex flex-wrap items-center gap-2 px-5 py-4"
          onSubmit={(e) => {
            e.preventDefault();
            run();
          }}
        >
          <select
            value={index}
            onChange={(e) => setIndex(e.target.value)}
            className={`${input} w-full font-mono text-[12px] sm:w-56`}
            aria-label="Index or data stream"
            disabled={!targets}
          >
            <option value="">{targets ? "Pick an index…" : listError ? "Couldn't list indices" : "Listing indices…"}</option>
            {targets && targets.some((t) => t.kind === "index") && (
              <optgroup label="Indices">
                {targets
                  .filter((t) => t.kind === "index")
                  .map((t) => (
                    <option key={t.name} value={t.name}>
                      {t.name}
                    </option>
                  ))}
              </optgroup>
            )}
            {targets && targets.some((t) => t.kind === "data stream") && (
              <optgroup label="Data streams">
                {targets
                  .filter((t) => t.kind === "data stream")
                  .map((t) => (
                    <option key={t.name} value={t.name}>
                      {t.name}
                    </option>
                  ))}
              </optgroup>
            )}
          </select>
          {elastic && (
            <Segmented
              value={view}
              options={[
                { label: "compare", value: "compare" },
                { label: "chat", value: "chat" },
              ]}
              onChange={setView}
            />
          )}
          {!chatting && (
            <>
              <label className={`${input} flex min-w-0 flex-1 basis-64 items-center gap-2`}>
                <Search className="size-3.5 shrink-0 text-subtle" strokeWidth={1.5} />
                <input
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  placeholder="Type a query once; both sides search it"
                  aria-label="Query"
                  className="min-w-0 flex-1 bg-transparent outline-none placeholder:text-subtle"
                />
              </label>
              <select value={size} onChange={(e) => setSize(Number(e.target.value))} className={`${input} font-mono text-[12px]`} aria-label="Hits per side">
                {SIZES.map((s) => (
                  <option key={s} value={s}>
                    {s} hits
                  </option>
                ))}
              </select>
              <button
                type="button"
                onClick={() => setCompare((c) => !c)}
                aria-pressed={compare}
                className={`${btn} ${compare ? "border-line-2 text-fg" : ""}`}
                title="Compare two configurations side by side"
              >
                <Columns2 className="size-3.5" strokeWidth={1.5} /> Compare
              </button>
              <button type="submit" className={primary} disabled={!configs || !query.trim() || running}>
                {running ? <span className="shimmer">Searching…</span> : "Search"}
              </button>
            </>
          )}
        </form>
        {listError && <p className="border-t border-line px-5 py-3 text-[13px] text-danger">{listError}</p>}
      </Panel>

      {!index ? (
        <Panel title="Nothing picked yet">
          <p className="px-5 py-6 text-[13px] text-muted">
            Pick an index or data stream, type a query, and compare how keyword, semantic and hybrid search rank its documents{elastic ? ", or chat with them" : ""}.
            {targets && targets.length === 0 && " This cluster has no indices yet: import a file or create one in Indices."}
          </p>
        </Panel>
      ) : mappingError ? (
        <Panel title={index}>
          <p className="px-5 py-4 text-[13px] text-danger">{mappingError}</p>
        </Panel>
      ) : !configs || !offer ? (
        <p className="px-1 text-[13px] text-subtle">
          <span className="shimmer">Reading the mapping…</span>
        </p>
      ) : chatting && chatConfig ? (
        <Chat
          cluster={cluster}
          index={index}
          cfg={chatConfig}
          fields={chatFields(chatConfig, offer)}
          settings={<ModeSettings cfg={chatConfig} offer={offer} targets={sem} cluster={cluster} onChange={setChatConfig} onRun={() => {}} onOpenInference={onOpenInference} />}
          endpoints={chatEndpoints}
          embed={embed}
          onOpenInference={onOpenInference}
        />
      ) : (
        <div className={`grid gap-4 ${compare ? "lg:grid-cols-2" : ""}`}>
          {sides.map((i) => (
            <Side
              key={i}
              label={SIDES[i]}
              cluster={cluster}
              index={index}
              offer={offer}
              targets={sem}
              cfg={configs[i]}
              onChange={(c) => setConfig(i, c)}
              result={results[i]}
              inputs={inputsFor(query)}
              moves={moves ? (i === 0 ? moves.a : moves.b) : null}
              onRun={run}
              onOpenInference={onOpenInference}
            />
          ))}
        </div>
      )}
    </div>
  );
}

function Side({
  label,
  cluster,
  index,
  offer,
  targets,
  cfg,
  onChange,
  result,
  inputs,
  moves,
  onRun,
  onOpenInference,
}: {
  label: string;
  cluster: SearchCluster;
  index: string;
  offer: Offer;
  targets: SemanticTarget[];
  cfg: Config;
  onChange: (c: Config) => void;
  result: Result | null;
  inputs: Inputs;
  moves: Map<string, Movement> | null;
  onRun: () => void;
  onOpenInference: () => void;
}) {
  const { copied, copy } = useCopy();
  const elastic = cluster.flavor !== "opensearch";
  const modes = elastic ? MODES : (["keyword", "raw"] as Mode[]);
  const built = buildRequest(cfg, index, inputs);
  const request: PlaygroundRequest | null = built.ok ? built.request : null;

  const setMode = (mode: Mode) => onChange({ ...cfg, mode, raw: mode === "raw" && !cfg.raw ? rawFrom(cfg, index, inputs) : cfg.raw });

  return (
    <Panel
      title={
        <span className="flex items-center gap-2">
          <span className="font-mono text-[12px] text-subtle">{label}</span>
          <span className="capitalize">{cfg.mode}</span>
        </span>
      }
      aside={
        <span className="flex items-center gap-2 text-[11px] text-subtle">
          <span className="hidden sm:inline">Copy as</span>
          {LANGUAGES.map((lang: Language) => (
            <button
              key={lang}
              type="button"
              disabled={!request}
              onClick={() => request && copy(lang, copyAs(lang, request, { url: cluster.url, flavor: cluster.flavor }))}
              title={request ? `Copy ${label}'s request as ${lang}` : built.ok ? undefined : built.reason}
              className="font-mono transition hover:text-fg disabled:opacity-40"
            >
              {copied === lang ? "copied" : lang === "javascript" ? "js" : lang}
            </button>
          ))}
        </span>
      }
    >
      <div className="space-y-3 border-b border-line px-5 py-4">
        <Segmented value={cfg.mode} options={modes.map((m) => ({ label: m, value: m }))} onChange={setMode} />
        <ModeSettings cfg={cfg} offer={offer} targets={targets} cluster={cluster} onChange={onChange} onRun={onRun} onOpenInference={onOpenInference} />
      </div>
      <Results result={result} moves={moves} cluster={cluster} index={index} />
    </Panel>
  );
}

function ModeSettings({
  cfg,
  offer,
  targets,
  cluster,
  onChange,
  onRun,
  onOpenInference,
}: {
  cfg: Config;
  offer: Offer;
  targets: SemanticTarget[];
  cluster: SearchCluster;
  onChange: (c: Config) => void;
  onRun: () => void;
  onOpenInference: () => void;
}) {
  const keyword = cfg.mode === "keyword" || cfg.mode === "hybrid";
  const semantic = cfg.mode === "semantic" || cfg.mode === "hybrid";

  if (cfg.mode === "raw") {
    return (
      <div className="-mx-5 -mb-4 border-t border-line">
        <RequestEditor cluster={cluster} value={cfg.raw} onChange={(raw) => onChange({ ...cfg, raw })} onSubmit={onRun} />
        <p className="border-t border-line px-5 py-2 text-[11px] text-subtle">
          <span className="font-mono">{QUERY_TOKEN}</span> stands for the query above. ⌘/Ctrl + Enter searches.
        </p>
      </div>
    );
  }
  if (semantic && targets.length === 0) {
    return (
      <p className="text-[13px] text-muted">
        This index has no <span className="font-mono">semantic_text</span> field, and no <span className="font-mono">dense_vector</span> field with a
        text-embedding endpoint to embed the query.{" "}
        <button type="button" onClick={onOpenInference} className="text-accent hover:underline">
          Set one up in Inference
        </button>
        .
      </p>
    );
  }
  return (
    <div className="space-y-3">
      {keyword && (
        <div className="space-y-1.5">
          <p className="text-[11px] text-subtle">Text fields</p>
          {offer.text.length === 0 ? (
            <p className="text-[12px] text-muted">The mapping has no text fields.</p>
          ) : (
            <div className="flex flex-wrap gap-1">
              {offer.text.map((f) => {
                const on = cfg.fields.includes(f);
                return (
                  <button
                    key={f}
                    type="button"
                    aria-pressed={on}
                    onClick={() => onChange({ ...cfg, fields: on ? cfg.fields.filter((x) => x !== f) : offer.text.filter((x) => x === f || cfg.fields.includes(x)) })}
                    className={`rounded-full border px-2.5 py-0.5 font-mono text-[11px] transition ${
                      on ? "border-line-2 bg-panel-2 text-fg" : "border-line text-subtle hover:text-fg"
                    }`}
                  >
                    {f}
                  </button>
                );
              })}
            </div>
          )}
        </div>
      )}
      {semantic && (
        <label className="block space-y-1.5">
          <span className="block text-[11px] text-subtle">Semantic field</span>
          <select
            value={cfg.semantic ? targetKey(cfg.semantic) : ""}
            onChange={(e) => onChange({ ...cfg, semantic: targets.find((t) => targetKey(t) === e.target.value) ?? null })}
            className={`${input} w-full font-mono text-[12px]`}
          >
            {!targets.some((t) => sameTarget(t, cfg.semantic)) && <option value="">Pick a field…</option>}
            {targets.map((t) => (
              <option key={targetKey(t)} value={targetKey(t)}>
                {targetLabel(t)}
              </option>
            ))}
          </select>
        </label>
      )}
      {cfg.mode === "hybrid" && (
        <div className="flex flex-wrap items-end gap-3">
          <div className="space-y-1.5">
            <p className="text-[11px] text-subtle">Fusion</p>
            <Segmented
              value={cfg.fusion}
              options={[
                { label: "rrf", value: "rrf" },
                { label: "linear", value: "linear" },
              ]}
              onChange={(fusion) => onChange({ ...cfg, fusion })}
            />
          </div>
          {cfg.fusion === "linear" &&
            (["keyword", "semantic"] as const).map((part, j) => (
              <label key={part} className="space-y-1.5">
                <span className="block text-[11px] text-subtle">{part} weight</span>
                <input
                  type="number"
                  min={0}
                  step={0.1}
                  value={cfg.weights[j]}
                  onChange={(e) => {
                    const w = Math.max(0, Number(e.target.value) || 0);
                    onChange({ ...cfg, weights: j === 0 ? [w, cfg.weights[1]] : [cfg.weights[0], w] });
                  }}
                  className={`${input} w-20 font-mono text-[12px]`}
                />
              </label>
            ))}
          <p className="basis-full text-[11px] text-subtle">
            {cfg.fusion === "rrf"
              ? "Reciprocal rank fusion: each part adds 1 / (60 + its rank), so scores' scales don't matter."
              : "Each part's scores are min–max normalized to 0–1, then weighted and summed."}
          </p>
        </div>
      )}
    </div>
  );
}

/** What the Chat view shows of a hit, and sends when it has no passages: its title field and
 * the fields it searches. */
function chatFields(cfg: Config, offer: Offer): SourceFields {
  const fields = cfg.mode === "raw" ? offer.text : cfg.fields.length ? cfg.fields : offer.text;
  const title = titleField(fields) ?? null;
  return { title, text: [...fields.filter((f) => f !== title), ...(cfg.semantic?.kind === "semantic" ? [cfg.semantic.field] : [])] };
}

/** The field shown as a hit's title: a text field named like one, else the first text field. */
function titleField(fields: string[]) {
  return fields.find((f) => /^(title|name|subject|headline)$/i.test(f)) ?? fields[0];
}

function sourceText(src: Record<string, unknown> | undefined, field: string | undefined): string | null {
  if (!src || !field) return null;
  let v: unknown = src;
  for (const part of field.split(".")) v = v && typeof v === "object" ? (v as Record<string, unknown>)[part] : undefined;
  if (typeof v === "string") return v;
  if (v && typeof v === "object" && typeof (v as { text?: unknown }).text === "string") return (v as { text: string }).text;
  return null;
}

function Results({ result, moves, cluster, index }: { result: Result | null; moves: Map<string, Movement> | null; cluster: SearchCluster; index: string }) {
  if (!result) return <p className="px-5 py-6 text-[13px] text-subtle">Search to see this side&apos;s hits.</p>;
  if (result.status === "running")
    return (
      <p className="px-5 py-6 text-[13px] text-subtle">
        <span className="shimmer">{result.step}</span>
      </p>
    );
  if (result.status === "error") return <p className="rise break-words px-5 py-4 text-[13px] text-danger">{result.reason}</p>;
  if (result.status === "blocked") return <p className="px-5 py-6 text-[13px] text-subtle">{result.reason}</p>;
  if (result.hits.length === 0)
    return <p className="rise px-5 py-6 text-[13px] text-muted">No hits for this query. Try other words, or other fields.</p>;
  const fields = result.cfg.mode === "raw" ? [] : result.cfg.fields;
  const title = titleField(fields.length ? fields : Object.keys(result.hits[0]._source ?? {}));
  // What a hit without passages (a knn match, a raw body without highlight) shows instead.
  const textFields = [...fields, ...(result.cfg.semantic ? [result.cfg.semantic.field] : [])].filter((f) => f !== title);
  return (
    <div className="rise">
      <p className="border-b border-line px-5 py-2 font-mono text-[11px] text-subtle">
        {result.total && `${result.total} ${result.total === "1" ? "hit" : "hits"}`}
        {result.took != null && ` · ${result.took} ms`}
      </p>
      <ol>
        {result.hits.map((h, i) => (
          <HitRow
            key={hitKey(h)}
            hit={h}
            rank={i + 1}
            title={sourceText(h._source, title)}
            snippet={textFields.map((f) => sourceText(h._source, f)).find((t) => t) ?? null}
            move={moves?.get(hitKey(h)) ?? null}
            cluster={cluster}
            index={index}
            cfg={result.cfg}
            inputs={result.inputs}
          />
        ))}
      </ol>
    </div>
  );
}

function MoveTag({ move }: { move: Movement }) {
  const quiet = move.kind === "same";
  return (
    <span
      className={`shrink-0 font-mono text-[11px] ${quiet ? "text-subtle/60" : "text-subtle"}`}
      title={
        move.kind === "new"
          ? "Not in A's hits"
          : move.kind === "gone"
            ? "Not in B's hits"
            : move.kind === "same"
              ? "Same rank in A"
              : `${move.kind === "up" ? "Higher" : "Lower"} than in A by ${move.by}`
      }
    >
      {movementLabel(move)}
    </span>
  );
}

function Passage({ text }: { text: string }) {
  return (
    <>
      {emParts(text).map((p, i) =>
        p.em ? (
          <em key={i} className="font-medium not-italic text-fg">
            {p.text}
          </em>
        ) : (
          <span key={i}>{p.text}</span>
        ),
      )}
    </>
  );
}

type Explained = { status: "loading" } | { status: "error"; reason: string } | { status: "done"; parts: { label: string | null; rows: ExplainRow[] | null }[] };

function HitRow({
  hit,
  rank,
  title,
  snippet,
  move,
  cluster,
  index,
  cfg,
  inputs,
}: {
  hit: Hit;
  rank: number;
  title: string | null;
  snippet: string | null;
  move: Movement | null;
  cluster: SearchCluster;
  index: string;
  cfg: Config;
  inputs: Inputs;
}) {
  const [open, setOpen] = useState(false);
  const [explained, setExplained] = useState<Explained | null>(null);
  const passages = Object.entries(hit.highlight ?? {}).flatMap(([f, ps]) => ps.slice(0, 2).map((p) => ({ field: f, text: p })));

  const explain = async () => {
    setOpen((o) => !o);
    if (explained && explained.status !== "error") return;
    const queries = explainQueries(cfg, inputs);
    if (typeof queries === "string") {
      setExplained({ status: "error", reason: queries });
      return;
    }
    setExplained({ status: "loading" });
    try {
      const parts = await Promise.all(
        queries.map(async (q) => {
          const req = explainRequest(hit._index, hit._id, q.query);
          const r = await esRaw(cluster.id, req.method as Method, req.path, req.body);
          const e = explanationOf(r.value);
          return { label: q.label, rows: e ? explainRows(e) : null };
        }),
      );
      setExplained({ status: "done", parts });
    } catch (e) {
      setExplained({ status: "error", reason: message(e) });
    }
  };

  return (
    <li className="border-b border-line px-5 py-3 last:border-0">
      <div className="flex items-baseline gap-3">
        <span className="w-5 shrink-0 text-right font-mono text-[11px] text-subtle tabular-nums">{rank}</span>
        <div className="min-w-0 flex-1 space-y-1">
          <div className="flex items-baseline gap-3">
            <p className="min-w-0 flex-1 truncate text-[13px]">{title ?? <span className="font-mono text-[12px]">{hit._id}</span>}</p>
            {move && <MoveTag move={move} />}
          </div>
          <p className="truncate font-mono text-[11px] text-subtle">
            <span className="text-muted tabular-nums">{formatScore(hit._score)}</span> · {hit._id}
            {hit._index !== index && <> · {hit._index}</>}
          </p>
          {passages.length === 0 && snippet && <p className="line-clamp-2 text-[12px] leading-relaxed text-muted">{snippet}</p>}
          {passages.length > 0 && (
            <ul className="space-y-1 pt-0.5">
              {passages.slice(0, 3).map((p, i) => (
                <li key={i} className="text-[12px] leading-relaxed text-muted">
                  <span className="mr-1.5 font-mono text-[10px] text-subtle">{p.field}</span>
                  <Passage text={p.text} />
                </li>
              ))}
            </ul>
          )}
          <button
            type="button"
            onClick={explain}
            aria-expanded={open}
            className="inline-flex items-center gap-1 pt-0.5 text-[11px] text-subtle transition hover:text-fg"
          >
            <ChevronRight className={`size-3.5 transition-transform duration-200 motion-reduce:transition-none ${open ? "rotate-90" : ""}`} strokeWidth={1.5} />
            Explain
          </button>
          {open && explained && <ExplainView explained={explained} cfg={cfg} />}
        </div>
      </div>
    </li>
  );
}

function ExplainView({ explained, cfg }: { explained: Explained; cfg: Config }) {
  if (explained.status === "loading")
    return (
      <p className="text-[12px] text-subtle">
        <span className="shimmer">Explaining…</span>
      </p>
    );
  if (explained.status === "error") return <p className="rise text-[12px] text-danger">{explained.reason}</p>;
  return (
    <div className="rise space-y-2 rounded-lg border border-line bg-bg px-3 py-2">
      {cfg.mode === "hybrid" && (
        <p className="text-[11px] text-subtle">
          {cfg.fusion === "rrf"
            ? "The fused rank comes from each part's rank, not its score. Each part on its own:"
            : `The fused score is ${cfg.weights[0]} × keyword + ${cfg.weights[1]} × semantic, each min–max normalized. Each part on its own:`}
        </p>
      )}
      {explained.parts.map((p, i) => (
        <div key={i}>
          {p.label && <p className="mb-1 text-[11px] font-medium text-muted">{p.label}</p>}
          {p.rows === null ? (
            <p className="text-[12px] text-muted">This part doesn&apos;t match the document.</p>
          ) : (
            <ul className="max-h-72 overflow-auto">
              {p.rows.map((r, j) => (
                <li key={j} className="grid grid-cols-[4.5rem_minmax(0,1fr)] gap-2 py-px text-[11px] leading-snug">
                  <span className={`text-right font-mono tabular-nums ${r.depth === 0 ? "text-fg" : "text-muted"}`}>{formatScore(r.value)}</span>
                  <span className={r.depth === 0 ? "text-fg" : "text-muted"} style={{ paddingLeft: `${r.depth * 12}px` }}>
                    {r.description}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>
      ))}
    </div>
  );
}
