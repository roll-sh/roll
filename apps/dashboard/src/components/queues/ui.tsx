"use client";

import { HorizonMark } from "@roll/brand";
import { Check, Copy, X } from "lucide-react";
import type { ReactNode } from "react";
import { useCopy } from "./hooks";

export const btn =
  "inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted transition hover:border-line-2 hover:text-fg disabled:pointer-events-none disabled:opacity-40";
export const primary =
  "inline-flex shrink-0 items-center gap-1.5 rounded-lg bg-fg px-3 py-1.5 text-[13px] font-medium text-bg transition hover:opacity-85 disabled:opacity-50";
export const input =
  "rounded-lg border border-line bg-panel px-2.5 py-1.5 text-[13px] outline-none transition placeholder:text-subtle focus:border-line-2";
export const iconBtn =
  "rounded-md p-1.5 text-subtle transition hover:bg-panel-2 hover:text-fg disabled:pointer-events-none disabled:opacity-40";
export const dangerIconBtn =
  "rounded-md p-1.5 text-subtle transition hover:bg-panel-2 hover:text-danger disabled:pointer-events-none disabled:opacity-40";
export const th = "border-b border-line px-4 py-2 text-left text-[11px] font-medium text-subtle whitespace-nowrap";
export const td = "border-b border-line px-4 py-2.5 align-middle";
export const num = "font-mono text-[12px] tabular-nums";

export const ICON = { className: "size-3.5", strokeWidth: 1.5 } as const;

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
    <section className={`flex min-w-0 flex-col overflow-hidden rounded-2xl border border-line bg-panel ${className}`}>
      {(title || actions || meta) && (
        <div className="flex min-h-[3.25rem] flex-wrap items-center gap-x-3 gap-y-2 border-b border-line px-5 py-3">
          {title && <h2 className="text-[15px] font-medium">{title}</h2>}
          {meta && <span className="text-[12px] text-subtle">{meta}</span>}
          {actions && <div className="ml-auto flex flex-wrap items-center gap-1.5">{actions}</div>}
        </div>
      )}
      {children}
    </section>
  );
}

export interface StatItem {
  label: string;
  value: ReactNode;
  hint?: ReactNode;
}

export function StatGrid({ stats, cols = 4 }: { stats: StatItem[]; cols?: 3 | 4 | 5 }) {
  const lg = cols === 5 ? "lg:grid-cols-5" : cols === 3 ? "lg:grid-cols-3" : "lg:grid-cols-4";
  return (
    <div className={`grid grid-cols-2 gap-px overflow-hidden rounded-2xl border border-line bg-line ${lg}`}>
      {stats.map((s) => (
        <div key={s.label} className="min-w-0 bg-panel p-5">
          <p className="truncate text-[12px] text-subtle">{s.label}</p>
          <p className="mt-2 truncate font-mono text-2xl font-semibold tracking-[-0.04em] tabular-nums">{s.value}</p>
          {s.hint !== undefined && <p className="mt-1 truncate text-[12px] text-muted">{s.hint}</p>}
        </div>
      ))}
    </div>
  );
}

export function Badge({ children, tone = "muted", title }: { children: ReactNode; tone?: "muted" | "accent" | "danger" | "ok"; title?: string }) {
  const cls =
    tone === "accent"
      ? "border-accent/40 text-accent"
      : tone === "danger"
        ? "border-danger/40 text-danger"
        : tone === "ok"
          ? "border-ok/40 text-ok"
          : "border-line-2 text-muted";
  return (
    <span title={title} className={`inline-flex shrink-0 items-center rounded-full border px-1.5 py-px font-mono text-[10px] leading-4 ${cls}`}>
      {children}
    </span>
  );
}

export function StateDot({ state }: { state: "running" | "sleeping" | "busy" | "off" }) {
  const cls =
    state === "running"
      ? "bg-ok"
      : state === "busy"
        ? "bg-accent animate-pulse motion-reduce:animate-none"
        : "border border-line-2 bg-transparent";
  return <span className={`inline-block size-2 shrink-0 rounded-full ${cls}`} />;
}

export function ErrorNote({ error, onClose }: { error: string | null | undefined; onClose?: () => void }) {
  if (!error) return null;
  return (
    <p className="rise flex items-start gap-2 rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[13px] text-danger">
      <span className="min-w-0 flex-1 break-words">{error}</span>
      {onClose && (
        <button onClick={onClose} aria-label="Dismiss" className="shrink-0 opacity-70 hover:opacity-100">
          <X {...ICON} />
        </button>
      )}
    </p>
  );
}

export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const { copied, copy } = useCopy();
  return (
    <button onClick={() => copy("v", text)} className="p-1 text-subtle transition hover:text-fg" title={label} aria-label={label}>
      {copied ? <Check {...ICON} className="size-3.5 text-ok" /> : <Copy {...ICON} />}
    </button>
  );
}

export function CopyField({ value, className = "" }: { value: string; className?: string }) {
  return (
    <div className={`flex min-w-0 items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-1.5 ${className}`}>
      <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{value}</code>
      <CopyButton text={value} />
    </div>
  );
}

/** Spinner-free pending indicator: the logo thinking at icon size. */
export function Busy({ size = 14 }: { size?: number }) {
  return <HorizonMark state="thinking" size={size} interactive={false} label="working" />;
}

export function Pending({ children }: { children: ReactNode }) {
  return <span className="shimmer">{children}</span>;
}

export function Empty({ children }: { children: ReactNode }) {
  return <p className="px-5 py-8 text-center text-[13px] text-subtle">{children}</p>;
}

export function Field({ label, hint, children, className = "" }: { label: string; hint?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <label className={`block min-w-0 space-y-1 ${className}`}>
      <span className="block text-[11px] text-subtle">{label}</span>
      {children}
      {hint && <span className="block text-[11px] text-subtle">{hint}</span>}
    </label>
  );
}

export function Segmented<T extends string>({
  value,
  options,
  onChange,
  disabled,
}: {
  value: T;
  options: readonly (T | { value: T; label: string })[];
  onChange: (v: T) => void;
  disabled?: boolean;
}) {
  return (
    <div className="inline-flex rounded-lg border border-line p-0.5">
      {options.map((o) => {
        const v = typeof o === "string" ? o : o.value;
        const label = typeof o === "string" ? o : o.label;
        return (
          <button
            key={v}
            type="button"
            disabled={disabled}
            onClick={() => onChange(v)}
            className={`rounded-md px-2.5 py-1 text-[12px] capitalize transition disabled:opacity-40 ${
              v === value ? "bg-panel-2 text-fg" : "text-muted hover:text-fg"
            }`}
          >
            {label}
          </button>
        );
      })}
    </div>
  );
}

export function Toggle({
  checked,
  onChange,
  label,
  disabled,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: ReactNode;
  disabled?: boolean;
}) {
  return (
    <label className={`inline-flex cursor-pointer items-center gap-2 text-[12px] text-muted select-none ${disabled ? "opacity-40" : ""}`}>
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        disabled={disabled}
        onClick={() => onChange(!checked)}
        className={`relative h-4 w-7 shrink-0 rounded-full border transition ${checked ? "border-fg bg-fg" : "border-line-2 bg-panel-2"}`}
      >
        <span
          className={`absolute top-1/2 size-2.5 -translate-y-1/2 rounded-full transition-[left] duration-200 ease-out ${
            checked ? "left-[13px] bg-bg" : "left-[2px] bg-subtle"
          }`}
        />
      </button>
      {label}
    </label>
  );
}

/** Small pill buttons, e.g. suggested argument keys. */
export function Chips({ items, onPick }: { items: { key: string; title?: string }[]; onPick: (key: string) => void }) {
  return (
    <div className="flex flex-wrap gap-1">
      {items.map((s) => (
        <button
          key={s.key}
          type="button"
          title={s.title}
          onClick={() => onPick(s.key)}
          className="rounded-full border border-line px-2 py-0.5 font-mono text-[10px] text-muted transition hover:border-line-2 hover:text-fg"
        >
          + {s.key}
        </button>
      ))}
    </div>
  );
}

export function VhostSelect({
  value,
  vhosts,
  onChange,
  all,
  className = "",
}: {
  value: string;
  vhosts: string[];
  onChange: (v: string) => void;
  /** Label of an extra "every vhost" option whose value is the empty string. */
  all?: string;
  className?: string;
}) {
  return (
    <select value={value} onChange={(e) => onChange(e.target.value)} className={`${input} font-mono text-[12px] ${className}`}>
      {all && <option value="">{all}</option>}
      {vhosts.map((v) => (
        <option key={v} value={v}>
          vhost {v}
        </option>
      ))}
    </select>
  );
}

/** Compact JSON for table cells. */
export function JsonInline({ value }: { value: Record<string, unknown> | null | undefined }) {
  const entries = Object.entries(value ?? {});
  if (!entries.length) return <span className="text-subtle">—</span>;
  return (
    <span className="font-mono text-[11px] text-muted">
      {entries.map(([k, v], i) => (
        <span key={k}>
          {i > 0 && ", "}
          {k}=<span className="text-fg">{typeof v === "string" ? v : JSON.stringify(v)}</span>
        </span>
      ))}
    </span>
  );
}
