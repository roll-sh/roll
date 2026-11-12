import { AtSign, Braces, Check, Copy, CornerDownRight, Eye, EyeOff, Import as ImportIcon, Link2, MessageSquare, User } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import type { Cell, Link, Problem, Secret } from "@/lib/secrets-api";
import { KIND_ICON } from "../workspace/kind";
import type { ResourceKind } from "@/lib/workspace-api";

/** The Secrets surface's controls, on the dashboard frame's tokens (docs/code/agents-design.md). */
export const button =
  "inline-flex h-7 shrink-0 items-center gap-1.5 rounded-md border border-line-2 bg-raised px-2.5 text-[12px] font-medium inset-shadow-highlight transition-[border-color,transform] duration-150 hover:border-line-3 active:scale-[0.97] disabled:pointer-events-none disabled:opacity-40 motion-reduce:transition-none";
export const primary =
  "inline-flex h-7 shrink-0 items-center gap-1.5 rounded-md bg-fg px-2.5 text-[12px] font-medium text-bg shadow-card inset-shadow-highlight transition-[opacity,transform] duration-150 hover:opacity-90 active:scale-[0.97] disabled:pointer-events-none disabled:opacity-40 motion-reduce:transition-none";
export const ghost =
  "inline-flex h-7 shrink-0 items-center gap-1.5 rounded-md px-2 text-[12px] text-muted transition-colors duration-150 hover:bg-fg/[0.05] hover:text-fg disabled:pointer-events-none disabled:opacity-40";
export const dangerButton =
  "inline-flex h-7 shrink-0 items-center gap-1.5 rounded-md border border-danger/50 px-2.5 text-[12px] text-danger transition-colors hover:bg-danger/10 disabled:pointer-events-none disabled:opacity-40";
export const iconButton =
  "grid size-6 shrink-0 place-items-center rounded-md text-muted transition-colors duration-150 hover:bg-fg/[0.06] hover:text-fg disabled:pointer-events-none disabled:opacity-40";
export const field =
  "h-8 w-full min-w-0 rounded-md border border-line-2 bg-panel-2 px-2.5 text-[12.5px] text-fg outline-none transition-colors placeholder:text-subtle focus:border-accent/60";
/** A field with its own width (`w-40`): `field` without `w-full`, which would win. */
export const sized = field.replace(" w-full", "");
export const label = "mb-1 block text-[11px] font-medium text-subtle";

export const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** What a load is: its data, its error, and a way to load it again. */
export function useLoad<T>(load: () => Promise<T>, deps: unknown[]) {
  const [state, setState] = useState<{ data: T | null; error: string | null; loading: boolean }>({ data: null, error: null, loading: true });
  const seq = useRef(0);
  const reload = useCallback(() => {
    const n = ++seq.current;
    setState((s) => ({ ...s, loading: true }));
    load().then(
      (data) => n === seq.current && setState({ data, error: null, loading: false }),
      (e) => n === seq.current && setState((s) => ({ data: s.data, error: errorText(e), loading: false })),
    );
  }, deps);
  useEffect(() => {
    reload();
    return () => {
      seq.current++;
    };
  }, [reload]);
  const set = (data: T) => setState({ data, error: null, loading: false });
  return { ...state, reload, set };
}

/** An API error where it happened, never swallowed; dismissable when `onDismiss` is given. */
export function ErrorNote({ error, onDismiss, className = "" }: { error: string | null; onDismiss?: () => void; className?: string }) {
  if (!error) return null;
  return (
    <div role="alert" className={`fade-in flex items-start gap-2 rounded-lg border border-danger/40 bg-danger/[0.07] px-3 py-2 text-[12px] text-danger ${className}`}>
      <span className="min-w-0 flex-1 break-words">{error}</span>
      {onDismiss && (
        <button onClick={onDismiss} className="shrink-0 text-danger/70 hover:text-danger" aria-label="Dismiss">
          ×
        </button>
      )}
    </div>
  );
}

/** Warnings from a write (a reference to a key that doesn't exist, a cycle) or a read's problems. */
export function Warnings({ items, className = "" }: { items: string[]; className?: string }) {
  if (!items.length) return null;
  return (
    <ul className={`fade-in space-y-0.5 rounded-lg border border-line-2 bg-panel-2 px-3 py-2 text-[11.5px] text-soft ${className}`}>
      {items.map((w) => (
        <li key={w} className="flex gap-1.5">
          <Braces className="mt-0.5 size-3 shrink-0 text-muted" />
          {w}
        </li>
      ))}
    </ul>
  );
}

/** An empty state that teaches: what goes here, and the CLI way to do it. */
export function Teach({ title, children, cli }: { title: string; children: ReactNode; cli?: string[] }) {
  return (
    <div className="rise mx-auto w-full max-w-[520px] px-4 py-14 text-center">
      <h2 className="text-[18px] font-semibold tracking-[-0.045em]">{title}</h2>
      <div className="mt-1.5 text-[12.5px] text-muted">{children}</div>
      {cli && (
        <pre className="mt-4 overflow-x-auto rounded-lg border border-line-2 bg-panel px-3 py-2.5 text-left font-mono text-[11.5px] leading-relaxed text-soft inset-shadow-highlight">
          {cli.join("\n")}
        </pre>
      )}
    </div>
  );
}

export function ago(ms: number, now = Date.now()) {
  const s = Math.max(0, Math.floor((now - ms) / 1000));
  if (s < 45) return "just now";
  if (s < 3600) return `${Math.max(1, Math.round(s / 60))}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  const d = Math.round(s / 86400);
  return d < 60 ? `${d}d ago` : new Date(ms).toLocaleDateString("en-US", { month: "short", day: "numeric", year: "numeric" });
}

/** Until a time: "in 42m", "in 3d", or "expired". */
export function until(ms: number, now = Date.now()) {
  const s = Math.floor((ms - now) / 1000);
  if (s <= 0) return "expired";
  if (s < 3600) return `in ${Math.max(1, Math.round(s / 60))}m`;
  if (s < 86400) return `in ${Math.round(s / 3600)}h`;
  return `in ${Math.round(s / 86400)}d`;
}

export const stamp = (ms: number) =>
  new Date(ms).toLocaleString("en-US", { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", hour12: false });

const MODE_HINT: Record<Link["mode"], string> = {
  live: "read from the resource on every resolve",
  managed: "a credential roll issued, rotatable",
  dynamic: "a fresh short-lived credential on every resolve",
};

/** A link as a quiet chip: the resource kind's icon, the resource, the field and the mode. */
export function LinkChip({ link, name, compact = false }: { link: Link; name?: string; compact?: boolean }) {
  const Icon = KIND_ICON[link.kind as ResourceKind] ?? Link2;
  return (
    <span
      title={`Linked to ${link.kind} ${name ?? link.ref} · ${link.field} · ${link.mode}: ${MODE_HINT[link.mode]}`}
      className="inline-flex min-w-0 items-center gap-1 rounded-md border border-line-2 bg-panel-2 px-1.5 py-px text-[11px] text-soft"
    >
      <Icon className="size-3 shrink-0 text-muted" />
      {!compact && (
        <>
          <span className="truncate">{name ?? link.ref}</span>
          <span className="text-subtle">·</span>
          <span className="truncate font-mono text-[10.5px]">{link.field}</span>
        </>
      )}
      <span className={`shrink-0 font-mono text-[10px] ${link.mode === "live" ? "text-subtle" : "text-muted"}`}>{link.mode}</span>
    </span>
  );
}

/** Where a key comes from when it isn't the folder's own. */
export function SourceMark({ source }: { source: Secret["source"] | Cell["source"] }) {
  const kind = typeof source === "string" ? source : source.kind;
  if (kind === "own") return null;
  const from = typeof source === "string" ? "" : source.kind === "import" ? `${source.environment}${source.path === "/" ? "" : source.path}` : source.kind === "base" ? source.environment : "";
  return kind === "import" ? (
    <span title={`Imported${from ? ` from ${from}` : ""}: read-only here`} className="inline-flex items-center gap-1 text-[11px] text-subtle">
      <ImportIcon className="size-3" />
      {from || "import"}
    </span>
  ) : (
    <span title={`Inherited from the base environment${from ? ` ${from}` : ""}`} className="inline-flex items-center gap-1 text-[11px] text-subtle">
      <CornerDownRight className="size-3" />
      {from || "base"}
    </span>
  );
}

export const RefMark = ({ refs }: { refs?: string[] }) => (
  <span title={refs?.length ? `References ${refs.join(", ")}` : "Has references"} className="inline-flex items-center text-muted">
    <Braces className="size-3" />
  </span>
);
export const PersonalMark = ({ title = "Your personal override replaces the shared value for you" }: { title?: string }) => (
  <span title={title} className="inline-flex items-center gap-0.5 text-[11px] text-accent">
    <User className="size-3" />
  </span>
);
export const CommentMark = ({ text }: { text?: string }) => (
  <span title={text || "Has a comment"} className="inline-flex items-center text-subtle">
    <MessageSquare className="size-3" />
  </span>
);
export const ActorMark = ({ who }: { who: string }) => (
  <span className="inline-flex min-w-0 items-center gap-1 truncate">
    <AtSign className="size-3 shrink-0 text-faint" />
    <span className="truncate">{who}</span>
  </span>
);

/** How long a revealed value stays on screen. */
export const REVEAL_MS = 20_000;

/**
 * A secret's value, masked until asked for. A click fetches it (an audited reveal) and shows it;
 * it masks again after `REVEAL_MS`, when the window loses focus or the tab hides, and when the
 * row goes. The value lives only in this component's state while it's shown. Copy fetches it
 * without showing it.
 */
export function Reveal({ load, className = "" }: { load: () => Promise<{ value: string; problems?: Problem[] }>; className?: string }) {
  const [shown, setShown] = useState<{ value: string; problems: Problem[] } | null>(null);
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!shown) return;
    const hide = () => setShown(null);
    const timer = setTimeout(hide, REVEAL_MS);
    const onVisibility = () => document.visibilityState === "hidden" && hide();
    window.addEventListener("blur", hide);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      clearTimeout(timer);
      window.removeEventListener("blur", hide);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [shown]);

  const fetchValue = async () => {
    setBusy(true);
    setError(null);
    try {
      return await load();
    } catch (e) {
      setError(errorText(e));
      return null;
    } finally {
      setBusy(false);
    }
  };
  const toggle = async () => {
    if (shown) return setShown(null);
    const v = await fetchValue();
    if (v) setShown({ value: v.value, problems: v.problems ?? [] });
  };
  const copy = async () => {
    const v = shown ?? (await fetchValue());
    if (!v) return;
    await navigator.clipboard?.writeText(v.value);
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  };

  return (
    <span className={`group/reveal flex min-w-0 items-start gap-1 ${className}`}>
      <button
        onClick={toggle}
        aria-label={shown ? "Hide the value" : "Reveal the value"}
        className={`min-w-0 flex-1 overflow-hidden rounded px-1 py-0.5 text-left font-mono text-[12px] whitespace-nowrap transition-colors hover:bg-fg/[0.04] ${shown ? "text-fg" : "text-subtle"}`}
      >
        {shown ? (
          <span data-revealed className="block max-h-40 overflow-auto break-all whitespace-pre-wrap">
            {shown.value === "" ? <span className="text-subtle italic">empty</span> : shown.value}
          </span>
        ) : busy ? (
          <span className="shimmer">revealing…</span>
        ) : (
          <span aria-hidden className="tracking-[0.12em]">
            ••••••••••
          </span>
        )}
      </button>
      <button onClick={toggle} className={`${iconButton} shrink-0`} aria-hidden tabIndex={-1} title={shown ? "Hide" : "Reveal (audited)"}>
        {shown ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
      </button>
      <button onClick={copy} className={`${iconButton} shrink-0`} aria-label="Copy the value" title="Copy (audited)">
        {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
      </button>
      {error && <span className="shrink-0 text-[11px] text-danger">{error}</span>}
      {shown && shown.problems.length > 0 && (
        <span className="shrink-0 text-[11px] text-danger" title={shown.problems.map((p) => `${p.key}: ${p.problem}`).join("\n")}>
          {shown.problems.length} unresolved
        </span>
      )}
    </span>
  );
}

/** A selectable chip (environments and actions in a token's scope). */
export function Chip({ on, onClick, children, title }: { on: boolean; onClick: () => void; children: ReactNode; title?: string }) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={on}
      title={title}
      className={`inline-flex h-6 items-center gap-1 rounded-md border px-2 text-[11.5px] transition-colors ${
        on ? "border-accent/50 bg-accent/[0.08] text-fg" : "border-line-2 text-muted hover:border-line-3 hover:text-fg"
      }`}
    >
      {children}
    </button>
  );
}

/** A section heading inside a view. */
export function Section({ title, aside, children, className = "" }: { title: ReactNode; aside?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <section className={className}>
      <div className="mb-2 flex min-h-7 items-center gap-2">
        <h3 className="text-[13px] font-semibold tracking-[-0.015em]">{title}</h3>
        <span className="ml-auto flex items-center gap-1.5">{aside}</span>
      </div>
      {children}
    </section>
  );
}

/** A flat card for lists (`bg-panel`, hairline, the highlight). */
export const card = "rounded-xl border border-line-2 bg-panel inset-shadow-highlight";
