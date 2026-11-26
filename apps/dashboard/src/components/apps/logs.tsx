import { useEffect, useMemo, useRef, useState } from "react";
import { appsApi, followLog, type LogLine, atLeast, type LogLevel } from "@/lib/apps-api";
import { matchesTerms, splitFilter, wordsMatch } from "@/lib/log-where";
import { btn } from "../ui";
import { LogBar, LogView } from "./shared";
import { TraceWaterfall } from "./metrics";
import { usePageVisible } from "./live";

const MAX_LINES = 5000;

/** The environment's runtime log, live: every instance's stdout and stderr, filterable, pausable. */
export function RuntimeLogs({
  service,
  env,
  onGroup,
  retentionDays = 7,
  initialFilter,
  initialDays,
  onSearch,
}: {
  service: string;
  env: string;
  onGroup?: () => void;
  retentionDays?: number;
  /** A search from the URL: run once as the tab opens. */
  initialFilter?: string;
  initialDays?: number;
  /** A search ran (its filter and range): the page keeps them in its URL. */
  onSearch?: (filter: string, days: number) => void;
}) {
  const [lines, setLines] = useState<LogLine[]>([]);
  // A request's trace, opened from its line.
  const [trace, setTrace] = useState<string | null>(null);
  const [open, setOpen] = useState(false);
  const [paused, setPaused] = useState(false);
  const [filter, setFilter] = useState(initialFilter ?? "");
  const [instance, setInstance] = useState("");
  // Only what the app says is at this level or worse (lines that say none are left out).
  const [level, setLevel] = useState<LogLevel | "">("");
  // One deployment's lines (a canary beside the live one), or one stream's, as `roll logs` narrows.
  const [deployment, setDeployment] = useState("");
  const [stream, setStream] = useState<"" | "stdout" | "stderr" | "system">("");
  // The app's output, or the requests it served (the proxy's access log).
  const [requests, setRequests] = useState(false);
  // The Requests view narrowed: a status class (a 5xx alert, followed to its requests) and slow ones.
  const [status, setStatus] = useState("");
  const [slow, setSlow] = useState(0);
  // A search back through the stored days, or the lines before a time, shown instead of the
  // live tail until left.
  // `since`/`until`: the window that search read, fixed when it was read.
  const [found, setFound] = useState<{ q?: string; where?: string; before?: number; by?: string; since?: number; until?: number; lines: LogLine[]; truncated: boolean; error?: string } | null>(null);
  const [searching, setSearching] = useState(false);
  // How far back a search reads: up to what the environment keeps (its log retention).
  const [days, setDays] = useState(Math.min(initialDays ?? 7, Math.max(7, retentionDays)));
  const span = days === 1 ? "day" : `${days} days`;
  const read = async (q: { q?: string; where?: string; before?: number; by?: string }) => {
    setSearching(true);
    try {
      const narrow = requests ? { status: status || undefined, min_ms: slow || undefined } : {};
      const now = Date.now();
      const since = now - days * 86_400_000;
      const got = await appsApi.readLogs(service, env, { ...q, ...narrow, since, instance: instance || undefined, level: (!requests && level) || undefined, deployment: (!requests && deployment) || undefined, stream: (!requests && stream) || undefined, http: requests, tail: q.q || q.where ? 2000 : 500 });
      // Cut short at its newest lines: the bars cover only what was read, from its oldest line.
      const oldest = got.lines.reduce((m, l) => Math.min(m, l.t), Infinity);
      setFound({ ...q, ...got, since: got.truncated && Number.isFinite(oldest) ? oldest : since, until: q.before ?? now });
    } catch (e) {
      setFound({ ...q, lines: [], truncated: false, error: e instanceof Error ? e.message : String(e) });
    } finally {
      setSearching(false);
    }
  };
  const search = () => {
    // Field terms (`level:error user_id:42`) are the API's `where`; the other words its text search.
    const { where, words } = splitFilter(filter);
    if (where || words) {
      onSearch?.(filter, days);
      void read({ ...(words ? { q: words } : {}), ...(where ? { where } : {}) });
    }
  };
  // A search the URL brought: once, as the tab opens.
  const opened = useRef(false);
  useEffect(() => {
    if (opened.current || !initialFilter?.trim()) return;
    opened.current = true;
    search();
    // Only the first render's filter: later edits search when asked.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const at = (t: number) => new Date(t).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
  // Lines keep arriving while paused; the view shows them on resume.
  const buffer = useRef<LogLine[]>([]);
  const pausedRef = useRef(paused);
  pausedRef.current = paused;

  // A tab nobody sees follows nothing; shown again, it reads the newest lines afresh.
  const visible = usePageVisible();
  useEffect(() => {
    if (!visible) return;
    buffer.current = [];
    setLines([]);
    let pending = false;
    const flush = () => {
      pending = false;
      if (!pausedRef.current) setLines(buffer.current.slice());
    };
    const stop = followLog(
      { runtime: { service, env, http: requests } },
      {
        tail: 500,
        onOpen: setOpen,
        onReset: () => {
          buffer.current = [];
          flush();
        },
        onLine: (l) => {
          buffer.current.push(l);
          if (buffer.current.length > MAX_LINES) buffer.current.splice(0, buffer.current.length - MAX_LINES);
          // One render per frame however fast the lines come.
          if (!pending) {
            pending = true;
            requestAnimationFrame(flush);
          }
        },
      },
    );
    return stop;
  }, [service, env, requests, visible]);

  useEffect(() => {
    if (!paused) setLines(buffer.current.slice());
  }, [paused]);

  const instances = useMemo(() => [...new Set(lines.map((l) => l.instance).filter((i): i is string => !!i))], [lines]);
  const deployments = useMemo(() => [...new Set(lines.map((l) => l.deployment).filter((d): d is string => !!d))], [lines]);
  const shown = useMemo(() => {
    const split = splitFilter(filter);
    const request = (l: LogLine) => {
      if (!requests || (!status && !slow)) return true;
      try {
        const a = JSON.parse(l.text) as { status?: number; ms?: number };
        return (!status || String(a.status ?? "").startsWith(status[0])) && (!slow || (a.ms ?? 0) >= slow);
      } catch {
        return false;
      }
    };
    return lines.filter(
      (l) =>
        (!instance || l.instance === instance) &&
        wordsMatch(split.words, l.text) &&
        matchesTerms(split.terms, l.text) &&
        request(l) &&
        (requests || (atLeast(l, level) && (!deployment || l.deployment === deployment) && (!stream || l.stream === stream))),
    );
  }, [lines, filter, instance, requests, status, slow, level, deployment, stream]);

  return (
    <div>
      <LogBar
        filter={filter}
        onFilter={setFilter}
        paused={paused}
        onPause={setPaused}
        live={open}
        extra={
          <>
          <div className="flex rounded-lg border border-line p-0.5" role="radiogroup" aria-label="Which log">
            {([false, true] as const).map((r) => (
              <button
                key={String(r)}
                type="button"
                role="radio"
                aria-checked={requests === r}
                onClick={() => {
                  setRequests(r);
                  setFound(null);
                }}
                className={`h-6 rounded-md px-2 text-[11.5px] ${requests === r ? "bg-fg/[0.08] text-fg" : "text-muted hover:text-fg"}`}
              >
                {r ? "Requests" : "Output"}
              </button>
            ))}
          </div>
          {requests && (
            <>
              <select value={status} onChange={(e) => setStatus(e.target.value)} aria-label="Status" className="h-8 rounded-lg border border-line bg-panel px-2 font-mono text-[12px] outline-none">
                <option value="">every status</option>
                <option value="2xx">2xx</option>
                <option value="4xx">4xx</option>
                <option value="5xx">5xx</option>
              </select>
              <select value={slow} onChange={(e) => setSlow(Number(e.target.value))} aria-label="Slower than" className="h-8 rounded-lg border border-line bg-panel px-2 font-mono text-[12px] outline-none">
                <option value={0}>any time</option>
                <option value={500}>≥ 500 ms</option>
                <option value={1000}>≥ 1 s</option>
                <option value={5000}>≥ 5 s</option>
              </select>
              {(status || slow > 0) && (
                <button type="button" className={btn} disabled={searching} onClick={() => void read({})} title={`These requests from the stored ${span}, not just what's here`}>
                  Search {span}
                </button>
              )}
            </>
          )}
          {requests && (
            <button type="button" className={btn} disabled={searching} onClick={() => void read({ by: "any" })} title="Only the requests roll answered itself: an allow list, a firewall rule, a rate limit, a private environment, maintenance, a wake that failed">
              Turned away
            </button>
          )}
          {filter.trim() && !requests && (
            <button type="button" className={btn} disabled={searching} onClick={() => search()} title={`Search the stored logs of the last ${span}, not just what's here`}>
              {searching ? "Searching…" : `Search ${span}`}
            </button>
          )}
          <select value={days} onChange={(e) => setDays(Number(e.target.value))} aria-label="Search back" title="How far back a search reads (the environment keeps its logs this long at most)" className="h-8 rounded-lg border border-line bg-panel px-2 text-[12px] outline-none">
            {[1, 7, 30, 90]
              .filter((d) => d <= Math.max(7, retentionDays))
              .map((d) => (
                <option key={d} value={d}>
                  {d === 1 ? "last day" : `last ${d} days`}
                </option>
              ))}
          </select>
          <input
            type="datetime-local"
            aria-label="Read back to"
            title={`The lines before this time, from the stored logs of the last ${span}`}
            className="h-8 rounded-lg border border-line bg-panel px-2 font-mono text-[12px] text-muted outline-none"
            onChange={(e) => {
              const t = new Date(e.target.value).getTime();
              if (Number.isFinite(t)) void read({ before: t });
            }}
          />
          {!requests && (
            <select value={level} onChange={(e) => setLevel(e.target.value as LogLevel | "")} className="h-8 rounded-lg border border-line bg-panel px-2 text-[12px] outline-none" aria-label="Level" title="What the app's lines say their level is (JSON's level, logfmt's level=, a leading ERROR)">
              <option value="">every level</option>
              <option value="warn">warnings and worse</option>
              <option value="error">errors and worse</option>
              <option value="info">info and worse</option>
              <option value="debug">debug and worse</option>
            </select>
          )}
          {!requests && (
            <select value={stream} onChange={(e) => setStream(e.target.value as typeof stream)} className="h-8 rounded-lg border border-line bg-panel px-2 text-[12px] outline-none" aria-label="Stream">
              <option value="">every stream</option>
              <option value="stdout">stdout</option>
              <option value="stderr">stderr</option>
              <option value="system">roll's lines</option>
            </select>
          )}
          {!requests && deployments.length > 1 && (
            <select value={deployment} onChange={(e) => setDeployment(e.target.value)} className="h-8 rounded-lg border border-line bg-panel px-2 font-mono text-[12px] outline-none" aria-label="Deployment">
              <option value="">every deployment</option>
              {deployments.map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
            </select>
          )}
          {instances.length > 1 && (
            <select value={instance} onChange={(e) => setInstance(e.target.value)} className="h-8 rounded-lg border border-line bg-panel px-2 font-mono text-[12px] outline-none" aria-label="Instance">
              <option value="">every instance</option>
              {instances.map((i) => (
                <option key={i} value={i}>
                  {i}
                </option>
              ))}
            </select>
          )}
          </>
        }
      />
      {found && (
        <p className="mb-2 flex items-center gap-2 text-[12px] text-muted">
          {found.error ? (
            <span className="text-danger">The search failed: {found.error}</span>
          ) : (
            found.by ? (
              <>
                {found.lines.length} {found.lines.length === 1 ? "request" : "requests"} roll answered itself in the last {span}
                {found.truncated && " (the newest)"}
              </>
            ) : found.q || found.where ? (
              <>
                {found.truncated ? `The newest ${found.lines.length}` : found.lines.length} {found.lines.length === 1 ? "line" : "lines"} with “{[found.where, found.q].filter(Boolean).join(" ")}” in the last {span}
                {found.truncated && " (more matched: narrow the search)"}
              </>
            ) : (
              <>
                {found.lines.length ? `${at(found.lines[0].t)} to ${at(found.lines[found.lines.length - 1].t)}` : `Nothing before ${at(found.before ?? Date.now())}`}
                {found.lines.length > 0 && (
                  <button type="button" className="text-accent hover:underline" disabled={searching} onClick={() => void read({ before: found.lines[0].t + 1 })}>
                    Earlier
                  </button>
                )}
              </>
            )
          )}
          {found.lines.length > 0 && (
            <button type="button" className="text-accent hover:underline" onClick={() => download(found.lines, `${service}-${env}-logs.ndjson`)} title="These lines, one JSON object each">
              Download
            </button>
          )}
          <button type="button" className="text-accent hover:underline" onClick={() => setFound(null)}>
            Back to live
          </button>
        </p>
      )}
      {found && (found.q || found.where) && found.lines.length > 0 && <Histogram lines={found.lines} since={found.since ?? Date.now() - days * 86_400_000} until={found.until ?? Date.now()} />}
      {trace && (
        <div className="rise mb-2 rounded-lg border border-line bg-panel px-3 py-2">
          <div className="mb-2 flex items-center justify-between text-[11.5px] text-muted">
            <span>
              Trace <span className="font-mono text-subtle">{trace.slice(0, 16)}</span>
            </span>
            <button type="button" className="hover:text-fg" onClick={() => setTrace(null)}>
              Close
            </button>
          </div>
          <TraceWaterfall service={service} env={env} trace={trace} />
        </div>
      )}
      <LogView
        lines={found ? found.lines : shown}
        columns
        onTrace={setTrace}
        onContext={found && (found.q || found.where || found.by) ? (l) => void read({ before: l.t + 30_000 }) : undefined}
        onGroup={onGroup}
        onBlock={async (ip) => {
          // First in the list (the first match decides), unless a rule denies it already.
          const now = await appsApi.environment(service, env);
          const rules = now.firewall ?? [];
          // Addresses are kept as networks (`a.b.c.d/32`).
          const as = [ip, `${ip}/32`, `${ip}/128`];
          if (rules.some((r) => r.action === "deny" && r.ips?.some((n) => as.includes(n)) && !r.path && !r.methods?.length)) return;
          await appsApi.putEnvironment(service, env, { firewall: [{ action: "deny", ips: [ip] }, ...rules] });
        }}
        className="h-[min(560px,65svh)]"
        empty={
          lines.length ? (
            "Nothing matches the filter."
          ) : !open ? (
            <span className="shimmer">Connecting…</span>
          ) : requests ? (
            "No requests yet. Each request through roll shows here: method, path, status and time."
          ) : (
            "No output yet. A service that's asleep logs again when a request wakes it."
          )
        }
      />
    </div>
  );
}

/** Lines as an NDJSON file the browser saves. */
function download(lines: LogLine[], name: string) {
  const url = URL.createObjectURL(new Blob([lines.map((l) => JSON.stringify(l)).join("\n") + "\n"], { type: "application/x-ndjson" }));
  const a = Object.assign(document.createElement("a"), { href: url, download: name });
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 0);
}

/** Matches over the searched window (Railway's Log Explorer, Cloud Logging's histogram): the
 *  lines found, in 48 bars, their error lines (an error or fatal level, or stderr) marked. */
export function histogram(lines: LogLine[], since: number, until: number, n = 48) {
  const width = Math.max(1, (until - since) / n);
  const bars = Array.from({ length: n }, (_, i) => ({ from: since + i * width, lines: 0, errors: 0 }));
  for (const l of lines) {
    const i = Math.min(n - 1, Math.max(0, Math.floor((l.t - since) / width)));
    bars[i]!.lines++;
    if (l.level === "error" || l.level === "fatal" || l.stream === "stderr") bars[i]!.errors++;
  }
  return { bars, width };
}

function Histogram({ lines, since, until }: { lines: LogLine[]; since: number; until: number }) {
  const { bars, width } = histogram(lines, since, until);
  const top = Math.max(1, ...bars.map((b) => b.lines));
  const at = (ms: number) => new Date(ms).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
  return (
    <div className="mb-2 flex h-10 items-end gap-px" role="img" aria-label="Matches over time">
      {bars.map((b) => (
        <div key={b.from} className="relative flex-1" style={{ height: `${(b.lines / top) * 100}%`, minHeight: b.lines ? 2 : 0 }} title={`${b.lines} ${b.lines === 1 ? "line" : "lines"}${b.errors ? `, ${b.errors} errors` : ""} · ${at(b.from)} – ${at(b.from + width)}`}>
          <div className="absolute inset-0 rounded-[1px] bg-fg/20" />
          {b.errors > 0 && <div className="absolute inset-x-0 bottom-0 rounded-[1px] bg-danger/70" style={{ height: `${(b.errors / b.lines) * 100}%` }} />}
        </div>
      ))}
    </div>
  );
}
