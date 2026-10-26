"use client";

import { ArrowDown, ArrowUp, ChevronLeft, ChevronRight, Columns3, Pencil, Plus, RefreshCw, Search, SquareTerminal, Trash2, X } from "lucide-react";
import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { formatBytes } from "@/lib/roll-api";
import { esJson, esRaw, formatCount, indexPath, type SearchCluster, type SearchIndex } from "@/lib/search-api";
import { mappingFields } from "@/lib/search-console";
import {
  cellText,
  columnsOf,
  deleteRequest,
  formatJson,
  hitSources,
  nextSort,
  parseDocument,
  sortClause,
  sortField,
  sourceCells,
  toggleColumn,
  visibleColumns,
  writeRequest,
  type MappedField,
  type TableSort,
} from "@/lib/search-docs";
import { Segmented } from "../redis/shared";
import { btn, Confirm, CopyButton, input, Panel, primary, useCopy } from "../ui";
import { JsonText } from "./json-view";

/** Documents per page of the browser. */
const PAGE = 20;

type View = "documents" | "mapping" | "settings";

interface Hit {
  _index: string;
  _id: string;
  _score: number | null;
  _routing?: string;
  /** The hit's `_source` exactly as the response holds it (never re-encoded). */
  source?: string;
}

interface SearchResponse {
  took: number;
  hits: { total?: { value: number; relation: "eq" | "gte" }; hits: Omit<Hit, "source">[] };
}

interface SearchResult {
  took: number;
  total?: { value: number; relation: "eq" | "gte" };
  hits: Hit[];
}

/** The request the documents view sends, also offered as a console request. */
export function documentsQuery(q: string, from: number, sort: TableSort | null = null) {
  const query = q.trim() ? { query_string: { query: q.trim() } } : { match_all: {} };
  const s = sortClause(sort);
  return s ? { query, from, size: PAGE, track_total_hits: true, sort: s } : { query, from, size: PAGE, track_total_hits: true };
}

/** Columns picked for an index's table, kept in the browser per cluster and index. */
function columnsKey(cluster: string, index: string) {
  return `roll.search.columns.${cluster}/${index}`;
}

function loadColumns(key: string): string[] | null {
  try {
    const v = JSON.parse(window.localStorage.getItem(key) ?? "null");
    return Array.isArray(v) && v.every((c) => typeof c === "string") ? v : null;
  } catch {
    return null;
  }
}

function saveColumns(key: string, cols: string[] | null) {
  try {
    if (cols && cols.length) window.localStorage.setItem(key, JSON.stringify(cols));
    else window.localStorage.removeItem(key);
  } catch {
    // Storage full or disabled: the pick lasts for this view only.
  }
}

/** The column picker: every path of the page and every mapped leaf field, the shown ones first. */
function ColumnPicker({ shown, available, onChange, onReset }: { shown: string[]; available: string[]; onChange: (cols: string[]) => void; onReset: () => void }) {
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState("");
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", esc);
    };
  }, [open]);
  const all = [...shown, ...available.filter((c) => !shown.includes(c))];
  const f = filter.trim().toLowerCase();
  const list = f ? all.filter((c) => c.toLowerCase().includes(f)) : all;
  return (
    <div ref={ref} className="relative">
      <button type="button" onClick={() => setOpen((o) => !o)} className={btn} aria-expanded={open} title="Choose the table's columns">
        <Columns3 className="size-3.5" strokeWidth={1.5} /> Columns
      </button>
      {open && (
        <div className="rise absolute right-0 z-20 mt-1 w-64 rounded-lg border border-line bg-panel p-2 text-[12px]">
          <input
            autoFocus
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder="Filter fields"
            spellCheck={false}
            className={`${input} mb-1.5 w-full font-mono text-[11px]`}
          />
          <ul className="max-h-64 overflow-y-auto">
            {list.map((c) => (
              <li key={c}>
                <label className="flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 transition hover:bg-panel-2">
                  <input type="checkbox" checked={shown.includes(c)} onChange={() => onChange(toggleColumn(shown, c))} className="accent-accent" />
                  <span className="truncate font-mono text-[11px]" title={c}>
                    {c}
                  </span>
                </label>
              </li>
            ))}
            {list.length === 0 && <li className="px-1.5 py-1 text-subtle">No field matches.</li>}
          </ul>
          <button type="button" onClick={onReset} className="mt-1.5 w-full rounded px-1.5 py-1 text-left text-muted transition hover:bg-panel-2 hover:text-fg">
            Reset to the most common fields
          </button>
        </div>
      )}
    </div>
  );
}

/** One index: its documents (paged `_search`), mapping and settings, refresh and delete. */
export function IndexBrowser({
  cluster,
  index,
  onClose,
  onDeleted,
  onChanged,
  onOpenConsole,
}: {
  cluster: SearchCluster;
  index: SearchIndex;
  onClose: () => void;
  onDeleted: () => void;
  /** Documents were written: the index's figures changed. */
  onChanged: () => void;
  onOpenConsole: (text: string) => void;
}) {
  const [view, setView] = useState<View>("documents");
  const [deleting, setDeleting] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);
  const [generation, setGeneration] = useState(0);
  const path = indexPath(index.index);

  const act = async (name: string, f: () => Promise<unknown>, done: string) => {
    setBusy(name);
    try {
      await f();
      setNote({ ok: true, text: done });
      return true;
    } catch (e) {
      setNote({ ok: false, text: e instanceof Error ? e.message : String(e) });
      return false;
    } finally {
      setBusy(null);
    }
  };

  return (
    <Panel
      title={
        <span className="flex min-w-0 items-center gap-2">
          <span className="truncate font-mono text-[13px]">{index.index}</span>
          <span className="shrink-0 font-mono text-[11px] text-subtle">
            {formatCount(index.docs)} docs · {formatBytes(index.size_bytes)}
          </span>
        </span>
      }
      aside={
        <span className="flex shrink-0 items-center gap-2">
          <button
            onClick={async () => {
              if (await act("refresh", () => esJson(cluster.id, "POST", `${path}/_refresh`), "Refreshed: recent writes are searchable.")) {
                setGeneration((g) => g + 1);
                onChanged();
              }
            }}
            disabled={busy !== null}
            className={btn}
            title="POST /<index>/_refresh"
          >
            <RefreshCw className={`size-3.5 ${busy === "refresh" ? "animate-spin motion-reduce:animate-none" : ""}`} /> Refresh
          </button>
          <button onClick={() => setDeleting(true)} disabled={busy !== null} className={`${btn} hover:border-danger/50 hover:text-danger`}>
            <Trash2 className="size-3.5" /> Delete
          </button>
          <button onClick={onClose} className={btn} aria-label="Close index">
            <X className="size-3.5" />
          </button>
        </span>
      }
      className="rise"
    >
      {deleting && (
        <div className="border-b border-line px-5 py-4">
          <Confirm
            word={index.index}
            action="Delete index"
            busy={busy === "delete"}
            onCancel={() => setDeleting(false)}
            onConfirm={async () => {
              if (await act("delete", () => esJson(cluster.id, "DELETE", path), `Deleted ${index.index}.`)) onDeleted();
              setDeleting(false);
            }}
          >
            Deletes the index and its documents. Branches made before now keep their copy.
          </Confirm>
        </div>
      )}
      {note && (
        <p className={`rise border-b border-line px-5 py-2 text-[12px] ${note.ok ? "text-muted" : "text-danger"}`}>{note.text}</p>
      )}
      <div className="border-b border-line px-5 py-3">
        <div className="w-72 max-w-full">
          <Segmented
            value={view}
            options={[
              { label: "documents", value: "documents" },
              { label: "mapping", value: "mapping" },
              { label: "settings", value: "settings" },
            ]}
            onChange={setView}
          />
        </div>
      </div>
      {view === "documents" && <Documents key={generation} cluster={cluster} index={index.index} onChanged={onChanged} onOpenConsole={onOpenConsole} />}
      {view === "mapping" && <JsonDoc cluster={cluster} path={`${path}/_mapping`} onOpenConsole={onOpenConsole} />}
      {view === "settings" && <JsonDoc cluster={cluster} path={`${path}/_settings`} onOpenConsole={onOpenConsole} />}
    </Panel>
  );
}

/** Pretty JSON of one GET (mapping, settings). */
function JsonDoc({ cluster, path, onOpenConsole }: { cluster: SearchCluster; path: string; onOpenConsole: (text: string) => void }) {
  const [text, setText] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const { copied, copy } = useCopy();

  useEffect(() => {
    let alive = true;
    setText(null);
    setErr(null);
    esRaw(cluster.id, "GET", path).then(
      (r) => alive && setText(formatJson(r.text)),
      (e) => alive && setErr(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [cluster.id, path]);

  return (
    <div className="relative">
      <div className="absolute top-3 right-4 flex gap-1">
        {text && <CopyButton id="doc" text={text} copied={copied} onCopy={copy} />}
        <button onClick={() => onOpenConsole(`GET ${path}`)} className="rounded-md p-1 text-subtle transition hover:text-fg" title="Open in console">
          <SquareTerminal className="size-3.5" />
        </button>
      </div>
      {err ? (
        <p className="px-5 py-4 text-[13px] text-danger">{err}</p>
      ) : text === null ? (
        <p className="px-5 py-4 text-[13px] text-subtle">
          <span className="shimmer">Loading…</span>
        </p>
      ) : (
        <pre className="rise max-h-[32rem] overflow-auto px-5 py-4 font-mono text-[12px] leading-relaxed">
          <JsonText text={text} />
        </pre>
      )}
    </div>
  );
}

/** One line of a document's source, for the collapsed row. */
function preview(source: string | undefined) {
  if (source === undefined) return "(no _source)";
  const s = formatJson(source, 0);
  return s.length > 240 ? `${s.slice(0, 240)}…` : s;
}

type Layout = "rows" | "table";

function Documents({
  cluster,
  index,
  onChanged,
  onOpenConsole,
}: {
  cluster: SearchCluster;
  index: string;
  onChanged: () => void;
  onOpenConsole: (text: string) => void;
}) {
  const [q, setQ] = useState("");
  const [applied, setApplied] = useState("");
  const [from, setFrom] = useState(0);
  const [result, setResult] = useState<SearchResult | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [open, setOpen] = useState<string | null>(null);
  const [layout, setLayout] = useState<Layout>("rows");
  const [adding, setAdding] = useState(false);
  const [sort, setSort] = useState<TableSort | null>(null);
  const [fields, setFields] = useState<MappedField[]>([]);
  const colKey = columnsKey(cluster.id, index);
  const [picked, setPicked] = useState<string[] | null>(null);

  // The pick is the browser's, so read it after mounting (the server render has none).
  useEffect(() => {
    setPicked(loadColumns(colKey));
  }, [colKey]);

  // The mapping says which columns can sort (and by which field), and offers fields the page lacks.
  useEffect(() => {
    if (layout !== "table") return;
    let live = true;
    esJson(cluster.id, "GET", `${indexPath(index)}/_mapping`).then(
      (m) => live && setFields(mappingFields(m)),
      () => live && setFields([]),
    );
    return () => {
      live = false;
    };
  }, [cluster.id, index, layout]);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const r = await esRaw<SearchResponse>(cluster.id, "POST", `${indexPath(index)}/_search`, documentsQuery(applied, from, sort));
      const sources = hitSources(r.text);
      setResult({ took: r.value.took, total: r.value.hits.total, hits: r.value.hits.hits.map((h, i) => ({ ...h, source: sources[i] })) });
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      setResult(null);
    } finally {
      setLoading(false);
    }
  }, [cluster.id, index, applied, from, sort]);

  useEffect(() => {
    load();
  }, [load]);

  const total = result?.total;
  const hits = result?.hits ?? [];
  const last = from + hits.length;
  const consoleText = `POST ${indexPath(index)}/_search\n${JSON.stringify(documentsQuery(applied, from, sort), null, 2)}`;
  const cells = useMemo(() => (layout === "table" ? hits.map((h) => sourceCells(h.source)) : []), [layout, hits]);
  const pagePaths = useMemo(() => columnsOf(cells.map((c) => c.keys()), Infinity), [cells]);
  const columns = visibleColumns(picked, pagePaths);
  // Mapped leaves the page has no value for yet. Objects aren't columns, and neither are
  // multi-fields (`title.keyword`): they index the parent's value, `_source` has none of their own.
  const available = useMemo(() => {
    const leafTypes = new Map(fields.map((f) => [f.name, f.type]));
    const isMultiField = (name: string) => {
      for (let i = name.lastIndexOf("."); i > 0; i = name.lastIndexOf(".", i - 1)) {
        const t = leafTypes.get(name.slice(0, i));
        if (t && t !== "object" && t !== "nested") return true;
      }
      return false;
    };
    const mapped = fields.filter((f) => f.type !== "object" && f.type !== "nested" && !pagePaths.includes(f.name) && !isMultiField(f.name)).map((f) => f.name);
    return [...pagePaths, ...mapped];
  }, [pagePaths, fields]);
  const pick = (cols: string[] | null) => {
    setPicked(cols && cols.length ? cols : null);
    saveColumns(colKey, cols);
  };
  const sortBy = (column: string, field: string) => {
    setFrom(0);
    setSort((s) => nextSort(s, column, field));
  };

  const expanded = (h: Hit) => (
    <SourceView
      cluster={cluster}
      hit={h}
      onOpenConsole={onOpenConsole}
      onChanged={() => {
        setOpen(null);
        load();
        onChanged();
      }}
    />
  );

  return (
    <div>
      <form
        className="flex flex-wrap items-center gap-2 border-b border-line px-5 py-3"
        onSubmit={(e) => {
          e.preventDefault();
          setFrom(0);
          setApplied(q);
          if (q === applied && from === 0) load();
        }}
      >
        <div className="relative min-w-0 flex-1">
          <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-subtle" />
          <input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="Query string, e.g. title:shoes AND price:<100 (empty: all documents)"
            spellCheck={false}
            className={`${input} w-full pl-8 font-mono text-[12px]`}
          />
        </div>
        <button type="submit" disabled={loading} className={btn}>
          Search
        </button>
        <div className="w-32">
          <Segmented
            value={layout}
            options={[
              { label: "rows", value: "rows" },
              { label: "table", value: "table" },
            ]}
            onChange={setLayout}
          />
        </div>
        {layout === "table" && <ColumnPicker shown={columns} available={available} onChange={pick} onReset={() => pick(null)} />}
        <button type="button" onClick={() => setAdding((a) => !a)} className={btn} title="Index a new document">
          <Plus className="size-3.5" /> Add
        </button>
        <button type="button" onClick={() => onOpenConsole(consoleText)} className={btn} title="Open this search in the console">
          <SquareTerminal className="size-3.5" /> Console
        </button>
      </form>
      {adding && (
        <DocEditor
          title="New document"
          initial={"{\n  \n}"}
          withId
          onCancel={() => setAdding(false)}
          onSave={async (id, doc) => {
            const r = writeRequest(index, id, { create: true });
            await esJson(cluster.id, r.method, r.path, doc);
            setAdding(false);
            load();
            onChanged();
          }}
        />
      )}
      {err ? (
        <p className="px-5 py-4 text-[13px] text-danger">{err}</p>
      ) : !result ? (
        <p className="px-5 py-6 text-[13px] text-subtle">
          <span className="shimmer">Searching…</span>
        </p>
      ) : hits.length === 0 ? (
        <p className="px-5 py-6 text-[13px] text-muted">
          {applied ? "No documents match." : "No documents yet (or none refreshed: writes become searchable after a refresh)."}
        </p>
      ) : layout === "table" ? (
        <div className={`overflow-x-auto ${loading ? "opacity-60 transition-opacity" : "transition-opacity"}`}>
          <table className="w-full border-collapse font-mono text-[11px]">
            <thead>
              <tr className="border-b border-line text-left text-subtle">
                <th className="px-5 py-2 font-normal">_id</th>
                {columns.map((c) => {
                  const field = sortField(c, fields);
                  const active = sort?.column === c ? sort.order : null;
                  return (
                    <th key={c} className="max-w-56 px-3 py-2 font-normal" aria-sort={active === "asc" ? "ascending" : active === "desc" ? "descending" : undefined}>
                      {field ? (
                        <button
                          type="button"
                          onClick={() => sortBy(c, field)}
                          className={`flex max-w-full items-center gap-1 truncate transition hover:text-fg ${active ? "text-accent" : ""}`}
                          title={active === "desc" ? `${c}: sorted descending (click for relevance)` : active === "asc" ? `${c}: sorted ascending` : `Sort by ${field}`}
                        >
                          <span className="truncate">{c}</span>
                          {active === "asc" && <ArrowUp className="size-3 shrink-0" strokeWidth={1.5} />}
                          {active === "desc" && <ArrowDown className="size-3 shrink-0" strokeWidth={1.5} />}
                        </button>
                      ) : (
                        <span className="block truncate" title={fields.length ? `${c} (not sortable)` : c}>
                          {c}
                        </span>
                      )}
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
                      onClick={() => setOpen(isOpen ? null : key)}
                      aria-expanded={isOpen}
                      className="cursor-pointer border-b border-line transition hover:bg-panel-2/60"
                    >
                      <td className="max-w-40 truncate px-5 py-1.5 text-fg" title={h._id}>
                        {h._id}
                      </td>
                      {columns.map((c) => {
                        const t = cellText(row?.get(c));
                        return (
                          <td key={c} className="max-w-56 truncate px-3 py-1.5 text-muted" title={t}>
                            {t}
                          </td>
                        );
                      })}
                    </tr>
                    {isOpen && (
                      <tr className="border-b border-line">
                        <td colSpan={columns.length + 1} className="p-0">
                          {expanded(h)}
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : (
        <ul className={loading ? "opacity-60 transition-opacity" : "transition-opacity"}>
          {hits.map((h) => {
            const key = `${h._index}/${h._id}`;
            const isOpen = open === key;
            return (
              <li key={key} className="border-b border-line last:border-0">
                <button
                  onClick={() => setOpen(isOpen ? null : key)}
                  aria-expanded={isOpen}
                  className="flex w-full items-baseline gap-3 px-5 py-2 text-left transition hover:bg-panel-2/60"
                >
                  <span className="w-40 shrink-0 truncate font-mono text-[12px]" title={h._id}>
                    {h._id}
                  </span>
                  <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-subtle">{preview(h.source)}</span>
                  {h._score != null && applied && <span className="shrink-0 font-mono text-[11px] text-subtle tabular-nums">{h._score.toFixed(3)}</span>}
                </button>
                {isOpen && expanded(h)}
              </li>
            );
          })}
        </ul>
      )}
      {result && total && (
        <div className="flex items-center justify-between gap-3 border-t border-line px-5 py-2.5 text-[12px] text-subtle">
          <span className="font-mono tabular-nums">
            {hits.length ? `${formatCount(from + 1)}–${formatCount(last)}` : "0"} of {total.relation === "gte" ? "≥" : ""}
            {formatCount(total.value)} · {result.took} ms
          </span>
          <span className="flex gap-1">
            <button onClick={() => setFrom(Math.max(0, from - PAGE))} disabled={loading || from === 0} className={btn} aria-label="Previous page">
              <ChevronLeft className="size-3.5" />
            </button>
            <button
              onClick={() => setFrom(from + PAGE)}
              disabled={loading || hits.length < PAGE || last >= total.value || last >= 10_000}
              className={btn}
              aria-label="Next page"
            >
              <ChevronRight className="size-3.5" />
            </button>
          </span>
        </div>
      )}
    </div>
  );
}

/** A JSON document typed by hand (new, or an edit of an existing one). */
function DocEditor({
  title,
  initial,
  withId,
  onSave,
  onCancel,
}: {
  title: string;
  initial: string;
  withId?: boolean;
  /** `doc` is the JSON text as typed: sent as is. */
  onSave: (id: string | null, doc: string) => Promise<void>;
  onCancel: () => void;
}) {
  const [text, setText] = useState(initial);
  const [id, setId] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const save = async () => {
    let doc: string;
    try {
      doc = parseDocument(text);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      await onSave(withId ? id.trim() || null : null, doc);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="rise space-y-2 border-b border-line bg-bg/40 px-5 py-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-[12px] text-muted">{title}</span>
        {withId && (
          <input
            value={id}
            onChange={(e) => setId(e.target.value)}
            placeholder="_id (empty: generated)"
            spellCheck={false}
            className={`${input} w-56 font-mono text-[12px]`}
          />
        )}
      </div>
      <textarea
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
            e.preventDefault();
            save();
          } else if (e.key === "Escape") {
            onCancel();
          }
        }}
        spellCheck={false}
        autoFocus
        aria-label="Document JSON"
        className="min-h-40 w-full resize-y rounded-md border border-line bg-transparent px-3 py-2 font-mono text-[12px] leading-relaxed outline-none focus:border-line-2"
      />
      {err && <p className="text-[12px] text-danger">{err}</p>}
      <div className="flex gap-2">
        <button onClick={save} disabled={busy} className={primary}>
          {busy ? <span className="shimmer">Saving…</span> : "Save"}
        </button>
        <button onClick={onCancel} disabled={busy} className={btn}>
          Cancel
        </button>
      </div>
    </div>
  );
}

function SourceView({
  cluster,
  hit,
  onOpenConsole,
  onChanged,
}: {
  cluster: SearchCluster;
  hit: Hit;
  onOpenConsole: (text: string) => void;
  onChanged: () => void;
}) {
  const { copied, copy } = useCopy();
  const [mode, setMode] = useState<"view" | "edit" | "delete">("view");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const text = hit.source === undefined ? "null" : formatJson(hit.source);
  const docPath = `${indexPath(hit._index)}/_doc/${encodeURIComponent(hit._id)}${hit._routing ? `?routing=${encodeURIComponent(hit._routing)}` : ""}`;

  if (mode === "edit") {
    return (
      <DocEditor
        title={`Edit ${hit._id}`}
        initial={text}
        onCancel={() => setMode("view")}
        onSave={async (_, doc) => {
          const r = writeRequest(hit._index, hit._id, { routing: hit._routing });
          await esJson(cluster.id, r.method, r.path, doc);
          onChanged();
        }}
      />
    );
  }

  return (
    <div className="rise relative border-t border-line bg-bg/40">
      <div className="absolute top-2 right-4 flex items-center gap-1">
        <CopyButton id={`src-${hit._id}`} text={text} copied={copied} onCopy={copy} />
        {hit.source !== undefined && (
          <button onClick={() => setMode("edit")} className="rounded-md p-1 text-subtle transition hover:text-fg" title="Edit document">
            <Pencil className="size-3.5" />
          </button>
        )}
        <button onClick={() => setMode("delete")} className="rounded-md p-1 text-subtle transition hover:text-danger" title="Delete document">
          <Trash2 className="size-3.5" />
        </button>
        <button onClick={() => onOpenConsole(`PUT ${docPath}\n${text}`)} className="rounded-md p-1 text-subtle transition hover:text-fg" title="Edit in console">
          <SquareTerminal className="size-3.5" />
        </button>
      </div>
      {mode === "delete" && (
        <div className="rise flex flex-wrap items-center gap-2 border-b border-line px-5 py-2 text-[12px]">
          <span className="text-muted">
            Delete <span className="font-mono text-fg">{hit._id}</span>?
          </span>
          <button
            disabled={busy}
            className={`${btn} hover:border-danger/50 hover:text-danger`}
            onClick={async () => {
              setBusy(true);
              setErr(null);
              try {
                const r = deleteRequest(hit._index, hit._id, hit._routing);
                await esJson(cluster.id, r.method, r.path);
                onChanged();
              } catch (e) {
                setErr(e instanceof Error ? e.message : String(e));
                setBusy(false);
              }
            }}
          >
            {busy ? <span className="shimmer">Deleting…</span> : "Delete"}
          </button>
          <button onClick={() => setMode("view")} disabled={busy} className={btn}>
            Cancel
          </button>
          {err && <span className="text-danger">{err}</span>}
        </div>
      )}
      <pre className="max-h-96 overflow-auto px-5 py-3 pr-32 font-mono text-[12px] leading-relaxed">
        <JsonText text={text} />
      </pre>
    </div>
  );
}
