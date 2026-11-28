import { Minus, Plus, X } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { chartBody, chartData, type ChartData, countedOver, type DiscoverState, type FieldChart as Kind, localTime, viewSearch } from "@/lib/search-discover";
import { esJson, formatCount } from "@/lib/search-api";
import type { TimeRange } from "@/lib/search-docs";
import { RequestList, RequestsButton } from "./discover-requests";

/** A share of the documents: one decimal below 1%. */
function pct(n: number, of: number): string {
  if (!of) return "";
  const p = (100 * n) / of;
  return p < 1 ? `${p.toFixed(1)}%` : `${Math.round(p)}%`;
}
const num = (n: number) => (Number.isInteger(n) ? formatCount(n) : n.toLocaleString("en-US", { maximumSignificantDigits: 6 }));

/**
 * A field drawn over the view's whole query and range, in one request: a keyword's top 10 values
 * as bars (+ and − keep or leave a value out), a number's spread (`variable_width_histogram`, each
 * bar as wide as the values it holds) or a date's documents over time (a bar narrows the range).
 * Its statistics are the whole range's, not the sidebar's sample: the documents, the share with
 * a value, an estimate of the distinct values, and for numbers and dates the extent (numbers'
 * mean too).
 */
export function FieldChart({
  cluster,
  state,
  kind,
  field,
  name,
  zone,
  onFilter,
  onRange,
  onClose,
  onOpenConsole,
}: {
  cluster: string;
  state: DiscoverState;
  kind: Kind;
  /** The field aggregated (a text field's keyword multi-field). */
  field: string;
  /** The field as the sidebar names it. */
  name: string;
  zone: string;
  onFilter: (field: string, value: string | number | boolean, exclude: boolean) => void;
  onRange: (r: TimeRange) => void;
  onClose: () => void;
  onOpenConsole: (text: string) => void;
}) {
  const [data, setData] = useState<ChartData | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [showRequest, setShowRequest] = useState(false);
  // In ES|QL and PPL modes the query isn't a filter: the chart is of the range.
  const body = JSON.stringify(chartBody(countedOver(state), kind, field, zone));
  const request = useMemo(() => viewSearch(state.view, JSON.parse(body)), [state.view, body]);
  useEffect(() => {
    let live = true;
    setData(null);
    setErr(null);
    esJson(cluster, request.method, request.path, request.body).then(
      (r) => live && setData(chartData(kind, r)),
      (e) => live && setErr(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      live = false;
    };
  }, [cluster, request, kind]);

  const value = (v: number) => (kind === "date" ? localTime(v, zone).slice(0, 19) : num(v));
  return (
    <section aria-label={`Chart of ${name}`} className="rise border-b border-line px-5 py-3">
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <h3 className="font-mono text-[12px] text-fg">{name}</h3>
        <span className="text-[11px] text-subtle">{kind === "terms" ? "top 10 values" : kind === "number" ? "spread of values" : `over time (${zone})`}</span>
        {data && (
          <span className="font-mono text-[11px] text-subtle tabular-nums">
            {formatCount(data.total)} documents
            {data.missing != null && data.total > 0 && ` · present in ${pct(data.total - data.missing, data.total)}`}
            {data.distinct != null && ` · ${data.distinct > 3000 ? "≈" : ""}${formatCount(data.distinct)} distinct`}
            {data.min != null && data.max != null && ` · ${value(data.min)} to ${value(data.max)}`}
            {data.avg != null && ` · mean ${num(data.avg)}`}
          </span>
        )}
        <span className="ml-auto flex items-center gap-1">
          <RequestsButton count={1} shown={showRequest} onToggle={() => setShowRequest((v) => !v)} />
          <button type="button" onClick={onClose} className="rounded p-1 text-subtle transition hover:text-fg" aria-label="Close the chart">
            <X className="size-3.5" strokeWidth={1.5} />
          </button>
        </span>
      </div>
      {showRequest && (
        <div className="pt-2">
          <RequestList requests={[request]} onOpenConsole={onOpenConsole} />
        </div>
      )}
      {err ? (
        <p role="alert" className="pt-2 font-mono text-[12px] text-danger">
          {err}
        </p>
      ) : !data ? (
        <p className="pt-2 text-[12px] text-subtle">
          <span className="shimmer">Counting over the range…</span>
        </p>
      ) : data.bars.length === 0 ? (
        <p className="pt-2 text-[12px] text-subtle">No values in this range.</p>
      ) : kind === "terms" ? (
        <TermBars data={data} field={name} onFilter={onFilter} />
      ) : (
        <Columns data={data} kind={kind} label={value} onRange={onRange} />
      )}
    </section>
  );
}

/** Top values as horizontal bars, each a share of the documents in range. */
function TermBars({ data, field, onFilter }: { data: ChartData; field: string; onFilter: (field: string, value: string | number | boolean, exclude: boolean) => void }) {
  const max = Math.max(1, ...data.bars.map((b) => b.count));
  return (
    <ul className="space-y-1 pt-2" aria-label={`Top values of ${field}`}>
      {data.bars.map((b) => (
        <li key={String(b.key)} className="group grid grid-cols-[minmax(0,12rem)_minmax(0,1fr)_auto_auto] items-center gap-2 text-[11px]">
          <span className="truncate font-mono" title={String(b.key)}>
            {String(b.key)}
          </span>
          <span className="h-1.5 rounded-full bg-panel-2">
            <span className="block h-full rounded-full bg-fg/25 transition-[width,background-color] duration-200 ease-out group-hover:bg-accent motion-reduce:transition-none" style={{ width: `${Math.max(1, (100 * b.count) / max)}%` }} />
          </span>
          <span className="w-24 text-right font-mono text-subtle tabular-nums">
            {formatCount(b.count)} <span className="text-subtle/70">{pct(b.count, data.total)}</span>
          </span>
          <span className="flex">
            <button type="button" onClick={() => onFilter(field, b.key, false)} className="rounded p-0.5 text-subtle transition hover:text-fg" aria-label={`Filter for ${field}: ${b.key}`}>
              <Plus className="size-3" strokeWidth={1.5} />
            </button>
            <button type="button" onClick={() => onFilter(field, b.key, true)} className="rounded p-0.5 text-subtle transition hover:text-fg" aria-label={`Filter out ${field}: ${b.key}`}>
              <Minus className="size-3" strokeWidth={1.5} />
            </button>
          </span>
        </li>
      ))}
      {data.other > 0 && <li className="pt-1 font-mono text-[11px] text-subtle">{formatCount(data.other)} documents with other values</li>}
    </ul>
  );
}

/** Numbers or dates as columns on a linear axis from the first bar's start to the last's end. */
function Columns({ data, kind, label, onRange }: { data: ChartData; kind: Kind; label: (v: number) => string; onRange: (r: TimeRange) => void }) {
  const lo = data.bars[0].from ?? 0;
  const hi = Math.max(lo + 1e-9, data.bars[data.bars.length - 1].to ?? lo);
  const span = hi - lo;
  const max = Math.max(1, ...data.bars.map((b) => b.count));
  return (
    <>
      <div className="relative mt-2 h-20" role="list" aria-label={kind === "date" ? "Documents over time" : "Documents by value"}>
        {data.bars.map((b) => {
          const from = b.from ?? lo;
          const to = b.to ?? from;
          const left = (100 * (from - lo)) / span;
          const width = Math.max(0.6, (100 * (to - from)) / span);
          const title = `${label(from)}${to > from ? ` to ${label(to)}` : ""} · ${formatCount(b.count)}`;
          const bar = <span className="w-full rounded-t-sm bg-fg/20 transition-[background-color] duration-200 ease-out group-hover:bg-accent motion-reduce:transition-none" style={{ height: `${Math.max(3, (100 * b.count) / max)}%` }} />;
          const place = { left: `${left}%`, width: `calc(${Math.min(width, 100 - left)}% - 1px)` };
          return kind === "date" ? (
            <button key={String(b.key)} type="button" role="listitem" onClick={() => onRange({ gte: from, lt: to })} title={title} className="group absolute inset-y-0 flex items-end" style={place}>
              {bar}
            </button>
          ) : (
            <span key={String(b.key)} role="listitem" title={title} className="group absolute inset-y-0 flex items-end" style={place}>
              {bar}
            </span>
          );
        })}
      </div>
      <div className="flex justify-between pt-1 font-mono text-[11px] text-subtle">
        <span>{label(lo)}</span>
        <span>{label(hi)}</span>
      </div>
    </>
  );
}
