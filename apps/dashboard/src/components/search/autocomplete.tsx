/**
 * The Relevance tab's "Autocomplete" (Algolia's Query Suggestions, Elastic App Search's query
 * suggestions): roll builds `<index>-suggestions` from the index's search analytics on a schedule,
 * and clients query it with the completion suggester or `search_as_you_type`. Here an owner turns
 * it on, sees the current suggestions and the last build, blocks a term in one click (the setting,
 * then a rebuild) and tries a prefix as a client's autocomplete would.
 *
 * Nothing animates beyond the shared `.rise` and `.shimmer`; the weight bars are static.
 */

import { Ban, RefreshCw, Sparkles, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { esJson, searchApi, type SearchCluster } from "@/lib/search-api";
import {
  blockUpdate,
  completionOptions,
  completionRequest,
  relative,
  share,
  span,
  SUGGEST,
  suggestionIndices,
  unblockUpdate,
  type SuggestionsStatus,
} from "@/lib/search-suggestions";
import { btn, input, Panel, primary } from "../ui";
import { errText } from "./relevance-shared";

const enc = encodeURIComponent;

function Stat({ label, value, mono = true }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="min-w-0">
      <div className="text-[11px] text-subtle">{label}</div>
      <div className={`truncate text-[15px] font-semibold tracking-[-0.03em] tabular-nums ${mono ? "font-mono" : ""}`} title={value}>
        {value}
      </div>
    </div>
  );
}

/** A prefix tried against the suggestions index, as a client's autocomplete sends it. */
function TryPrefix({ cluster, alias, built }: { cluster: SearchCluster; alias: string; built: boolean }) {
  const [prefix, setPrefix] = useState("");
  const [options, setOptions] = useState<{ text: string; weight: number }[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const seq = useRef(0);
  useEffect(() => {
    if (!prefix.trim() || !built) {
      setOptions(null);
      setError(null);
      return;
    }
    const n = ++seq.current;
    const t = setTimeout(() => {
      esJson(cluster.id, "POST", `/${enc(alias)}/_search`, completionRequest(prefix.toLowerCase())).then(
        (a) => n === seq.current && (setOptions(completionOptions(a)), setError(null)),
        (e) => n === seq.current && setError(errText(e)),
      );
    }, 150);
    return () => clearTimeout(t);
  }, [prefix, alias, built, cluster.id]);
  return (
    <div className="min-w-0 space-y-2">
      <label className="block text-[11px] text-subtle" htmlFor="try-prefix">
        Try a prefix
      </label>
      <input
        id="try-prefix"
        className={`${input} w-full font-mono text-[12px]`}
        placeholder={built ? "Type what a user would…" : "Available after the first build"}
        disabled={!built}
        value={prefix}
        onChange={(e) => setPrefix(e.target.value)}
        spellCheck={false}
        autoComplete="off"
      />
      {error && <p className="text-[12px] text-danger">{error}</p>}
      {options && (
        <ul className="rise overflow-hidden rounded-lg border border-line" aria-label="Suggestions for the prefix">
          {options.length === 0 && <li className="px-3 py-2 text-[12px] text-subtle">No suggestion starts with that.</li>}
          {options.map((o) => (
            <li key={o.text} className="flex items-center justify-between gap-3 border-b border-line px-3 py-1.5 text-[13px] last:border-0">
              <span className="truncate">
                <span className="text-fg">{o.text.slice(0, prefix.length)}</span>
                <span className="text-muted">{o.text.slice(prefix.length)}</span>
              </span>
              <span className="font-mono text-[11px] tabular-nums text-subtle">{o.weight.toLocaleString("en-US")}</span>
            </li>
          ))}
        </ul>
      )}
      <p className="text-[11px] leading-relaxed text-subtle">
        Clients send the completion suggester on <span className="font-mono">suggest</span>, or a <span className="font-mono">bool_prefix</span> multi-match on{" "}
        <span className="font-mono">query</span>, <span className="font-mono">query._2gram</span> and <span className="font-mono">query._3gram</span>. A browser
        key needs only <span className="font-mono">read</span> on <span className="break-all font-mono">{alias}</span>.
      </p>
    </div>
  );
}

function Blocked({ status, busy, onUnblock }: { status: SuggestionsStatus; busy: boolean; onUnblock: (term: string, prefix: boolean) => void }) {
  const terms = [...status.config.block.map((t) => ({ t, prefix: false })), ...status.config.block_prefix.map((t) => ({ t, prefix: true }))];
  if (terms.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-2 border-t border-line px-5 py-3 text-[12px] text-muted">
      <span>Never suggested:</span>
      {terms.map(({ t, prefix }) => (
        <span key={`${prefix}:${t}`} className="inline-flex items-center gap-1 rounded-md border border-line py-0.5 pl-2 pr-1 text-[11px]">
          <span className="font-mono text-fg">
            {t}
            {prefix && <span className="text-subtle">…</span>}
          </span>
          <button disabled={busy} onClick={() => onUnblock(t, prefix)} className="rounded p-0.5 text-subtle transition hover:text-fg" aria-label={`Allow “${t}” again`}>
            <X className="size-3" />
          </button>
        </span>
      ))}
    </div>
  );
}

export function AutocompletePanel({ cluster }: { cluster: SearchCluster }) {
  const [indices, setIndices] = useState<{ index: string; suggestions: boolean }[] | null>(null);
  const [index, setIndex] = useState("");
  const [status, setStatus] = useState<SuggestionsStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  const reload = useCallback(() => setTick((t) => t + 1), []);

  useEffect(() => {
    let alive = true;
    esJson<Record<string, { settings?: Record<string, unknown> }>>(cluster.id, "GET", "/_settings/index.roll.*?flat_settings=true").then(
      (s) => {
        if (!alive) return;
        const list = suggestionIndices(s);
        setIndices(list);
        setIndex((cur) => (list.some((i) => i.index === cur) ? cur : (list.find((i) => i.suggestions) ?? list[0])?.index ?? ""));
      },
      (e) => alive && setError(errText(e)),
    );
    return () => {
      alive = false;
    };
  }, [cluster.id, tick]);

  useEffect(() => {
    if (!index) return setStatus(null);
    let alive = true;
    searchApi.suggestions(cluster.id, index).then(
      (s) => alive && (setStatus(s), setError(null)),
      (e) => alive && setError(errText(e)),
    );
    return () => {
      alive = false;
    };
  }, [cluster.id, index, tick]);

  /** A settings change, then a rebuild so it shows at once. */
  const apply = async (what: string, settings: Record<string, unknown> | null) => {
    setBusy(what);
    setNote(null);
    try {
      if (settings) await esJson(cluster.id, "PUT", `/${enc(index)}/_settings`, settings);
      if (!settings || settings[SUGGEST.enabled] !== null) {
        const s = await searchApi.rebuildSuggestions(cluster.id, index);
        if (!settings) setNote(s.changed ? `Rebuilt: ${s.last_build?.count ?? 0} suggestions.` : "Nothing changed since the last build.");
      }
      reload();
    } catch (e) {
      setError(errText(e));
      reload();
    } finally {
      setBusy(null);
    }
  };

  const picker = indices && indices.length > 1 && (
    <select className={`${input} py-1 font-mono text-[12px]`} value={index} onChange={(e) => setIndex(e.target.value)} aria-label="Index">
      {indices.map((i) => (
        <option key={i.index} value={i.index}>
          {i.index}
        </option>
      ))}
    </select>
  );

  if (!indices)
    return (
      <Panel title="Autocomplete">
        <p className="px-5 py-4 text-[13px] text-subtle">{error ? <span className="text-danger">{error}</span> : <span className="shimmer">Loading autocomplete…</span>}</p>
      </Panel>
    );
  if (indices.length === 0)
    return (
      <Panel title="Autocomplete">
        <div className="flex flex-col items-start gap-2 px-5 py-6">
          <p className="flex items-center gap-2 text-[14px] font-medium">
            <Sparkles className="size-4 text-subtle" /> Autocomplete from real searches
          </p>
          <p className="max-w-xl text-[13px] text-muted">
            roll can build an index&apos;s autocomplete from what people actually search for, refreshed daily. It&apos;s built from search analytics, so
            turn those on for an index first.
          </p>
        </div>
      </Panel>
    );

  const cfg = status?.config;
  const on = status?.enabled ?? false;
  const last = status?.last_build;
  const built = Boolean(last?.at);
  return (
    <Panel title="Autocomplete" aside={picker}>
      {!status || !cfg ? (
        <p className="px-5 py-4 text-[13px] text-subtle">{error ? <span className="text-danger">{error}</span> : <span className="shimmer">Loading autocomplete…</span>}</p>
      ) : !on ? (
        <div className="flex flex-col items-start gap-3 px-5 py-6">
          <p className="max-w-xl text-[13px] text-muted">
            Build <span className="font-mono text-fg">{cfg.index}</span> from the searches of <span className="font-mono text-fg">{index}</span>: queries searched at
            least {cfg.min_count} times in the last {span(cfg.lookback_ms)} that found results, weighted by how often they&apos;re searched and clicked, refreshed
            every {span(cfg.interval_ms)}. A query fewer people searched is never shown, so one person&apos;s unusual search stays theirs.
          </p>
          <button className={primary} disabled={busy !== null} onClick={() => apply("on", { [SUGGEST.enabled]: true })}>
            {busy === "on" ? <span className="shimmer">Building…</span> : "Turn on autocomplete"}
          </button>
          {error && <p className="text-[13px] text-danger">{error}</p>}
        </div>
      ) : (
        <>
          <div className="grid grid-cols-2 gap-4 px-5 pt-4 sm:grid-cols-4">
            <Stat label="Suggestions" value={built ? (last?.count ?? 0).toLocaleString("en-US") : "—"} />
            <Stat label="Last built" value={last?.at ? relative(last.at) : "never"} mono={false} />
            <Stat label="Next build" value={status.next_at ? relative(status.next_at) : "—"} mono={false} />
            <Stat label="Alias" value={cfg.index} />
          </div>
          <p className="px-5 pt-2 text-[12px] text-subtle">
            At least {cfg.min_count} searches in the last {span(cfg.lookback_ms)}, every {span(cfg.interval_ms)}
            {last?.at ? `; the last build took ${last.took_ms.toLocaleString("en-US")} ms` : ""}.
          </p>
          {(error || last?.error) && <p className="px-5 pt-2 text-[13px] text-danger">{error ?? last?.error}</p>}
          {note && <p className="rise px-5 pt-2 text-[13px] text-muted">{note}</p>}
          <div className="grid gap-6 px-5 py-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,20rem)]">
            <div className="min-w-0">
              <div className="mb-2 text-[11px] text-subtle">Heaviest suggestions</div>
              {status.suggestions.length === 0 ? (
                <p className="text-[13px] text-muted">
                  {built ? "No query has been searched often enough yet." : "Nothing built yet: suggestions appear once queries reach the minimum count."}
                </p>
              ) : (
                <ul className="overflow-hidden rounded-lg border border-line">
                  {status.suggestions.map((r) => (
                    <li key={r.query} className="group flex items-center gap-3 border-b border-line px-3 py-1.5 last:border-0">
                      <span className="min-w-0 flex-1 truncate font-mono text-[12px]" title={r.query}>
                        {r.query}
                      </span>
                      <span className="hidden h-1 w-20 overflow-hidden rounded-full bg-line sm:block" aria-hidden>
                        <span className="block h-full rounded-full bg-fg/40" style={{ width: `${share(r.weight, status.suggestions) * 100}%` }} />
                      </span>
                      <span className="w-14 text-right font-mono text-[11px] tabular-nums text-muted" title={`${r.searches} searches, ${r.clicks} clicks`}>
                        {r.weight.toLocaleString("en-US")}
                      </span>
                      <button
                        disabled={busy !== null}
                        onClick={() => apply(`block:${r.query}`, blockUpdate(cfg, r.query))}
                        className="rounded-md px-2 py-0.5 text-[12px] text-muted transition hover:bg-panel-2 hover:text-fg"
                        aria-label={`Never suggest “${r.query}”`}
                        title="Never suggest this"
                      >
                        {busy === `block:${r.query}` ? <span className="shimmer">Blocking…</span> : <><Ban className="mr-1 inline size-3.5 align-[-2px]" />Block</>}
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </div>
            <TryPrefix cluster={cluster} alias={cfg.index} built={built} />
          </div>
          <Blocked status={status} busy={busy !== null} onUnblock={(t, prefix) => apply(`unblock:${t}`, unblockUpdate(cfg, t, prefix))} />
          <div className="flex flex-wrap items-center gap-2 border-t border-line px-5 py-3">
            <button className={btn} disabled={busy !== null} onClick={() => apply("rebuild", null)}>
              <RefreshCw className="size-3.5" />
              {busy === "rebuild" ? <span className="shimmer">Rebuilding…</span> : "Rebuild now"}
            </button>
            <button className={btn} disabled={busy !== null} onClick={() => apply("off", { [SUGGEST.enabled]: null })}>
              {busy === "off" ? <span className="shimmer">Turning off…</span> : "Turn off"}
            </button>
            <span className="ml-auto max-w-md text-[11px] text-subtle">
              Turning it off stops rebuilds and keeps the index; turning analytics off deletes it with the queries.
            </span>
          </div>
        </>
      )}
    </Panel>
  );
}
