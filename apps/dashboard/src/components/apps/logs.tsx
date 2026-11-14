import { useEffect, useMemo, useRef, useState } from "react";
import { appsApi, followLog, type LogLine } from "@/lib/apps-api";
import { btn } from "../ui";
import { LogBar, LogView } from "./shared";

const MAX_LINES = 5000;

/** The environment's runtime log, live: every instance's stdout and stderr, filterable, pausable. */
export function RuntimeLogs({ service, env }: { service: string; env: string }) {
  const [lines, setLines] = useState<LogLine[]>([]);
  const [open, setOpen] = useState(false);
  const [paused, setPaused] = useState(false);
  const [filter, setFilter] = useState("");
  const [instance, setInstance] = useState("");
  // The app's output, or the requests it served (the proxy's access log).
  const [requests, setRequests] = useState(false);
  // The Requests view narrowed: a status class (a 5xx alert, followed to its requests) and slow ones.
  const [status, setStatus] = useState("");
  const [slow, setSlow] = useState(0);
  // A search back through the stored days, or the lines before a time, shown instead of the
  // live tail until left.
  const [found, setFound] = useState<{ q?: string; before?: number; by?: string; lines: LogLine[]; truncated: boolean; error?: string } | null>(null);
  const [searching, setSearching] = useState(false);
  const read = async (q: { q?: string; before?: number; by?: string }) => {
    setSearching(true);
    try {
      const narrow = requests ? { status: status || undefined, min_ms: slow || undefined } : {};
      const got = await appsApi.readLogs(service, env, { ...q, ...narrow, since: Date.now() - 7 * 86_400_000, instance: instance || undefined, http: requests, tail: q.q ? 2000 : 500 });
      setFound({ ...q, ...got });
    } catch (e) {
      setFound({ ...q, lines: [], truncated: false, error: e instanceof Error ? e.message : String(e) });
    } finally {
      setSearching(false);
    }
  };
  const search = () => {
    const q = filter.trim();
    if (q) void read({ q });
  };
  const at = (t: number) => new Date(t).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
  // Lines keep arriving while paused; the view shows them on resume.
  const buffer = useRef<LogLine[]>([]);
  const pausedRef = useRef(paused);
  pausedRef.current = paused;

  useEffect(() => {
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
  }, [service, env, requests]);

  useEffect(() => {
    if (!paused) setLines(buffer.current.slice());
  }, [paused]);

  const instances = useMemo(() => [...new Set(lines.map((l) => l.instance).filter((i): i is string => !!i))], [lines]);
  const shown = useMemo(() => {
    const q = filter.trim().toLowerCase();
    const request = (l: LogLine) => {
      if (!requests || (!status && !slow)) return true;
      try {
        const a = JSON.parse(l.text) as { status?: number; ms?: number };
        return (!status || String(a.status ?? "").startsWith(status[0])) && (!slow || (a.ms ?? 0) >= slow);
      } catch {
        return false;
      }
    };
    return lines.filter((l) => (!instance || l.instance === instance) && (!q || l.text.toLowerCase().includes(q)) && request(l));
  }, [lines, filter, instance, requests, status, slow]);

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
                <button type="button" className={btn} disabled={searching} onClick={() => void read({})} title="These requests from the stored week, not just what's here">
                  Search 7 days
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
            <button type="button" className={btn} disabled={searching} onClick={() => search()} title="Search the stored logs of the last 7 days, not just what's here">
              {searching ? "Searching…" : "Search 7 days"}
            </button>
          )}
          <input
            type="datetime-local"
            aria-label="Read back to"
            title="The lines before this time, from the stored logs of the last 7 days"
            className="h-8 rounded-lg border border-line bg-panel px-2 font-mono text-[12px] text-muted outline-none"
            onChange={(e) => {
              const t = new Date(e.target.value).getTime();
              if (Number.isFinite(t)) void read({ before: t });
            }}
          />
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
                {found.lines.length} {found.lines.length === 1 ? "request" : "requests"} roll answered itself in the last 7 days
                {found.truncated && " (the newest)"}
              </>
            ) : found.q ? (
              <>
                {found.truncated ? `The newest ${found.lines.length}` : found.lines.length} {found.lines.length === 1 ? "line" : "lines"} with “{found.q}” in the last 7 days
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
          <button type="button" className="text-accent hover:underline" onClick={() => setFound(null)}>
            Back to live
          </button>
        </p>
      )}
      <LogView
        lines={found ? found.lines : shown}
        columns
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
