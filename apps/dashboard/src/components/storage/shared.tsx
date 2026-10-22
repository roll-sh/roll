"use client";

import { Globe } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { storageApi, type Bucket, type StorageInfo } from "@/lib/storage-api";
import { POLL_MS } from "../redis/shared";

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
