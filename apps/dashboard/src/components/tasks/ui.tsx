"use client";

import { HorizonMark, type MarkState } from "@roll/brand";
import { Check, Copy, X } from "lucide-react";
import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import { prettyBody, prettyJson, type MsgState, type RunStatus } from "@/lib/tasks-api";
import { useMarkState } from "../mark-state";

// Same control styles as databases.tsx.
export const btn =
  "inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted transition hover:border-line-2 hover:text-fg disabled:opacity-40";
export const primary =
  "inline-flex shrink-0 items-center gap-1.5 rounded-lg bg-fg px-3 py-1.5 text-[13px] font-medium text-bg transition hover:opacity-85 disabled:opacity-50";
export const input =
  "rounded-lg border border-line bg-panel px-2.5 py-1.5 text-[13px] outline-none transition placeholder:text-subtle focus:border-line-2";
export const iconBtn =
  "rounded-md p-1.5 text-subtle transition hover:bg-panel-2 hover:text-fg disabled:opacity-40";
export const dangerLink = "inline-flex items-center gap-1.5 text-[12px] text-subtle transition hover:text-danger disabled:opacity-40";

export const ICON = { className: "size-3.5", strokeWidth: 1.5 } as const;

// --- actions: one busy key, one error banner, the shared mark reflects progress ---

interface ActionsContext {
  busy: string | null;
  error: string | null;
  setError: (e: string | null) => void;
  /**
   * Run a mutation. The logo plays `mark` (deploying by default) and then success / error.
   * Returns the result, or undefined when it failed (the error is shown in the banner).
   */
  run: <T>(key: string, fn: () => Promise<T>, mark?: MarkState) => Promise<T | undefined>;
}

const Actions = createContext<ActionsContext | null>(null);

export function ActionsProvider({ children }: { children: ReactNode }) {
  const { play, setState } = useMarkState();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const run = useCallback(
    async <T,>(key: string, fn: () => Promise<T>, mark: MarkState = "deploying") => {
      setBusy(key);
      setError(null);
      setState(mark);
      try {
        const out = await fn();
        play([["success", 1200]]);
        return out;
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
        play([["error", 1400]]);
        return undefined;
      } finally {
        setBusy(null);
      }
    },
    [play, setState],
  );

  return <Actions.Provider value={{ busy, error, setError, run }}>{children}</Actions.Provider>;
}

export function useActions() {
  const ctx = useContext(Actions);
  if (!ctx) throw new Error("useActions must be used inside <ActionsProvider>");
  return ctx;
}

// --- polling ---

/**
 * Load `fn` now and every `ms` while mounted and the page is visible. Changing `key` reloads
 * at once. Only the visible tab is mounted, so only it polls.
 */
export function usePoll<T>(fn: () => Promise<T>, ms: number, key = "") {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  const ref = useRef(fn);
  useEffect(() => {
    ref.current = fn;
  });

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      if (!document.hidden) {
        try {
          const v = await ref.current();
          if (!alive) return;
          setData(v);
          setError(null);
        } catch (e) {
          if (alive) setError(e instanceof Error ? e.message : String(e));
        }
      }
      if (alive) timer = setTimeout(loop, ms);
    };
    loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [ms, key, tick]);

  const reload = useCallback(() => setTick((t) => t + 1), []);
  return { data, error, reload, setData };
}

/** Re-render every `ms` so relative times stay fresh between polls. */
export function useNow(ms = 1000) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

export function useCopy() {
  const [copied, setCopied] = useState<string | null>(null);
  const copy = (key: string, text: string) => {
    navigator.clipboard?.writeText(text);
    setCopied(key);
    setTimeout(() => setCopied((c) => (c === key ? null : c)), 1200);
  };
  return { copied, copy };
}

// --- primitives ---

export function Panel({
  title,
  meta,
  actions,
  children,
  className = "",
}: {
  title?: ReactNode;
  meta?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={`flex min-w-0 flex-col rounded-2xl border border-line bg-panel ${className}`}>
      {(title || meta || actions) && (
        <div className="flex items-center justify-between gap-3 border-b border-line px-5 py-3.5">
          <h2 className="truncate text-[15px] font-medium">{title}</h2>
          <div className="flex shrink-0 items-center gap-2">
            {meta && <span className="text-[12px] text-subtle">{meta}</span>}
            {actions}
          </div>
        </div>
      )}
      {children}
    </section>
  );
}

/** Message and run states. Accent for live work, muted for done, red only for failures. */
export function StateDot({ state, title }: { state: MsgState | RunStatus | "paused" | "live"; title?: string }) {
  const cls =
    state === "active" || state === "running" || state === "live"
      ? "bg-accent animate-pulse motion-reduce:animate-none"
      : state === "pending" || state === "queued" || state === "waiting"
        ? "bg-accent/60"
        : state === "failed"
          ? "bg-danger/80"
          : state === "delivered" || state === "completed"
            ? "bg-muted"
            : "border border-line-2 bg-transparent";
  return <span title={title ?? state} className={`inline-block size-2 shrink-0 rounded-full ${cls}`} />;
}

export function Chip({ children, title, tone = "default" }: { children: ReactNode; title?: string; tone?: "default" | "accent" | "danger" }) {
  const cls =
    tone === "accent"
      ? "border-accent/30 text-accent"
      : tone === "danger"
        ? "border-danger/30 text-danger"
        : "border-line text-muted";
  return (
    <span title={title} className={`inline-flex max-w-full items-center gap-1 truncate rounded-md border px-1.5 py-px font-mono text-[11px] ${cls}`}>
      {children}
    </span>
  );
}

/** Small "label: value" pair for detail grids. */
export function Field({ label, children, mono = true }: { label: string; children: ReactNode; mono?: boolean }) {
  return (
    <div className="min-w-0">
      <p className="text-[11px] text-subtle">{label}</p>
      <div className={`mt-0.5 truncate text-[12px] ${mono ? "font-mono" : ""}`}>{children}</div>
    </div>
  );
}

export function CopyButton({ text, id, label }: { text: string; id?: string; label?: string }) {
  const { copied, copy } = useCopy();
  const k = id ?? text;
  return (
    <button onClick={() => copy(k, text)} className={iconBtn} title={label ?? "Copy"} type="button">
      {copied === k ? <Check {...ICON} className="size-3.5 text-ok" /> : <Copy {...ICON} />}
    </button>
  );
}

/** JSON (or text) in a mono block; JSON strings are pretty-printed. */
export function CodeBlock({ value, raw, max = "max-h-72" }: { value?: unknown; raw?: string; max?: string }) {
  const text = raw !== undefined ? prettyBody(raw).text : prettyJson(value);
  if (!text) return <p className="font-mono text-[11px] text-subtle">empty</p>;
  return (
    <div className="group relative">
      <pre className={`${max} overflow-auto rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[11px] leading-relaxed whitespace-pre-wrap break-all`}>
        {text}
      </pre>
      <span className="absolute top-1 right-1 opacity-0 transition group-hover:opacity-100">
        <CopyButton text={text} />
      </span>
    </div>
  );
}

export function Empty({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="px-5 py-10 text-center">
      <p className="text-[13px] text-muted">{title}</p>
      {children && <div className="mx-auto mt-1 max-w-sm text-[12px] text-subtle">{children}</div>}
    </div>
  );
}

export function Loading({ label = "Loading…" }: { label?: string }) {
  return (
    <p className="px-5 py-6 text-[13px] text-subtle">
      <span className="shimmer">{label}</span>
    </p>
  );
}

export function InlineError({ error }: { error: string | null }) {
  if (!error) return null;
  return <p className="rise px-5 py-2 font-mono text-[11px] text-danger">{error}</p>;
}

/** Segmented control for filters. */
export function Segmented<T extends string>({
  value,
  options,
  onChange,
}: {
  value: T;
  options: { value: T; label: string; count?: number }[];
  onChange: (v: T) => void;
}) {
  return (
    <div className="inline-flex flex-wrap rounded-lg border border-line p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          onClick={() => onChange(o.value)}
          className={`rounded-md px-2.5 py-1 text-[12px] transition ${
            o.value === value ? "bg-panel-2 text-fg" : "text-muted hover:text-fg"
          }`}
        >
          {o.label}
          {o.count != null && <span className="ml-1.5 font-mono text-[10px] text-subtle">{o.count}</span>}
        </button>
      ))}
    </div>
  );
}

/** Right-hand detail drawer. Escape or a click outside closes it. */
export function Drawer({ title, onClose, children, actions }: { title: ReactNode; onClose: () => void; children: ReactNode; actions?: ReactNode }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div className="fixed inset-0 z-40 flex justify-end bg-black/40" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <aside className="drawer-in flex h-full w-full max-w-2xl flex-col border-l border-line bg-panel">
        <div className="flex h-14 shrink-0 items-center gap-3 border-b border-line px-5">
          <div className="min-w-0 flex-1 truncate text-[14px] font-medium">{title}</div>
          {actions}
          <button onClick={onClose} className={iconBtn} aria-label="Close">
            <X className="size-4" strokeWidth={1.5} />
          </button>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto">{children}</div>
      </aside>
    </div>
  );
}

export function Section({ title, children, aside }: { title: string; children: ReactNode; aside?: ReactNode }) {
  return (
    <div className="border-b border-line px-5 py-4 last:border-0">
      <div className="mb-2 flex items-center justify-between">
        <p className="text-[12px] text-subtle">{title}</p>
        {aside}
      </div>
      {children}
    </div>
  );
}

/** Busy indicator for icon buttons: the mark thinking at 14px. */
export function Spinner() {
  return <HorizonMark state="thinking" size={14} interactive={false} />;
}

/** "Load more" footer for cursor pagination. */
export function LoadMore({ cursor, onMore, loading }: { cursor: string | null | undefined; onMore: () => void; loading: boolean }) {
  if (!cursor) return null;
  return (
    <div className="border-t border-line px-5 py-3 text-center">
      <button onClick={onMore} disabled={loading} className={btn}>
        {loading ? <span className="shimmer">Loading…</span> : "Load more"}
      </button>
    </div>
  );
}

/** Parse a JSON text field; empty is undefined. Throws a readable error. */
export function parseJsonInput(text: string, what = "data"): unknown {
  const t = text.trim();
  if (!t) return undefined;
  try {
    return JSON.parse(t);
  } catch {
    throw new Error(`${what} isn't valid JSON`);
  }
}

// --- detail drawers (messages, runs, events) can be opened from any tab ---

export type Detail = { kind: "message" | "run" | "event"; id: string };

const DetailCtx = createContext<(d: Detail | null) => void>(() => {});
export const DetailProvider = DetailCtx.Provider;

/** Open the message / run / event drawer. */
export function useOpenDetail() {
  return useContext(DetailCtx);
}

/** A mono id that opens its drawer. */
export function IdLink({ kind, id, className = "" }: Detail & { className?: string }) {
  const open = useOpenDetail();
  return (
    <button
      type="button"
      onClick={(e) => {
        e.stopPropagation();
        open({ kind, id });
      }}
      className={`truncate font-mono text-[11px] text-accent hover:underline ${className}`}
    >
      {id}
    </button>
  );
}
