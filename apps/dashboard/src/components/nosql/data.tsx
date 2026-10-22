"use client";

import { HorizonMark } from "@roll/brand";
import { ChevronLeft, ChevronRight, KeyRound, RefreshCw, X } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { isCqlError, nosqlApi, type CqlRows, type NosqlDb, type SchemaTable } from "@/lib/nosql-api";
import { selectAll } from "@/lib/nosql-cql";
import { btn, CopyButton, EmptyPanel, input, Panel, useCopy } from "../ui";
import type { SchemaState } from "./database";

const PAGE_SIZES = [25, 50, 100, 500];

interface Page {
  rows: CqlRows;
  /** Page states that led here (the first page has none). */
  index: number;
  ms: number;
}

/** Table browser: `SELECT *` pages through the console route with the server's paging state. */
export function DataBrowser({
  db,
  schema: st,
  target,
  onTarget,
}: {
  db: NosqlDb;
  schema: SchemaState;
  target: { ks: string; table: string } | null;
  onTarget: (t: { ks: string; table: string }) => void;
}) {
  const { schema, error: schemaError } = st;
  const [showSystem, setShowSystem] = useState(false);
  const [pageSize, setPageSize] = useState(50);
  const [states, setStates] = useState<(string | null)[]>([null]);
  const [page, setPage] = useState<Page | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [open, setOpen] = useState<number | null>(null);

  const keyspaces = (schema?.keyspaces ?? []).filter((k) => showSystem || !k.system);
  const first = keyspaces.find((k) => k.tables.length);
  const sel = target ?? (first ? { ks: first.name, table: first.tables[0].name } : null);
  const ks = schema?.keyspaces.find((k) => k.name === sel?.ks);
  const table: SchemaTable | undefined = ks?.tables.find((t) => t.name === sel?.table) ?? ks?.views.find((v) => v.name === sel?.table);

  const load = useCallback(
    async (index: number, pageState: string | null) => {
      if (!sel) return;
      setLoading(true);
      setOpen(null);
      const t0 = performance.now();
      try {
        const r = await nosqlApi.cql(db.id, {
          query: selectAll(sel.ks, sel.table),
          page_size: pageSize,
          ...(pageState ? { page_state: pageState } : {}),
        });
        if (isCqlError(r)) {
          setError(`${r.code_name}: ${r.error}`);
          setPage(null);
        } else if (r.kind === "rows") {
          setError(null);
          setPage({ rows: r, index, ms: r.elapsed_ms ?? performance.now() - t0 });
        }
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setLoading(false);
      }
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [db.id, sel?.ks, sel?.table, pageSize],
  );

  useEffect(() => {
    setStates([null]);
    load(0, null);
  }, [load]);

  if (!schema) {
    return schemaError ? (
      <p className="text-[13px] text-danger">{schemaError}</p>
    ) : (
      <p className="text-[13px] text-subtle">
        <span className="shimmer">{db.state === "running" ? "Reading the schema…" : "Waking the database…"}</span>
      </p>
    );
  }
  if (!sel) {
    return (
      <EmptyPanel>
        <h2 className="text-[15px] font-medium">No tables yet</h2>
        <p className="max-w-md text-[13px] text-muted">
          Create a keyspace and a table in the console, or import one from a live cluster, and browse its rows here.
        </p>
      </EmptyPanel>
    );
  }

  const rows = page?.rows;
  const keyOf = (name: string) =>
    table?.partition_key.includes(name) ? "partition key" : table?.clustering_key.includes(name) ? "clustering" : null;
  const next = rows?.page_state ?? null;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-2">
        <select
          value={`${sel.ks}\u0000${sel.table}`}
          onChange={(e) => {
            const [k, t] = e.target.value.split("\u0000");
            onTarget({ ks: k, table: t });
          }}
          aria-label="Table"
          className={`${input} min-w-56 font-mono text-[12px]`}
        >
          {keyspaces.map((k) =>
            k.tables.length || k.views.length ? (
              <optgroup key={k.name} label={k.name}>
                {[...k.tables, ...k.views].map((t) => (
                  <option key={t.name} value={`${k.name}\u0000${t.name}`}>
                    {k.name}.{t.name}
                  </option>
                ))}
              </optgroup>
            ) : null,
          )}
          {!keyspaces.some((k) => k.name === sel.ks) && (
            <option value={`${sel.ks}\u0000${sel.table}`}>
              {sel.ks}.{sel.table}
            </option>
          )}
        </select>
        <label className="flex items-center gap-1.5 text-[12px] text-subtle">
          <input type="checkbox" checked={showSystem} onChange={(e) => setShowSystem(e.target.checked)} className="accent-current" />
          system
        </label>
        <span className="ml-auto flex items-center gap-2">
          <select
            value={pageSize}
            onChange={(e) => setPageSize(Number(e.target.value))}
            aria-label="Rows per page"
            className={`${input} font-mono text-[12px]`}
          >
            {PAGE_SIZES.map((n) => (
              <option key={n} value={n}>
                {n} / page
              </option>
            ))}
          </select>
          <button onClick={() => load(page?.index ?? 0, states[page?.index ?? 0] ?? null)} disabled={loading} className={btn} title="Reload page">
            {loading ? <HorizonMark state="thinking" size={14} interactive={false} /> : <RefreshCw className="size-3.5" />}
          </button>
        </span>
      </div>

      <Panel
        title={<span className="font-mono text-[13px]">{selectAll(sel.ks, sel.table)}</span>}
        aside={
          rows && (
            <span className="font-mono text-[11px] text-subtle">
              page {(page?.index ?? 0) + 1} · {rows.rows.length} row{rows.rows.length === 1 ? "" : "s"} · {page!.ms.toFixed(1)} ms
            </span>
          )
        }
      >
        {error ? (
          <p className="px-5 py-4 font-mono text-[12px] text-danger">{error}</p>
        ) : !rows ? (
          <p className="px-5 py-6 text-[13px] text-subtle">
            <span className="shimmer">Reading rows…</span>
          </p>
        ) : rows.rows.length === 0 ? (
          <p className="px-5 py-6 text-[13px] text-muted">No rows.</p>
        ) : (
          <div className={`overflow-x-auto transition-opacity ${loading ? "opacity-60" : ""}`}>
            <table className="w-full border-collapse font-mono text-[12px]">
              <thead>
                <tr className="border-b border-line text-left">
                  {rows.columns.map((c) => {
                    const key = keyOf(c.name);
                    return (
                      <th key={c.name} className="px-3 py-2 align-bottom font-normal whitespace-nowrap first:pl-5 last:pr-5">
                        <span className="flex items-center gap-1 text-fg">
                          {key && <KeyRound className="size-3 text-subtle" aria-label={key} />}
                          {c.name}
                        </span>
                        <span className="block text-[10px] text-subtle">{c.type}</span>
                      </th>
                    );
                  })}
                </tr>
              </thead>
              <tbody>
                {rows.rows.map((r, i) => (
                  <tr
                    key={i}
                    onClick={() => setOpen(open === i ? null : i)}
                    className={`cursor-pointer border-b border-line last:border-0 ${open === i ? "bg-panel-2" : "hover:bg-panel-2/60"}`}
                  >
                    {r.map((v, j) => (
                      <td key={j} className="max-w-72 truncate px-3 py-1.5 align-top first:pl-5 last:pr-5" title={v ?? "null"}>
                        <CqlValue value={v} />
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {rows && (
          <div className="flex items-center justify-between gap-3 border-t border-line px-5 py-2.5">
            <span className="text-[11px] text-subtle">
              {rows.warnings?.length ? rows.warnings.join(" · ") : "Click a row for its full values."}
            </span>
            <div className="flex gap-1">
              <button
                onClick={() => {
                  const i = (page?.index ?? 0) - 1;
                  if (i >= 0) load(i, states[i]);
                }}
                disabled={loading || !page || page.index === 0}
                className={btn}
                aria-label="Previous page"
              >
                <ChevronLeft className="size-3.5" />
              </button>
              <button
                onClick={() => {
                  if (!next || !page) return;
                  const i = page.index + 1;
                  setStates((s) => [...s.slice(0, i), next]);
                  load(i, next);
                }}
                disabled={loading || !next}
                className={btn}
                aria-label="Next page"
              >
                <ChevronRight className="size-3.5" />
              </button>
            </div>
          </div>
        )}
      </Panel>

      {rows && open != null && rows.rows[open] && <RowDetail rows={rows} index={open} keyOf={keyOf} onClose={() => setOpen(null)} />}
    </div>
  );
}

/** A value as cqlsh prints it; `null` recessive. */
export function CqlValue({ value }: { value: string | null }) {
  if (value == null) return <span className="text-subtle italic">null</span>;
  return <span className="text-fg">{value}</span>;
}

function RowDetail({
  rows,
  index,
  keyOf,
  onClose,
}: {
  rows: CqlRows;
  index: number;
  keyOf: (name: string) => string | null;
  onClose: () => void;
}) {
  const { copied, copy } = useCopy();
  const json = Object.fromEntries(rows.columns.map((c, i) => [c.name, rows.values[index]?.[i] ?? null]));
  return (
    <Panel
      title="Row"
      className="drawer-in"
      aside={
        <div className="flex items-center gap-1">
          <button onClick={() => copy("json", JSON.stringify(json, null, 2))} className="px-1.5 text-[12px] text-subtle hover:text-fg">
            {copied === "json" ? "Copied JSON" : "Copy as JSON"}
          </button>
          <button onClick={onClose} className="p-1 text-subtle hover:text-fg" aria-label="Close row">
            <X className="size-3.5" />
          </button>
        </div>
      }
    >
      <dl className="divide-y divide-line">
        {rows.columns.map((c, i) => {
          const v = rows.rows[index][i];
          const key = keyOf(c.name);
          return (
            <div key={c.name} className="grid grid-cols-[minmax(0,12rem)_minmax(0,1fr)_auto] items-start gap-3 px-5 py-2">
              <dt className="min-w-0">
                <p className="truncate font-mono text-[12px]">{c.name}</p>
                <p className="truncate font-mono text-[10px] text-subtle">
                  {c.type}
                  {key && ` · ${key}`}
                </p>
              </dt>
              <dd className="min-w-0 font-mono text-[12px] break-all whitespace-pre-wrap">
                <CqlValue value={v} />
              </dd>
              {v != null ? <CopyButton id={`c-${i}`} text={v} copied={copied} onCopy={copy} /> : <span />}
            </div>
          );
        })}
      </dl>
    </Panel>
  );
}
