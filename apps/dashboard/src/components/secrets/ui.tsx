import { AtSign, BadgeCheck, Braces, Check, Clock, Copy, CornerDownRight, Eye, EyeOff, Import as ImportIcon, Link2, MessageSquare, Timer, User } from "lucide-react";
import { useReducedMotion } from "@roll/brand";
import { useWorkspace } from "@/lib/workspace-store";
import { useCallback, useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { ROLL_API } from "@/lib/roll-api";
import type { AuditEntry, Cell, Link, Problem, Secret, Totp } from "@/lib/secrets-api";
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
/** A multi-line field: `field` without the fixed height, which would win over `rows`. */
export const textareaField = field.replace("h-8 ", "h-auto py-1.5 ");
export const label = "mb-1 block text-[11px] font-medium text-subtle";

export const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** Saves `text` as a file the browser downloads (an export). */
export function saveText(name: string, text: string, type = "text/plain") {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  a.click();
  URL.revokeObjectURL(url);
}

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
const still = () => () => {};

/**
 * How the CLI signs in to this roll, before an empty state's CLI lines: the API as this browser
 * reaches it (relative to the page's origin), then `roll login`. None on the server, which has no
 * origin.
 */
function useCliLogin(): string[] {
  const api = useSyncExternalStore(still, () => new URL(ROLL_API, window.location.origin).href.replace(/\/$/, ""), () => null);
  return api ? [`export ROLL_API_URL=${api}`, "roll login"] : [];
}

/** `login`: the CLI lines start by signing in to this roll. */
export function Teach({ title, children, cli, login = false }: { title: string; children: ReactNode; cli?: string[]; login?: boolean }) {
  const signIn = useCliLogin();
  if (cli && login) cli = [...signIn, ...cli];
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
/** Where an inherited key comes from; with `onOpen`, a button to that import (or the base). */
export function SourceMark({ source, onOpen }: { source: Secret["source"] | Cell["source"]; onOpen?: () => void }) {
  const ws = useWorkspace();
  const kind = typeof source === "string" ? source : source.kind;
  if (kind === "own") return null;
  const other = typeof source !== "string" && source.kind === "import" && source.project ? `${ws.projects.find((p) => p.id === source.project)?.name ?? source.project} · ` : "";
  const from = typeof source === "string" ? "" : source.kind === "import" ? `${other}${source.environment}${source.path === "/" ? "" : source.path}` : source.kind === "base" ? source.environment : "";
  const title = kind === "import" ? `Imported${from ? ` from ${from}` : ""}: read-only here` : `Inherited from the base environment${from ? ` ${from}` : ""}`;
  const body = (
    <>
      {kind === "import" ? <ImportIcon className="size-3 shrink-0" /> : <CornerDownRight className="size-3 shrink-0" />}
      <span className="truncate">{from || kind}</span>
    </>
  );
  return onOpen ? (
    <button type="button" onClick={onOpen} title={title} aria-label={kind === "import" ? `Show the import from ${from}` : `Open the base, ${from}`} className="inline-flex min-w-0 items-center gap-1 rounded text-[11px] text-subtle transition-colors hover:text-accent hover:underline">
      {body}
    </button>
  ) : (
    <span title={title} className="inline-flex min-w-0 items-center gap-1 text-[11px] text-subtle">
      {body}
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
/** A value that stops working outside roll: quiet when far off, a count of days within two weeks, red once it has. */
export function ExpiryMark({ at, now = Date.now() }: { at: number; now?: number }) {
  const days = Math.ceil((at - now) / 86_400_000);
  const date = new Date(at).toISOString().slice(0, 10);
  if (at <= now)
    return (
      <span title={`Expired ${date}`} className="inline-flex shrink-0 items-center gap-0.5 font-mono text-[10.5px] text-danger">
        <Clock className="size-3" />
        expired
      </span>
    );
  return (
    <span title={`Expires ${date}`} className={`inline-flex shrink-0 items-center gap-0.5 font-mono text-[10.5px] ${days <= 14 ? "text-fg" : "text-subtle"}`}>
      <Clock className="size-3" />
      {days <= 14 && `${days}d`}
    </span>
  );
}

export const ActorMark = ({ who }: { who: string }) => (
  <span className="inline-flex min-w-0 items-center gap-1 truncate">
    <AtSign className="size-3 shrink-0 text-faint" />
    <span className="truncate">{who}</span>
  </span>
);

/** The approval that let a request through roll-access's gate (an `ask` approved, or a rule remembered for a while). */
export function ApprovalMark({ approval }: { approval: NonNullable<AuditEntry["approval"]> }) {
  const until = approval.until ? `, allowed until ${new Date(approval.until).toLocaleString()}` : ", this request only";
  return (
    <span className="inline-flex shrink-0 items-center gap-0.5 text-[10.5px] text-subtle" title={`Approved in ${approval.id}${until}`}>
      <BadgeCheck className="size-3" />
      approved{approval.by?.length ? ` by ${approval.by.join(", ")}` : ""}
    </span>
  );
}

/** How long a revealed value stays on screen. */
export const REVEAL_MS = 20_000;

/** Hides revealed values again (`hide`) `REVEAL_MS` after `shown` changes, and when the window loses focus or the tab hides. */
export function useMaskAgain(shown: unknown, hide: () => void) {
  const latest = useRef(hide);
  latest.current = hide;
  useEffect(() => {
    if (!shown) return;
    const go = () => latest.current();
    const timer = setTimeout(go, REVEAL_MS);
    const onVisibility = () => document.visibilityState === "hidden" && go();
    window.addEventListener("blur", go);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      clearTimeout(timer);
      window.removeEventListener("blur", go);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [shown]);
}

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

  useMaskAgain(shown, () => setShown(null));

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

/**
 * A TOTP key's current code (docs/secrets.md, "TOTP keys"): a click fetches it (audited as
 * `secrets.totp`) and shows it with a ring counting down its step. It hides when the step ends, and
 * again as a revealed value does (`useMaskAgain`). Reduced motion: the ring steps, it doesn't glide.
 */
export function TotpCode({ load }: { load: () => Promise<Totp> }) {
  const reduced = useReducedMotion();
  const [shown, setShown] = useState<Totp | null>(null);
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());

  useMaskAgain(shown, () => setShown(null));
  useEffect(() => {
    if (!shown) return;
    setNow(Date.now());
    const tick = setInterval(() => setNow(Date.now()), 1000);
    const end = setTimeout(() => setShown(null), Math.max(0, shown.expires_at - Date.now()));
    return () => {
      clearInterval(tick);
      clearTimeout(end);
    };
  }, [shown]);

  const show = async () => {
    if (shown) return setShown(null);
    setBusy(true);
    setError(null);
    try {
      setShown(await load());
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const copy = async () => {
    if (!shown) return;
    await navigator.clipboard?.writeText(shown.code);
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  };

  const left = shown ? Math.max(0, Math.ceil((shown.expires_at - now) / 1000)) : 0;
  const ring = 2 * Math.PI * 6;
  return (
    <span className="flex shrink-0 items-center gap-1">
      {shown ? (
        <>
          <button onClick={show} aria-label="Hide the code" title="Hide" className="rounded px-1 py-0.5 font-mono text-[12px] tracking-[0.08em] text-fg tabular-nums transition-colors hover:bg-fg/[0.04]">
            <span data-revealed>{shown.code}</span>
          </button>
          <svg viewBox="0 0 16 16" className="size-3.5 -rotate-90" role="img" aria-label={`${left} s left`}>
            <circle cx="8" cy="8" r="6" fill="none" strokeWidth="1.5" className="stroke-line-2" />
            <circle
              cx="8"
              cy="8"
              r="6"
              fill="none"
              strokeWidth="1.5"
              strokeLinecap="round"
              className={`stroke-accent ${reduced ? "" : "transition-[stroke-dashoffset] duration-1000 ease-linear"}`}
              strokeDasharray={ring}
              strokeDashoffset={ring * (1 - left / shown.period)}
            />
          </svg>
          <span className="w-6 font-mono text-[11px] text-subtle tabular-nums">{left}s</span>
          <button onClick={copy} className={iconButton} aria-label="Copy the code" title="Copy">
            {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
          </button>
        </>
      ) : (
        <button onClick={show} disabled={busy} className={`${ghost} h-6 px-1.5`} aria-label="Show the one-time code" title="The current one-time code (audited)">
          <Timer className="size-3.5" strokeWidth={1.5} />
          {busy ? <span className="shimmer">code…</span> : "code"}
        </button>
      )}
      {error && <span className="text-[11px] text-danger">{error}</span>}
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
