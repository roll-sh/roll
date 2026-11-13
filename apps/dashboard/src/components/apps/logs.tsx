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
  // A search back through the stored days, shown instead of the live tail until left.
  const [found, setFound] = useState<{ q: string; lines: LogLine[]; truncated: boolean; error?: string } | null>(null);
  const [searching, setSearching] = useState(false);
  const search = async () => {
    const q = filter.trim();
    if (!q) return;
    setSearching(true);
    try {
      setFound({ q, ...(await appsApi.searchLogs(service, env, q, Date.now() - 7 * 86_400_000)) });
    } catch (e) {
      setFound({ q, lines: [], truncated: false, error: e instanceof Error ? e.message : String(e) });
    } finally {
      setSearching(false);
    }
  };
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
      { runtime: { service, env } },
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
  }, [service, env]);

  useEffect(() => {
    if (!paused) setLines(buffer.current.slice());
  }, [paused]);

  const instances = useMemo(() => [...new Set(lines.map((l) => l.instance).filter((i): i is string => !!i))], [lines]);
  const shown = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return lines.filter((l) => (!instance || l.instance === instance) && (!q || l.text.toLowerCase().includes(q)));
  }, [lines, filter, instance]);

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
          {filter.trim() && (
            <button type="button" className={btn} disabled={searching} onClick={() => void search()} title="Search the stored logs of the last 7 days, not just what's here">
              {searching ? "Searching…" : "Search 7 days"}
            </button>
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
            <>
              {found.truncated ? `The newest ${found.lines.length}` : found.lines.length} {found.lines.length === 1 ? "line" : "lines"} with “{found.q}” in the last 7 days
              {found.truncated && " (more matched: narrow the search)"}
            </>
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
        empty={lines.length ? "Nothing matches the filter." : open ? "No output yet. A service that's asleep logs again when a request wakes it." : <span className="shimmer">Connecting…</span>}
      />
    </div>
  );
}
