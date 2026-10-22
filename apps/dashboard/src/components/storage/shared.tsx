"use client";

import { Globe } from "lucide-react";
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { storageApi, type Bucket, type StorageInfo } from "@/lib/storage-api";
import { POLL_MS } from "../redis/shared";
import { btn, primary } from "../ui";

export { Field, Segmented, useAction, type Run } from "../redis/shared";

/** Poll the control plane: storage info and every bucket. */
export function useStorage() {
  const [info, setInfo] = useState<StorageInfo | null>(null);
  const [buckets, setBuckets] = useState<Bucket[] | null>(null);
  const [offline, setOffline] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const [i, b] = await Promise.all([storageApi.info(), storageApi.buckets()]);
      setInfo(i);
      setBuckets(b);
      setOffline(false);
    } catch {
      setOffline(true);
    }
  }, []);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      await refresh();
      if (alive) timer = setTimeout(loop, POLL_MS * 2);
    };
    loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [refresh]);

  return { info, buckets, offline, refresh };
}

/** Tabs kept in the URL hash, like the Redis pages. */
export function useHashTab<T extends string>(tabs: readonly T[], initial: T) {
  const [tab, setTabRaw] = useState<T>(initial);
  useEffect(() => {
    const h = window.location.hash.slice(1) as T;
    if (tabs.includes(h)) setTabRaw(h);
  }, [tabs]);
  const setTab = (t: T) => {
    setTabRaw(t);
    window.history.replaceState(null, "", t === initial ? window.location.pathname : `#${t}`);
  };
  return [tab, setTab] as const;
}

export function TabNav<T extends string>({
  tabs,
  tab,
  onChange,
  label,
  counts = {},
}: {
  tabs: readonly { value: T; label: string }[];
  tab: T;
  onChange: (t: T) => void;
  label: string;
  counts?: Partial<Record<T, number>>;
}) {
  return (
    <nav className="mt-6 flex gap-1 overflow-x-auto border-b border-line" aria-label={label}>
      {tabs.map((t) => (
        <button
          key={t.value}
          onClick={() => onChange(t.value)}
          aria-current={t.value === tab ? "page" : undefined}
          className={`-mb-px shrink-0 border-b px-3 py-2 text-[13px] transition ${
            t.value === tab ? "border-fg text-fg" : "border-transparent text-muted hover:text-fg"
          }`}
        >
          {t.label}
          {!!counts[t.value] && <span className="ml-1.5 font-mono text-[11px] text-subtle">{counts[t.value]}</span>}
        </button>
      ))}
    </nav>
  );
}

export function PublicBadge() {
  return (
    <span
      className="inline-flex items-center gap-1 rounded-md border border-line px-1.5 py-px text-[10px] text-muted"
      title="Objects can be read without credentials"
    >
      <Globe className="size-3" /> public
    </span>
  );
}

/** The footer of a settings form: whether there are unsaved changes, reset and save. */
export function SaveBar({ dirty, busy, onReset, label = "Save" }: { dirty: boolean; busy: boolean; onReset: () => void; label?: string }) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line pt-4">
      <span className="text-[12px] text-subtle">{dirty ? "Unsaved changes" : "Up to date"}</span>
      <div className="flex gap-2">
        {dirty && (
          <button type="button" onClick={onReset} className={btn}>
            Reset
          </button>
        )}
        <button type="submit" disabled={busy || !dirty} className={primary}>
          {busy ? <span className="shimmer">Saving…</span> : label}
        </button>
      </div>
    </div>
  );
}

/** A labelled checkbox with a one-line explanation. */
export function Check({
  checked,
  onChange,
  label,
  hint,
  disabled,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: string;
  hint?: string;
  disabled?: boolean;
}) {
  return (
    <label className={`flex items-start gap-2 text-[13px] ${disabled ? "opacity-60" : "cursor-pointer"}`}>
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
        className="mt-0.5 accent-accent"
      />
      <span className="min-w-0">
        <span className="block">{label}</span>
        {hint && <span className="block text-[12px] text-subtle">{hint}</span>}
      </span>
    </label>
  );
}

/** A small toggle chip (event types, methods). */
export function Chip({ on, onClick, children, title }: { on: boolean; onClick: () => void; children: ReactNode; title?: string }) {
  return (
    <button
      type="button"
      aria-pressed={on}
      title={title}
      onClick={onClick}
      className={`rounded-md border px-2 py-1 font-mono text-[11px] transition ${
        on ? "border-line-2 bg-panel-2 text-fg" : "border-line text-subtle hover:text-fg"
      }`}
    >
      {children}
    </button>
  );
}

/** Two-step confirmation for a small, reversible-ish action inside a row. */
export function InlineConfirm({
  label,
  busy,
  onConfirm,
  onCancel,
  danger = true,
}: {
  label: string;
  busy: boolean;
  onConfirm: () => void;
  onCancel: () => void;
  danger?: boolean;
}) {
  return (
    <span className="flex shrink-0 gap-1.5">
      <button
        type="button"
        disabled={busy}
        onClick={onConfirm}
        className={
          danger
            ? "inline-flex items-center gap-1.5 rounded-lg bg-danger px-2.5 py-1.5 text-[12px] font-medium text-white transition hover:opacity-85 disabled:opacity-40"
            : primary
        }
      >
        {busy ? <span className="shimmer">Working…</span> : label}
      </button>
      <button type="button" onClick={onCancel} className={btn}>
        Cancel
      </button>
    </span>
  );
}

/** Thin progress bar (uploads). */
export function Progress({ value }: { value: number }) {
  return (
    <div className="h-1 overflow-hidden rounded-full bg-line">
      <div
        className="h-full rounded-full bg-accent transition-[width] duration-200 ease-out motion-reduce:transition-none"
        style={{ width: `${Math.min(100, Math.max(0, value * 100))}%` }}
      />
    </div>
  );
}
