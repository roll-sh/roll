import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { appsApi, atLeast, followLog, PRODUCTION, type LogLevel, type LogLine, type Service } from "@/lib/apps-api";
import { matchesTerms, splitFilter } from "@/lib/log-where";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { btn, input, panel } from "../ui";
import { clock, lineTone } from "./shared";

/** Lines kept on screen, newest last. */
const KEEP = 2000;

type Line = LogLine & { service: string; key: number };

/**
 * A project's logs in one view (Railway's Log Explorer): each service's runtime log in one
 * environment, followed over its own stream (the same route and permission as its Logs tab, so
 * nothing new reads them) and merged by time, each line named by its service.
 */
export function ProjectLogs({ project }: { project: WorkspaceProject }) {
  const [services, setServices] = useState<Service[] | null>(null);
  const [envName, setEnvName] = useState(PRODUCTION);
  const [lines, setLines] = useState<Line[]>([]);
  const [paused, setPaused] = useState(false);
  const [filter, setFilter] = useState("");
  const [level, setLevel] = useState<LogLevel | "">("");
  const [only, setOnly] = useState("");
  const pausedRef = useRef(paused);
  pausedRef.current = paused;
  const seq = useRef(0);

  useEffect(() => {
    appsApi.services(project.id).then(setServices, () => setServices([]));
  }, [project.id]);
  const envs = useMemo(() => [...new Set((services ?? []).flatMap((s) => s.environments.map((e) => e.name)))].sort((a, b) => (a === PRODUCTION ? -1 : b === PRODUCTION ? 1 : a.localeCompare(b))), [services]);
  const following = useMemo(() => (services ?? []).filter((s) => s.environments.some((e) => e.name === envName)), [services, envName]);
  const ids = following.map((s) => s.id).join(",");

  useEffect(() => {
    setLines([]);
    const stops = following.map((s) =>
      followLog(
        { runtime: { service: s.id, env: envName } },
        {
          tail: 200,
          onReset: () => setLines((all) => all.filter((l) => l.service !== s.name)),
          onLine: (l) => {
            if (pausedRef.current) return;
            const line: Line = { ...l, service: s.name, key: seq.current++ };
            setLines((all) => {
              // Mostly in order: placed by time from the end, so a late line from one stream sits where it belongs.
              const next = [...all];
              let i = next.length;
              while (i > 0 && next[i - 1]!.t > line.t) i--;
              next.splice(i, 0, line);
              return next.length > KEEP ? next.slice(next.length - KEEP) : next;
            });
          },
        },
      ),
    );
    return () => stops.forEach((stop) => stop());
    // Restarted when the set of services or the environment changes, not on every poll.
  }, [ids, envName]); // eslint-disable-line react-hooks/exhaustive-deps

  const split = useMemo(() => splitFilter(filter), [filter]);
  const shown = useMemo(() => {
    const q = split.words.toLowerCase();
    return lines.filter((l) => (!only || l.service === only) && (!q || l.text.toLowerCase().includes(q)) && matchesTerms(split.terms, l.text) && atLeast(l, level));
  }, [lines, only, split, level]);

  const box = useRef<HTMLDivElement>(null);
  const [stuck, setStuck] = useState(true);
  useLayoutEffect(() => {
    if (stuck && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [shown, stuck]);

  if (services === null) return <p className="p-6 text-[13px] text-subtle"><span className="shimmer">Loading services…</span></p>;
  if (services.length === 0) return <p className="p-6 text-[13px] text-muted">This project has no services yet: their logs show here, together.</p>;

  return (
    <div className="flex h-full min-h-0 flex-col gap-3 p-4">
      <div className="flex flex-wrap items-center gap-2">
        <select value={envName} onChange={(e) => setEnvName(e.target.value)} aria-label="Environment" className="h-8 rounded-lg border border-line bg-panel px-2 font-mono text-[12px] outline-none">
          {envs.map((e) => (
            <option key={e} value={e}>
              {e}
            </option>
          ))}
        </select>
        <select value={only} onChange={(e) => setOnly(e.target.value)} aria-label="Service" className="h-8 rounded-lg border border-line bg-panel px-2 font-mono text-[12px] outline-none">
          <option value="">every service ({following.length})</option>
          {following.map((s) => (
            <option key={s.id} value={s.name}>
              {s.name}
            </option>
          ))}
        </select>
        <select value={level} onChange={(e) => setLevel(e.target.value as LogLevel | "")} aria-label="Level" className="h-8 rounded-lg border border-line bg-panel px-2 text-[12px] outline-none">
          <option value="">every level</option>
          <option value="warn">warnings and worse</option>
          <option value="error">errors and worse</option>
        </select>
        <input value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="Filter, or level:error user_id:42" aria-label="Filter lines" spellCheck={false} className={`${input} w-56 font-mono text-[12px]`} />
        <button type="button" className={btn} aria-pressed={paused} onClick={() => setPaused((p) => !p)}>
          {paused ? "Resume" : "Pause"}
        </button>
        <span className="ml-auto text-[11.5px] text-subtle">Live; each service's own Logs tab reads back through the stored days.</span>
      </div>
      <div
        ref={box}
        onScroll={(e) => {
          const el = e.currentTarget;
          setStuck(el.scrollHeight - el.scrollTop - el.clientHeight < 24);
        }}
        className={`${panel} min-h-0 flex-1 overflow-auto py-2 font-mono text-[12px] leading-[1.6]`}
        role="log"
        aria-label={`${project.name}'s logs`}
      >
        {shown.length === 0 ? (
          <p className="px-3 text-subtle">{lines.length ? "No line matches." : "Waiting for lines…"}</p>
        ) : (
          shown.map((l) => (
            <div key={l.key} className="flex gap-3 px-3 hover:bg-fg/[0.03]">
              <span className="shrink-0 text-subtle tabular-nums select-none max-sm:hidden">{clock(l.t)}</span>
              <span className="w-24 shrink-0 truncate text-muted select-none max-sm:w-14" title={`${l.service} · ${clock(l.t)}`}>
                {l.service}
              </span>
              {l.level && (l.level === "warn" || l.level === "error" || l.level === "fatal") && (
                <span className={`w-9 shrink-0 select-none uppercase ${l.level === "warn" ? "text-muted" : "text-danger"}`}>{l.level === "error" ? "err" : l.level}</span>
              )}
              <span className={`min-w-0 flex-1 [overflow-wrap:anywhere] whitespace-pre-wrap ${lineTone(l)}`}>{l.text}</span>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
