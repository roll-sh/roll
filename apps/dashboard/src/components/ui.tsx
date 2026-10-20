"use client";

import { DotField, HorizonMark } from "@roll/brand";
import { Check, Copy, Eye, EyeOff } from "lucide-react";
import { useState, type ReactNode } from "react";
import { ROLL_API } from "@/lib/roll-api";

/** Shared building blocks of the dashboard pages (Postgres, Redis). */

export const btn =
  "inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted transition hover:border-line-2 hover:text-fg disabled:opacity-40";
export const primary =
  "inline-flex shrink-0 items-center gap-1.5 rounded-lg bg-fg px-3 py-1.5 text-[13px] font-medium text-bg transition hover:opacity-85 disabled:opacity-50";
export const danger =
  "inline-flex shrink-0 items-center gap-1.5 rounded-lg bg-danger px-3 py-1.5 text-[13px] font-medium text-white transition hover:opacity-85 disabled:opacity-40";
export const input =
  "rounded-lg border border-line bg-panel px-2.5 py-1.5 text-[13px] outline-none transition placeholder:text-subtle focus:border-line-2";
export const panel = "min-w-0 rounded-xl border border-line bg-panel";

export function ago(ts: number) {
  const s = Math.max(0, Math.floor(Date.now() / 1000) - ts);
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
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

/** A panel with the standard title row. */
export function Panel({
  title,
  aside,
  className = "",
  children,
}: {
  title: ReactNode;
  aside?: ReactNode;
  className?: string;
  children: ReactNode;
}) {
  return (
    <section className={`${panel} flex flex-col ${className}`}>
      <div className="flex min-h-14 items-center justify-between gap-3 border-b border-line px-5 py-3">
        <h2 className="text-[15px] font-medium">{title}</h2>
        {aside}
      </div>
      {children}
    </section>
  );
}

/** A secret-bearing value with reveal and copy. */
export function SecretField({
  id,
  value,
  masked,
  copied,
  onCopy,
}: {
  id: string;
  value: string;
  masked: string;
  copied: string | null;
  onCopy: (key: string, text: string) => void;
}) {
  const [reveal, setReveal] = useState(false);
  return (
    <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
      <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{reveal ? value : masked}</code>
      {masked !== value && (
        <button
          onClick={() => setReveal((r) => !r)}
          className="p-1 text-subtle hover:text-fg"
          title={reveal ? "Hide" : "Reveal"}
          aria-label={reveal ? "Hide" : "Reveal"}
        >
          {reveal ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
        </button>
      )}
      <CopyButton id={id} text={value} copied={copied} onCopy={onCopy} />
    </div>
  );
}

export function CopyButton({
  id,
  text,
  copied,
  onCopy,
}: {
  id: string;
  text: string;
  copied: string | null;
  onCopy: (key: string, text: string) => void;
}) {
  return (
    <button onClick={() => onCopy(id, text)} className="p-1 text-subtle hover:text-fg" title="Copy" aria-label="Copy">
      {copied === id ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
    </button>
  );
}

export function ErrorNote({ error }: { error: string | null }) {
  if (!error) return null;
  return (
    <p className="rise mt-4 rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[13px] text-danger">{error}</p>
  );
}

export function Connecting() {
  return (
    <p className="mt-10 text-[13px] text-subtle">
      <span className="shimmer">Connecting to rolld…</span>
    </p>
  );
}

/** A quiet hero panel over the dot field, for empty and offline states. */
export function EmptyPanel({ children }: { children: ReactNode }) {
  return (
    <section className="relative isolate mt-6 overflow-hidden rounded-xl border border-line bg-panel">
      <DotField spacing={18} radius={90} className="-z-10 text-fg" />
      <div className="flex flex-col items-start gap-3 p-8">{children}</div>
    </section>
  );
}

export function Offline() {
  return (
    <EmptyPanel>
      <HorizonMark state="sleep" size={40} />
      <h2 className="text-[15px] font-medium">rolld isn&apos;t running</h2>
      <p className="max-w-md text-[13px] text-muted">
        The dashboard talks to the local control plane at <code className="font-mono text-fg">{ROLL_API}</code>.
        Start it from the repo root (needs Docker):
      </p>
      <pre className="rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[12px]">cargo run -p rolld</pre>
    </EmptyPanel>
  );
}

/** Stat tiles in a hairline grid. Numbers are Geist Mono. */
export function Stats({
  items,
}: {
  items: { label: string; value: ReactNode; hint?: ReactNode; tone?: "ok" | "muted" }[];
}) {
  return (
    <div
      className={`grid grid-cols-2 gap-px overflow-hidden rounded-xl border border-line bg-line ${
        items.length >= 4 ? "lg:grid-cols-4" : "lg:grid-cols-3"
      }`}
    >
      {items.map((s) => (
        <div key={s.label} className="min-w-0 bg-panel p-5">
          <p className="text-[12px] text-subtle">{s.label}</p>
          <p className="mt-2 truncate font-mono text-xl font-semibold tracking-[-0.03em] tabular-nums">{s.value}</p>
          <div className={`mt-1 truncate text-[12px] ${s.tone === "ok" ? "text-ok" : "text-muted"}`}>{s.hint ?? " "}</div>
        </div>
      ))}
    </div>
  );
}

/** A thin fill meter (memory used vs limit, …). */
export function Meter({ value, max, title }: { value: number; max: number; title?: string }) {
  const pct = max > 0 ? Math.min(100, (value / max) * 100) : 0;
  return (
    <div className="h-1 overflow-hidden rounded-full bg-line" title={title}>
      <div
        className={`h-full rounded-full transition-[width] duration-500 motion-reduce:transition-none ${
          pct >= 90 ? "bg-danger" : "bg-accent"
        }`}
        style={{ width: `${pct}%` }}
      />
    </div>
  );
}

/** Two-step destructive confirmation: type the name to enable the action. */
export function Confirm({
  word,
  action,
  busy,
  onConfirm,
  onCancel,
  children,
}: {
  word: string;
  action: string;
  busy: boolean;
  onConfirm: () => void;
  onCancel: () => void;
  children: ReactNode;
}) {
  const [typed, setTyped] = useState("");
  return (
    <form
      className="rise space-y-3 rounded-lg border border-danger/40 bg-danger/5 p-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (typed === word) onConfirm();
      }}
    >
      <div className="text-[13px] text-muted">{children}</div>
      <label className="block space-y-1">
        <span className="block text-[11px] text-subtle">
          Type <span className="font-mono text-fg">{word}</span> to confirm
        </span>
        <input
          autoFocus
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          className={`${input} w-full font-mono text-[12px]`}
          spellCheck={false}
        />
      </label>
      <div className="flex gap-2">
        <button type="submit" disabled={busy || typed !== word} className={danger}>
          {busy ? <span className="shimmer">Working…</span> : action}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </form>
  );
}
