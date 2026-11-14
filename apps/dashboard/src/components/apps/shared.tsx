import { ArrowDown, Pause, Play, Search, X } from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { deployStatus, instanceStateLabel, instanceTone, type AccessLine, type DeployStatus, type InstanceState, type LogLine, type Tone } from "@/lib/apps-api";
import { useMarkState } from "../mark-state";

export const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

const TONE_DOT: Record<Tone, string> = {
  busy: "bg-accent pulse-dot",
  ok: "bg-ok",
  error: "bg-danger",
  muted: "bg-subtle",
};

/** A deployment's status: a dot (the accent only while it's on its way) and its word. */
export function DeployBadge({ status, className = "" }: { status: DeployStatus; className?: string }) {
  const s = deployStatus(status);
  return (
    <span className={`inline-flex items-center gap-1.5 text-[12px] ${s.tone === "error" ? "text-danger" : s.tone === "muted" ? "text-muted" : "text-fg"} ${className}`}>
      <i className={`size-1.5 shrink-0 rounded-full ${TONE_DOT[s.tone]}`} />
      {s.label}
    </span>
  );
}

const STATE_DOT = { online: "bg-ok", starting: "bg-accent pulse-dot", idle: "border border-line-2 bg-transparent", failed: "bg-danger" } as const;

export function StateBadge({ state }: { state: InstanceState }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <i className={`inline-block size-2 shrink-0 rounded-full ${STATE_DOT[instanceTone(state)]}`} />
      {instanceStateLabel(state)}
    </span>
  );
}

/** Runs an action against rolld; a failure shows and plays `error` on every mark. */
export function useRunner(after: () => unknown) {
  const { play } = useMarkState();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // What failed (the action's key: "firewall", "restart:<instance>"), for the toast to name.
  const [failed, setFailed] = useState<string | null>(null);
  const run = async <T,>(key: string, fn: () => Promise<T>): Promise<T | undefined> => {
    setBusy(key);
    setError(null);
    try {
      const out = await fn();
      await after();
      return out;
    } catch (e) {
      setFailed(key.split(":")[0]);
      setError(message(e));
      play([["error", 1600]]);
      return undefined;
    } finally {
      setBusy(null);
    }
  };
  return { busy, error, failed, setError, run };
}
export type Runner = ReturnType<typeof useRunner>;

/**
 * A runner's error where it can be seen whatever was clicked: fixed to the bottom of the window
 * (Settings is screens tall: a note above the tabs is off-screen), naming what failed, until
 * dismissed or the next action.
 */
export function RunnerError({ runner }: { runner: Runner }) {
  if (!runner.error) return null;
  return (
    <div role="alert" className="rise fixed inset-x-4 bottom-4 z-50 mx-auto flex max-w-xl items-start gap-3 rounded-xl border border-danger/40 bg-panel px-4 py-3 text-[13px]">
      <i className="mt-1.5 size-1.5 shrink-0 rounded-full bg-danger" />
      <p className="min-w-0 flex-1 text-fg">
        {runner.failed && <span className="text-muted">{runner.failed.replaceAll("-", " ")} didn't work: </span>}
        {runner.error}
      </p>
      <button type="button" className="text-muted hover:text-fg" aria-label="Dismiss" onClick={() => runner.setError(null)}>
        <X className="size-3.5" strokeWidth={1.5} />
      </button>
    </div>
  );
}

/** Unix ms as the time of day (and the date when it isn't today). */
export function clock(t: number, withDate = false) {
  const d = new Date(t);
  const time = d.toLocaleTimeString("en-US", { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
  return withDate ? `${d.toLocaleDateString("en-US", { month: "short", day: "numeric" })} ${time}` : time;
}

export function since(t: number, now = Date.now()) {
  const s = Math.max(0, Math.floor((now - t) / 1000));
  if (s < 45) return "just now";
  if (s < 3600) return `${Math.max(1, Math.round(s / 60))}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

/** How far ahead `t` is: "in 4m", "in 3h", "in 2d". */
export function until(t: number, now = Date.now()) {
  const s = Math.max(0, Math.floor((t - now) / 1000));
  if (s < 60) return "in under a minute";
  if (s < 3600) return `in ${Math.round(s / 60)}m`;
  if (s < 86400) return `in ${Math.round(s / 3600)}h`;
  return `in ${Math.round(s / 86400)}d`;
}

const STREAM_TEXT: Record<LogLine["stream"], string> = {
  stdout: "text-fg",
  stderr: "text-danger",
  build: "text-soft",
  system: "text-muted",
  http: "text-fg",
};

/** Why roll answered a request itself, as a person reads it. */
const BY: Record<NonNullable<AccessLine["by"]>, string> = {
  allow_list: "not on the allow list",
  firewall: "firewall rule",
  rate_limit: "rate limited",
  private: "private environment",
  maintenance: "maintenance",
  wake: "couldn't wake",
  capped: "spending limit",
  paused: "paused",
  not_deployed: "not deployed",
  moving: "moving nodes",
  unavailable: "settings unreadable",
};

/** An access line as a person reads it: `GET /path 200 12ms`, a 5xx in red, a cold start's wait. */
function AccessText({ text }: { text: string }) {
  let a: AccessLine;
  try {
    a = JSON.parse(text) as AccessLine;
  } catch {
    return <>{text}</>;
  }
  return (
    <>
      <span className="text-muted">{a.method}</span> {a.path} <span className={a.status >= 500 ? "text-danger" : a.status >= 400 ? "text-soft" : "text-muted"}>{a.status}</span>{" "}
      <span className="text-subtle">
        {a.ms}ms{a.cold ? ` · cold +${a.waited_ms}ms` : ""}
        {a.by ? ` · ${BY[a.by] ?? a.by}` : ""}
        {a.ip ? ` · ${a.ip}` : ""}
      </span>
    </>
  );
}

/**
 * A log in mono: time, (stream and instance), text. Follows the end while it's scrolled there;
 * scrolling up stops following until the end is reached again.
 */
export function LogView({ lines, columns = false, empty, className = "h-[420px]" }: { lines: LogLine[]; columns?: boolean; empty: ReactNode; className?: string }) {
  const box = useRef<HTMLDivElement>(null);
  const [stuck, setStuck] = useState(true);
  useLayoutEffect(() => {
    if (stuck && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [lines, stuck]);
  return (
    <div className="relative">
      <div
        ref={box}
        onScroll={(e) => {
          const el = e.currentTarget;
          setStuck(el.scrollHeight - el.scrollTop - el.clientHeight < 24);
        }}
        className={`overflow-auto rounded-lg border border-line bg-bg py-2 font-mono text-[11.5px] leading-[1.6] ${className}`}
        role="log"
        aria-live="off"
      >
        {lines.length === 0 ? (
          <div className="px-3 text-subtle">{empty}</div>
        ) : (
          lines.map((l, i) => (
            <div key={i} className="flex gap-3 px-3 hover:bg-fg/[0.03]">
              <span className="shrink-0 text-subtle tabular-nums select-none">{clock(l.t)}</span>
              {columns && (
                <span className="w-24 shrink-0 truncate text-subtle select-none" title={l.instance ?? l.stream}>
                  {l.stream === "stdout" || l.stream === "stderr" ? (l.instance ?? l.stream) : l.stream}
                </span>
              )}
              <span className={`min-w-0 flex-1 break-all whitespace-pre-wrap ${STREAM_TEXT[l.stream]}`}>{l.stream === "http" ? <AccessText text={l.text} /> : l.text}</span>
            </div>
          ))
        )}
      </div>
      {!stuck && lines.length > 0 && (
        <button
          onClick={() => setStuck(true)}
          className="rise absolute right-3 bottom-3 inline-flex items-center gap-1 rounded-md border border-line-2 bg-panel px-2 py-1 text-[11px] text-muted hover:text-fg"
        >
          <ArrowDown className="size-3" /> Follow
        </button>
      )}
    </div>
  );
}

/** The bar over a live log: filter text, pause and follow. */
export function LogBar({
  filter,
  onFilter,
  paused,
  onPause,
  live,
  extra,
}: {
  filter: string;
  onFilter: (f: string) => void;
  paused: boolean;
  onPause: (p: boolean) => void;
  live: boolean;
  extra?: ReactNode;
}) {
  return (
    <div className="mb-2 flex flex-wrap items-center gap-2">
      <label className="flex h-8 min-w-0 flex-1 items-center gap-2 rounded-lg border border-line bg-panel px-2.5 text-subtle focus-within:border-line-2">
        <Search className="size-3.5 shrink-0" />
        <input value={filter} onChange={(e) => onFilter(e.target.value)} placeholder="Filter" className="min-w-0 flex-1 bg-transparent font-mono text-[12px] text-fg outline-none placeholder:text-subtle" />
      </label>
      {extra}
      <button onClick={() => onPause(!paused)} className="inline-flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] text-muted hover:border-line-2 hover:text-fg" aria-pressed={paused}>
        {paused ? <Play className="size-3.5" /> : <Pause className="size-3.5" />}
        {paused ? "Resume" : "Pause"}
      </button>
      <span className="inline-flex items-center gap-1.5 text-[11.5px] text-subtle">
        <i className={`size-1.5 rounded-full ${live && !paused ? "bg-accent pulse-dot" : "bg-subtle"}`} />
        {paused ? "paused" : live ? "live" : "connecting"}
      </span>
    </div>
  );
}

/** Keeps the latest value of something in a ref (for callbacks a stream holds on to). */
export function useLatest<T>(v: T) {
  const r = useRef(v);
  useEffect(() => {
    r.current = v;
  });
  return r;
}

/** The sizes an instance comes in. */
export const CPU = [0.25, 0.5, 1, 2, 4, 8];
export const MEMORY = [256, 512, 1024, 2048, 4096, 8192, 16384];
export const mb = (n: number) => (n >= 1024 ? `${n / 1024} GB` : `${n} MB`);

/**
 * A button for something that's hard to take back (a rollback, a removal, a restart): the first
 * click arms it ("Sure? …" for four seconds), the second does it. Lighter than typing a name,
 * for what doesn't destroy data.
 */
export function SureButton({
  className,
  disabled,
  title,
  label,
  sure,
  onSure,
  children,
}: {
  className: string;
  disabled?: boolean;
  title?: string;
  /** Its accessible name, armed or not (an icon-only button). */
  label?: string;
  /** What the armed button says. */
  sure: string;
  onSure: () => void;
  children: ReactNode;
}) {
  const [armed, setArmed] = useState(false);
  useEffect(() => {
    if (!armed) return;
    const t = setTimeout(() => setArmed(false), 4000);
    return () => clearTimeout(t);
  }, [armed]);
  return (
    <button
      type="button"
      className={`${className} ${armed ? "!text-danger" : ""}`}
      disabled={disabled}
      title={title}
      aria-label={label && (armed ? `${sure} (click again)` : label)}
      onClick={() => {
        if (!armed) return setArmed(true);
        setArmed(false);
        onSure();
      }}
      onBlur={() => setArmed(false)}
    >
      {armed ? <span className="text-[12px]">{sure}</span> : children}
    </button>
  );
}

