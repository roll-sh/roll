import { ArrowLeft, Bookmark, ChartColumn, ChevronLeft, ChevronRight, Code2, Columns3, FoldVertical, Minus, Plus, Search, SquareTerminal, X } from "lucide-react";
import { Fragment, useCallback, useEffect, useMemo, useState } from "react";
import {
  anchorBody,
  chartOf,
  CONTEXT_STEP,
  dateFields,
  DISCOVER_PAGE,
  edited,
  fromSpec,
  hashToState,
  initialState,
  localTime,
  MODES,
  piped,
  pipedTable,
  sampleBody,
  stateToHash,
  surroundingBodies,
  topValues,
  valuesField,
  viewFields,
  viewerZone,
  viewRequest,
  withFilter,
  type DiscoverState,
  type FieldChart as ChartKind,
  type ViewField,
} from "@/lib/search-discover";
import { esJson, esRaw, formatCount, type SavedSearch, type SearchCluster } from "@/lib/search-api";
import { cellText, hitSources, nextSort, sortField, sourceCells, TIME_PRESETS, timeBuckets, timeFieldOf, type TimeBucket } from "@/lib/search-docs";
import { Segmented } from "../redis/shared";
import { btn, input } from "../ui";
import { FieldChart } from "./discover-chart";
import { SavedSearches } from "./discover-saved";
import { JsonDoc, TimeHistogram } from "./index-browser";

interface Hit {
  _index: string;
  _id: string;
  source?: string;
  /** Its sort values (`search_after` from it). */
  sort?: unknown[];
  /** The time field in epoch milliseconds (`fields`), whatever `_source` holds. */
  fields?: Record<string, unknown[]>;
}

/** A search's hits with their `_source` text as sent (`hitSources`). */
async function searchHits(cluster: string, path: string, body: unknown): Promise<{ took: number; total?: { value: number; relation: string }; hits: Hit[]; value: unknown }> {
  const r = await esRaw<{ took: number; hits: { total?: { value: number; relation: string }; hits: Omit<Hit, "source">[] } }>(cluster, "POST", path, body);
  const sources = hitSources(r.text);
  return { took: r.value.took, total: r.value.hits.total, hits: r.value.hits.hits.map((h, i) => ({ ...h, source: sources[i] })), value: r.value };
}

/** What a view's query answered: a page of documents (KQL, DQL), or a piped query's table. */
type Result =
  | { kind: "hits"; took: number; total?: { value: number; relation: string }; hits: Hit[]; buckets: TimeBucket[] }
  | { kind: "table"; columns: string[]; rows: unknown[][] };

/** The names a view can be: the cluster's indices, aliases and data streams (hidden ones left out). */
async function viewNames(cluster: string): Promise<string[]> {
  const r = await esJson<{ indices?: { name: string }[]; aliases?: { name: string }[]; data_streams?: { name: string }[] }>(cluster, "GET", "/_resolve/index/*?expand_wildcards=open");
  const names = [...(r.data_streams ?? []), ...(r.aliases ?? []), ...(r.indices ?? [])].map((x) => x.name);
  return [...new Set(names)].filter((n) => !n.startsWith(".")).sort();
}

const requestText = (r: { method: string; path: string; body: unknown }) => `${r.method} ${r.path}\n${JSON.stringify(r.body, null, 2)}`;

/**
 * Discover (Kibana's and OpenSearch Dashboards' first screen for logs): a view of an index
 * pattern, alias or data stream over a time range, its documents over time, its fields with their
 * top values, and a table of the chosen columns. The query is KQL (or ES|QL) on Elasticsearch-flavor
 * clusters, DQL (or PPL) on OpenSearch ones. The whole view lives in the URL's hash, so it can be
 * bookmarked and shared, and every request it sends can be opened in the console. A view can be
 * saved under a name (`SavedSearches`), a document shown among the ones around it in time
 * (`Surrounding`), and a field drawn over the whole range (`FieldChart`). Times are the viewer's
 * zone's, and say so.
 */
export function DiscoverTab({ cluster, onOpenConsole }: { cluster: SearchCluster; onOpenConsole: (text: string) => void }) {
  const modes = MODES[cluster.flavor === "opensearch" ? "opensearch" : "elasticsearch"];
  const [state, setStateRaw] = useState<DiscoverState>(() => initialState(cluster.flavor === "opensearch" ? "opensearch" : "elasticsearch"));
  const [ready, setReady] = useState(false);
  const [draft, setDraft] = useState("");
  const [viewDraft, setViewDraft] = useState("");
  const [names, setNames] = useState<string[]>([]);
  const [fields, setFields] = useState<ViewField[] | null>(null);
  const [fieldsErr, setFieldsErr] = useState<string | null>(null);
  const [result, setResult] = useState<Result | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [from, setFrom] = useState(0);
  const [open, setOpen] = useState<string | null>(null);
  const [showRequest, setShowRequest] = useState(false);
  const [now, setNow] = useState(() => Date.now());
  const [opened, setOpened] = useState<SavedSearch | null>(null);
  const [showSaved, setShowSaved] = useState(false);
  const [chart, setChart] = useState<{ name: string; kind: ChartKind; field: string } | null>(null);
  const [context, setContext] = useState<Hit | null>(null);
  // The server renders in its own zone: the viewer's is read once mounted.
  const [zone, setZone] = useState("UTC");

  // The view in the URL is read once mounted (the server render has no hash).
  useEffect(() => {
    const s = hashToState(window.location.hash, state, modes.map((m) => m.value));
    setStateRaw(s);
    setDraft(s.query);
    setViewDraft(s.view);
    setZone(viewerZone());
    setReady(true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const setState = useCallback((f: (s: DiscoverState) => DiscoverState) => {
    setStateRaw(f);
    setFrom(0);
    setOpen(null);
    setContext(null);
    setNow(Date.now());
  }, []);
  // A new view starts over: its own time field, no columns, no sort, no chart.
  const commitView = (v: string) => {
    const view = v.trim();
    if (view === state.view) return;
    setChart(null);
    setState((s) => ({ ...s, view, timeField: undefined, columns: [], sort: null }));
  };
  const openSaved = (saved: SavedSearch) => {
    const s = fromSpec(saved.search, modes.map((m) => m.value));
    setOpened(saved);
    setChart(null);
    setDraft(s.query);
    setViewDraft(s.view);
    setState(() => s);
  };
  // The view goes in the URL once it's rendered (an updater runs during a render, where the
  // router mustn't hear of a new URL).
  useEffect(() => {
    if (ready) window.history.replaceState(null, "", `${window.location.pathname}${window.location.search}${stateToHash(state)}`);
  }, [ready, state]);

  useEffect(() => {
    let live = true;
    viewNames(cluster.id).then(
      (n) => live && setNames(n),
      () => live && setNames([]),
    );
    return () => {
      live = false;
    };
  }, [cluster.id]);

  // The view's fields, and its time field when the URL doesn't name one.
  useEffect(() => {
    if (!ready || !state.view) return;
    let live = true;
    setFields(null);
    setFieldsErr(null);
    esJson(cluster.id, "GET", `/${encodeURIComponent(state.view)}/_field_caps?fields=*&ignore_unavailable=true&allow_no_indices=true`).then(
      (caps) => {
        if (!live) return;
        const f = viewFields(caps);
        setFields(f);
        if (state.timeField === undefined) setState((s) => ({ ...s, timeField: timeFieldOf(f) }));
      },
      (e) => {
        if (!live) return;
        setFields([]);
        setFieldsErr(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cluster.id, ready, state.view]);

  const request = useMemo(() => (state.view && state.timeField !== undefined ? viewRequest(state, now, from) : null), [state, now, from]);

  const load = useCallback(async () => {
    if (!request) return;
    setLoading(true);
    try {
      if (piped(state.mode)) {
        const r = await esJson(cluster.id, request.method, request.path, request.body);
        setResult({ kind: "table", ...pipedTable(r) });
      } else {
        const r = await searchHits(cluster.id, request.path, request.body);
        setResult((prev) => ({
          kind: "hits",
          took: r.took,
          total: r.total,
          hits: r.hits,
          // Later pages keep the first page's histogram.
          buckets: from === 0 ? timeBuckets(r.value) : prev?.kind === "hits" ? prev.buckets : [],
        }));
      }
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      setResult(null);
    } finally {
      setLoading(false);
    }
  }, [cluster.id, request, state.mode, from]);

  useEffect(() => {
    load();
  }, [load]);

  const consoleText = request ? requestText(request) : "";
  const dates = fields ? dateFields(fields) : [];
  const toggleColumn = (name: string) => setState((s) => ({ ...s, columns: s.columns.includes(name) ? s.columns.filter((c) => c !== name) : [...s.columns, name] }));
  const filter = (field: string, value: string | number | boolean, exclude: boolean) => {
    const q = withFilter(state.mode, state.query, field, value, exclude);
    setDraft(q);
    setState((s) => ({ ...s, query: q }));
  };

  return (
    <div className="space-y-4">
      <form
        className="flex flex-wrap items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          commitView(viewDraft);
          setState((s) => ({ ...s, query: draft }));
        }}
      >
        <input
          aria-label="View"
          list={`views-${cluster.id}`}
          value={viewDraft}
          onChange={(e) => {
            setViewDraft(e.target.value);
            // A pick from the list is a choice; typing waits for Enter or leaving the field.
            if (names.includes(e.target.value)) commitView(e.target.value);
          }}
          onBlur={() => commitView(viewDraft)}
          placeholder="Index, alias, data stream or pattern (logs-*)"
          spellCheck={false}
          className={`${input} w-64 max-w-full font-mono text-[12px]`}
        />
        <datalist id={`views-${cluster.id}`}>
          {names.map((n) => (
            <option key={n} value={n} />
          ))}
        </datalist>
        {state.view && fields && (
          <select
            aria-label="Time field"
            value={state.timeField ?? ""}
            onChange={(e) => setState((s) => ({ ...s, timeField: e.target.value || null, sort: null }))}
            className={`${input} font-mono text-[12px]`}
          >
            <option value="">no time field</option>
            {dates.map((d) => (
              <option key={d} value={d}>
                {d}
              </option>
            ))}
          </select>
        )}
        <div className="w-36">
          <Segmented value={state.mode} options={modes} onChange={(mode) => setState((s) => ({ ...s, mode, query: "" }))} />
        </div>
        {/* A row of its own on a phone. */}
        <div className="relative min-w-0 flex-1 basis-full sm:basis-0">
          <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-subtle" strokeWidth={1.5} />
          <input
            aria-label="Query"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            placeholder={
              state.mode === "esql"
                ? "After FROM and the time range: WHERE status >= 500 | STATS count = COUNT(*) BY host"
                : state.mode === "ppl"
                  ? "After source and the time range: where status >= 500 | stats count() by host"
                  : "status >= 500 and host: web-* (empty: every document)"
            }
            spellCheck={false}
            className={`${input} w-full pl-8 font-mono text-[12px]`}
          />
        </div>
        <button type="submit" disabled={loading || !viewDraft.trim()} className={btn}>
          Search
        </button>
        <button type="button" onClick={() => setShowRequest((v) => !v)} disabled={!request} className={btn} aria-pressed={showRequest} title="Show the request this view sends">
          <Code2 className="size-3.5" strokeWidth={1.5} /> Request
        </button>
        <button type="button" onClick={() => onOpenConsole(consoleText)} disabled={!request} className={btn} title="Open this request in the console">
          <SquareTerminal className="size-3.5" strokeWidth={1.5} /> Console
        </button>
        <button type="button" onClick={() => setShowSaved((v) => !v)} className={btn} aria-pressed={showSaved} aria-label="Saved searches" title={opened ? `Opened from ${opened.name}` : "Open or save a saved search"}>
          <Bookmark className="size-3.5" strokeWidth={1.5} />
          <span className="max-w-40 truncate">{opened ? opened.name : "Saved"}</span>
          {opened && edited(state, opened.search) && <span className="size-1.5 rounded-full bg-accent" title="Changed since it was saved" />}
        </button>
      </form>

      {showSaved && <SavedSearches cluster={cluster.id} state={state} opened={opened} onOpen={openSaved} onSaved={setOpened} />}

      {showRequest && request && (
        <pre className="rise max-h-64 overflow-auto rounded-lg border border-line bg-panel px-4 py-3 font-mono text-[11px] leading-relaxed text-muted">{consoleText}</pre>
      )}

      {!state.view ? (
        <div className="rounded-lg border border-line bg-panel px-5 py-8 text-[13px] text-muted">
          Pick an index, alias or data stream, or type a pattern such as <span className="font-mono text-fg">logs-*</span>, to see its documents over time.
        </div>
      ) : (
        <div className="grid gap-4 lg:grid-cols-[16rem_minmax(0,1fr)]">
          <FieldList
            cluster={cluster}
            state={state}
            fields={fields}
            error={fieldsErr}
            zone={zone}
            chart={chart?.name ?? null}
            onFilter={filter}
            onToggleColumn={toggleColumn}
            onVisualize={(name) => {
              const c = fields && chartOf(fields, name);
              setChart(c && chart?.name !== name ? { name, ...c } : null);
            }}
          />
          {context && state.timeField ? (
            <Surrounding cluster={cluster} state={state} fields={fields ?? []} timeField={state.timeField} anchor={context} zone={zone} onBack={() => setContext(null)} onOpenConsole={onOpenConsole} />
          ) : (
            <div className="min-w-0 overflow-hidden rounded-lg border border-line bg-panel">
              {state.timeField && !piped(state.mode) && result?.kind === "hits" && (
                <TimeHistogram field={state.timeField} range={state.range} buckets={result.buckets} total={result.total} onRange={(range) => setState((s) => ({ ...s, range }))} />
              )}
              {state.timeField && (piped(state.mode) || !result) && (
                <div className="border-b border-line px-5 py-3">
                  <RangeOnly state={state} zone={zone} onRange={(range) => setState((s) => ({ ...s, range }))} />
                </div>
              )}
              {chart && (
                <FieldChart
                  cluster={cluster.id}
                  state={state}
                  kind={chart.kind}
                  field={chart.field}
                  name={chart.name}
                  zone={zone}
                  onFilter={filter}
                  onRange={(range) => setState((s) => ({ ...s, range }))}
                  onClose={() => setChart(null)}
                />
              )}
              {err ? (
                <p role="alert" className="px-5 py-4 font-mono text-[12px] text-danger">
                  {err}
                </p>
              ) : !result ? (
                <p className="px-5 py-6 text-[13px] text-subtle">
                  <span className="shimmer">Searching…</span>
                </p>
              ) : result.kind === "table" ? (
                <PipedTable result={result} />
              ) : (
                <Documents
                  cluster={cluster}
                  state={state}
                  fields={fields ?? []}
                  hits={result.hits}
                  zone={zone}
                  loading={loading}
                  open={open}
                  onOpen={setOpen}
                  onSort={(column, field) => setState((s) => ({ ...s, sort: nextSort(s.sort, column, field) }))}
                  onRemoveColumn={toggleColumn}
                  onSurround={state.timeField ? setContext : undefined}
                  onOpenConsole={onOpenConsole}
                />
              )}
              {result?.kind === "hits" && result.total && (
                <div className="flex items-center justify-between gap-3 border-t border-line px-5 py-2.5 text-[12px] text-subtle">
                  <span className="font-mono tabular-nums">
                    {result.hits.length ? `${formatCount(from + 1)}–${formatCount(from + result.hits.length)}` : "0"} of {result.total.relation === "gte" ? "≥" : ""}
                    {formatCount(result.total.value)} · {result.took} ms
                  </span>
                  <span className="flex gap-1">
                    <button onClick={() => setFrom(Math.max(0, from - DISCOVER_PAGE))} disabled={loading || from === 0} className={btn} aria-label="Previous page">
                      <ChevronLeft className="size-3.5" strokeWidth={1.5} />
                    </button>
                    <button
                      onClick={() => setFrom(from + DISCOVER_PAGE)}
                      disabled={loading || result.hits.length < DISCOVER_PAGE || from + DISCOVER_PAGE >= Math.min(result.total.value, 10_000)}
                      className={btn}
                      aria-label="Next page"
                    >
                      <ChevronRight className="size-3.5" strokeWidth={1.5} />
                    </button>
                  </span>
                </div>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/**
 * The documents around one in time (Kibana's "View surrounding documents"): the anchor, the
 * newer ones above it and the older ones below, found with `search_after` from the anchor's own
 * sort values (the time field, then `_doc`) both ways. The range doesn't apply; the view's query
 * does when asked to. More load five at a time on either side.
 */
function Surrounding({
  cluster,
  state,
  fields,
  timeField,
  anchor,
  zone,
  onBack,
  onOpenConsole,
}: {
  cluster: SearchCluster;
  state: DiscoverState;
  fields: ViewField[];
  timeField: string;
  anchor: Hit;
  zone: string;
  onBack: () => void;
  onOpenConsole: (text: string) => void;
}) {
  const [counts, setCounts] = useState({ newer: CONTEXT_STEP, older: CONTEXT_STEP });
  const [keepQuery, setKeepQuery] = useState(false);
  const [found, setFound] = useState<Hit | null>(null);
  const [around, setAround] = useState<{ newer: Hit[]; older: Hit[] } | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [open, setOpen] = useState<string | null>(null);
  const canKeep = !piped(state.mode) && state.query.trim() !== "";

  // The anchor again, by id on its own index, for its sort values.
  useEffect(() => {
    let live = true;
    searchHits(cluster.id, `/${encodeURIComponent(anchor._index)}/_search`, anchorBody(timeField, anchor._id)).then(
      (r) => {
        if (!live) return;
        if (r.hits[0]?.sort) setFound(r.hits[0]);
        else setErr("The document is gone, or has no value in the time field.");
      },
      (e) => live && setErr(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      live = false;
    };
  }, [cluster.id, anchor._index, anchor._id, timeField]);

  useEffect(() => {
    if (!found?.sort) return;
    let live = true;
    setLoading(true);
    const bodies = surroundingBodies(state, timeField, found.sort, counts, keepQuery && canKeep);
    const path = `/${encodeURIComponent(state.view)}/_search`;
    Promise.all([searchHits(cluster.id, path, bodies.newer), searchHits(cluster.id, path, bodies.older)]).then(
      ([newer, older]) => {
        if (!live) return;
        setAround({ newer: newer.hits, older: older.hits });
        setErr(null);
        setLoading(false);
      },
      (e) => {
        if (!live) return;
        setErr(e instanceof Error ? e.message : String(e));
        setLoading(false);
      },
    );
    return () => {
      live = false;
    };
    // `state` is read for its view and query only.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cluster.id, found, state.view, state.query, timeField, counts, keepQuery, canKeep]);

  const key = `${anchor._index}/${anchor._id}`;
  const hits = found && around ? [...[...around.newer].reverse(), found, ...around.older] : [];
  const more = (side: "newer" | "older") => setCounts((c) => ({ ...c, [side]: c[side] + CONTEXT_STEP }));
  return (
    <section aria-label="Surrounding documents" className="rise min-w-0 overflow-hidden rounded-lg border border-line bg-panel">
      <div className="flex flex-wrap items-center gap-3 border-b border-line px-5 py-2.5">
        <button type="button" onClick={onBack} className={btn}>
          <ArrowLeft className="size-3.5" strokeWidth={1.5} /> Documents
        </button>
        <span className="min-w-0 truncate text-[12px] text-muted">
          Around <span className="font-mono text-fg">{anchor._id}</span> by <span className="font-mono">{timeField}</span>, in every time
        </span>
        <label className={`ml-auto flex items-center gap-1.5 text-[12px] ${canKeep ? "text-muted" : "text-subtle opacity-60"}`} title={canKeep ? "Only documents the view's query matches" : "The view has no query to keep"}>
          <input type="checkbox" checked={keepQuery && canKeep} disabled={!canKeep} onChange={(e) => setKeepQuery(e.target.checked)} className="accent-[var(--color-accent)]" />
          Keep the query
        </label>
      </div>
      {err ? (
        <p role="alert" className="px-5 py-4 font-mono text-[12px] text-danger">
          {err}
        </p>
      ) : !around ? (
        <p className="px-5 py-6 text-[13px] text-subtle">
          <span className="shimmer">Finding the documents around it…</span>
        </p>
      ) : (
        <>
          <ContextMore side="newer" shown={around.newer.length} asked={counts.newer} loading={loading} onMore={() => more("newer")} />
          <Documents cluster={cluster} state={state} fields={fields} hits={hits} zone={zone} loading={loading} open={open} onOpen={setOpen} mark={key} onOpenConsole={onOpenConsole} />
          <ContextMore side="older" shown={around.older.length} asked={counts.older} loading={loading} onMore={() => more("older")} />
        </>
      )}
    </section>
  );
}

/** Above or below the surrounding documents: how many there are on that side, and five more. */
function ContextMore({ side, shown, asked, loading, onMore }: { side: "newer" | "older"; shown: number; asked: number; loading: boolean; onMore: () => void }) {
  const all = shown < asked;
  return (
    <div className={`flex items-center justify-between gap-3 px-5 py-2 text-[12px] text-subtle ${side === "newer" ? "border-b border-line" : "border-t border-line"}`}>
      <span className="font-mono tabular-nums">
        {formatCount(shown)} {side}
        {all ? ` (${side === "newer" ? "the newest" : "the oldest"})` : ""}
      </span>
      <button type="button" onClick={onMore} disabled={loading || all} className={btn}>
        <FoldVertical className="size-3.5" strokeWidth={1.5} /> {CONTEXT_STEP} more {side}
      </button>
    </div>
  );
}

/** The range presets alone, where no histogram is drawn (a piped query, or before the first answer). */
function RangeOnly({ state, zone, onRange }: { state: DiscoverState; zone: string; onRange: (r: DiscoverState["range"]) => void }) {
  const r = state.range;
  return (
    <div className="flex flex-wrap items-center gap-3">
      <div className="w-72 max-w-full">
        <Segmented value={"last" in r ? (r.last ?? "all") : "custom"} options={TIME_PRESETS.map((p) => ({ label: p.label, value: p.value ?? "all" }))} onChange={(v) => onRange({ last: v === "all" ? null : v })} />
      </div>
      {"gte" in r && (
        <button type="button" onClick={() => onRange({ last: null })} className={btn} title="Back to all time">
          {localTime(r.gte, zone).slice(0, 19)} – {localTime(r.lt, zone).slice(0, 19)} <X className="size-3" strokeWidth={1.5} />
        </button>
      )}
      <span className="ml-auto font-mono text-[11px] text-subtle">
        by {state.timeField} · {zone}
      </span>
    </div>
  );
}

/** The view's fields: a click shows a field's top values over a sample, with + and − to keep or
 * leave out a value; the field can be added to the table, or drawn over the whole range. */
function FieldList({
  cluster,
  state,
  fields,
  error,
  zone,
  chart,
  onFilter,
  onToggleColumn,
  onVisualize,
}: {
  cluster: SearchCluster;
  state: DiscoverState;
  fields: ViewField[] | null;
  error: string | null;
  zone: string;
  /** The field the chart draws. */
  chart: string | null;
  onFilter: (field: string, value: string | number | boolean, exclude: boolean) => void;
  onToggleColumn: (field: string) => void;
  onVisualize: (field: string) => void;
}) {
  const [filter, setFilter] = useState("");
  const [open, setOpen] = useState<string | null>(null);
  const f = filter.trim().toLowerCase();
  const list = (fields ?? []).filter((x) => !f || x.name.toLowerCase().includes(f));
  return (
    <aside className="min-w-0 rounded-lg border border-line bg-panel" aria-label="Fields">
      <div className="border-b border-line p-2">
        <input value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="Filter fields" spellCheck={false} className={`${input} w-full font-mono text-[11px]`} />
      </div>
      {error && <p className="px-3 py-2 text-[12px] text-danger">{error}</p>}
      {!fields ? (
        <p className="px-3 py-3 text-[12px] text-subtle">
          <span className="shimmer">Reading fields…</span>
        </p>
      ) : (
        <ul className="max-h-[36rem] overflow-y-auto py-1">
          {list.map((x) => (
            <li key={x.name}>
              <div className="group flex items-center gap-1.5 px-2">
                <button type="button" onClick={() => setOpen(open === x.name ? null : x.name)} aria-expanded={open === x.name} aria-label={`${x.name} (${x.type})`} className="flex min-w-0 flex-1 items-center gap-2 rounded px-1 py-1 text-left transition hover:bg-panel-2">
                  <span className="w-14 shrink-0 truncate font-mono text-[10px] text-subtle" title={x.conflict ? `${x.type} (and other types in other indices)` : x.type}>
                    {x.type}
                    {x.conflict ? "*" : ""}
                  </span>
                  <span className="truncate font-mono text-[12px]" title={x.name}>
                    {x.name}
                  </span>
                </button>
                <button
                  type="button"
                  onClick={() => onToggleColumn(x.name)}
                  className={`rounded p-1 transition hover:text-fg ${state.columns.includes(x.name) ? "text-accent" : "text-subtle opacity-0 group-hover:opacity-100 focus:opacity-100"}`}
                  aria-label={state.columns.includes(x.name) ? `Remove column ${x.name}` : `Add column ${x.name}`}
                  title={state.columns.includes(x.name) ? "Remove the column" : "Add a column"}
                >
                  <Columns3 className="size-3.5" strokeWidth={1.5} />
                </button>
              </div>
              {open === x.name && <TopValues cluster={cluster} state={state} fields={fields} field={x.name} zone={zone} charted={chart === x.name} onFilter={onFilter} onVisualize={onVisualize} />}
            </li>
          ))}
          {list.length === 0 && <li className="px-3 py-2 text-[12px] text-subtle">No field matches.</li>}
        </ul>
      )}
    </aside>
  );
}

/** One field's top 5 values over a sample of the view's matching documents. In ES|QL and PPL
 * modes the sample is of the range alone (their query isn't a filter). */
function TopValues({
  cluster,
  state,
  fields,
  field,
  zone,
  charted,
  onFilter,
  onVisualize,
}: {
  cluster: SearchCluster;
  state: DiscoverState;
  fields: ViewField[];
  field: string;
  zone: string;
  charted: boolean;
  onFilter: (field: string, value: string | number | boolean, exclude: boolean) => void;
  onVisualize: (field: string) => void;
}) {
  const on = valuesField(fields, field);
  const chartable = chartOf(fields, field) !== null;
  const isDate = fields.some((f) => f.name === field && (f.type === "date" || f.type === "date_nanos"));
  const [values, setValues] = useState<ReturnType<typeof topValues> | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const query = piped(state.mode) ? "" : state.query;
  useEffect(() => {
    if (!on) return;
    let live = true;
    esJson(cluster.id, "POST", `/${encodeURIComponent(state.view)}/_search`, sampleBody({ query, range: state.range, timeField: state.timeField }, on)).then(
      (r) => live && setValues(topValues(r)),
      (e) => live && setErr(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      live = false;
    };
  }, [cluster.id, state.view, state.range, state.timeField, query, on]);
  if (!on) return <p className="px-4 pb-2 text-[11px] text-subtle">Not aggregatable: no top values.</p>;
  if (err) return <p className="px-4 pb-2 text-[11px] text-danger">{err}</p>;
  if (!values)
    return (
      <p className="px-4 pb-2 text-[11px] text-subtle">
        <span className="shimmer">Sampling…</span>
      </p>
    );
  return (
    <div className="rise px-3 pb-2">
      <p className="flex items-center gap-2 px-1 pb-1 text-[10px] text-subtle">
        <span className="flex-1">
          Top values in {formatCount(values.sampled)} sampled documents{isDate ? ` (${zone})` : ""}
        </span>
        {chartable && (
          <button type="button" onClick={() => onVisualize(field)} className={`flex items-center gap-1 rounded px-1 py-0.5 transition hover:text-fg ${charted ? "text-accent" : ""}`} aria-pressed={charted} title="Draw it over the whole range">
            <ChartColumn className="size-3" strokeWidth={1.5} /> Visualize
          </button>
        )}
      </p>
      {values.values.length === 0 && <p className="px-1 text-[11px] text-subtle">No values.</p>}
      <ul>
        {values.values.map((v) => (
          <li key={String(v.value)} className="flex items-center gap-1 px-1 py-0.5 text-[11px]">
            <span className="min-w-0 flex-1 truncate font-mono" title={String(v.value)}>
              {isDate ? localTime(v.value, zone) : String(v.value)}
            </span>
            <span className="font-mono text-subtle tabular-nums">{values.sampled ? `${Math.round((100 * v.count) / values.sampled)}%` : formatCount(v.count)}</span>
            <button type="button" onClick={() => onFilter(field, v.value, false)} className="rounded p-0.5 text-subtle transition hover:text-fg" aria-label={`Filter for ${field}: ${v.value}`}>
              <Plus className="size-3" strokeWidth={1.5} />
            </button>
            <button type="button" onClick={() => onFilter(field, v.value, true)} className="rounded p-0.5 text-subtle transition hover:text-fg" aria-label={`Filter out ${field}: ${v.value}`}>
              <Minus className="size-3" strokeWidth={1.5} />
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** The documents of a KQL or DQL view: its time and the chosen columns (or the document itself),
 * newest first or by a column; a row opens the document's JSON. */
function Documents({
  cluster,
  state,
  fields,
  hits,
  zone,
  loading,
  open,
  onOpen,
  mark,
  onSort,
  onRemoveColumn,
  onSurround,
  onOpenConsole,
}: {
  cluster: SearchCluster;
  state: DiscoverState;
  fields: ViewField[];
  hits: Hit[];
  zone: string;
  loading: boolean;
  open: string | null;
  onOpen: (key: string | null) => void;
  /** The row shown as the one the others surround. */
  mark?: string;
  /** Without it, the columns don't sort (the surrounding documents are in time order). */
  onSort?: (column: string, field: string) => void;
  onRemoveColumn?: (column: string) => void;
  onSurround?: (hit: Hit) => void;
  onOpenConsole: (text: string) => void;
}) {
  const cells = useMemo(() => hits.map((h) => sourceCells(h.source)), [hits]);
  if (hits.length === 0) return <p className="px-5 py-6 text-[13px] text-muted">No documents match in this range.</p>;
  const columns = state.columns;
  const tf = state.timeField;
  const span = (tf ? 1 : 0) + Math.max(1, columns.length);
  return (
    <div className={`overflow-x-auto ${loading ? "opacity-60 transition-opacity" : "transition-opacity"}`}>
      <table className="w-full border-collapse font-mono text-[11px]">
        <thead>
          <tr className="border-b border-line text-left text-subtle">
            {tf && (
              <th className="w-48 px-4 py-2 font-normal whitespace-nowrap">
                {tf} <span className="text-subtle/70">{zone}</span>
              </th>
            )}
            {columns.length === 0 && <th className="px-3 py-2 font-normal">document</th>}
            {columns.map((c) => {
              const field = onSort ? sortField(c, fields) : null;
              const active = state.sort?.column === c ? state.sort.order : null;
              return (
                <th key={c} className="max-w-56 px-3 py-2 font-normal" aria-sort={active === "asc" ? "ascending" : active === "desc" ? "descending" : undefined}>
                  <span className="flex items-center gap-1">
                    {field ? (
                      <button type="button" onClick={() => onSort?.(c, field)} className={`truncate transition hover:text-fg ${active ? "text-accent" : ""}`} title={`Sort by ${field}`}>
                        {c}
                        {active === "asc" ? " ↑" : active === "desc" ? " ↓" : ""}
                      </button>
                    ) : (
                      <span className="truncate">{c}</span>
                    )}
                    {onRemoveColumn && (
                      <button type="button" onClick={() => onRemoveColumn(c)} className="rounded text-subtle transition hover:text-fg" aria-label={`Remove column ${c}`}>
                        <X className="size-3" strokeWidth={1.5} />
                      </button>
                    )}
                  </span>
                </th>
              );
            })}
          </tr>
        </thead>
        <tbody>
          {hits.map((h, i) => {
            const key = `${h._index}/${h._id}`;
            const row = cells[i];
            const isOpen = open === key;
            return (
              <Fragment key={key}>
                <tr
                  onClick={() => onOpen(isOpen ? null : key)}
                  aria-expanded={isOpen}
                  aria-current={mark === key ? "true" : undefined}
                  className={`cursor-pointer border-b border-line align-top transition hover:bg-panel-2/60 ${mark === key ? "bg-panel-2" : ""}`}
                >
                  {tf && <td className="px-4 py-1.5 whitespace-nowrap text-fg">{h.fields?.[tf]?.[0] != null ? localTime(h.fields[tf][0], zone) : cellText(row.get(tf))}</td>}
                  {columns.length === 0 && (
                    <td className="max-w-0 truncate px-3 py-1.5 text-muted" title={h.source}>
                      {[...row.entries()]
                        .filter(([k]) => k !== tf)
                        .slice(0, 12)
                        .map(([k, v]) => `${k}: ${cellText(v, 60)}`)
                        .join("  ")}
                    </td>
                  )}
                  {columns.map((c) => {
                    const t = cellText(row.get(c));
                    return (
                      <td key={c} className="max-w-56 truncate px-3 py-1.5 text-muted" title={t}>
                        {t}
                      </td>
                    );
                  })}
                </tr>
                {isOpen && (
                  <tr className="border-b border-line">
                    <td colSpan={span} className="p-0">
                      {onSurround && (
                        <div className="flex justify-end px-4 pt-2">
                          <button type="button" onClick={() => onSurround(h)} className={btn} title="The documents just before and after this one in time">
                            <FoldVertical className="size-3.5" strokeWidth={1.5} /> Surrounding documents
                          </button>
                        </div>
                      )}
                      <JsonDoc cluster={cluster} path={`/${encodeURIComponent(h._index)}/_doc/${encodeURIComponent(h._id)}`} onOpenConsole={onOpenConsole} />
                    </td>
                  </tr>
                )}
              </Fragment>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

/** An ES|QL or PPL answer as a table. */
function PipedTable({ result }: { result: { columns: string[]; rows: unknown[][] } }) {
  if (result.rows.length === 0) return <p className="px-5 py-6 text-[13px] text-muted">No rows.</p>;
  return (
    <div className="overflow-x-auto">
      <table className="w-full border-collapse font-mono text-[11px]">
        <thead>
          <tr className="border-b border-line text-left text-subtle">
            {result.columns.map((c) => (
              <th key={c} className="px-3 py-2 font-normal">
                {c}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {result.rows.map((r, i) => (
            // Rows have no ids: their place is their key.
            <tr key={i} className="border-b border-line">
              {r.map((v, j) => (
                <td key={j} className="max-w-56 truncate px-3 py-1.5 text-muted" title={cellText(v)}>
                  {cellText(v)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      <p className="px-5 py-2.5 text-[12px] text-subtle">{formatCount(result.rows.length)} rows</p>
    </div>
  );
}
