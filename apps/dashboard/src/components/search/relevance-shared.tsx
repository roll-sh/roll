import { ArrowLeft, RefreshCw } from "lucide-react";
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { btn } from "../ui";

/** Pieces the Relevance tab's synonym and query-rule views share. */

export const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** A list's empty state: what it is and why you'd want one. */
export function Empty({ title, children, action }: { title: string; children: ReactNode; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-start gap-2 px-5 py-8">
      <p className="text-[14px] font-medium">{title}</p>
      <p className="max-w-xl text-[13px] text-muted">{children}</p>
      {action && <div className="mt-2">{action}</div>}
    </div>
  );
}

/** A list that failed to load, with a retry. */
export function LoadError({ error, onRetry }: { error: string; onRetry: () => void }) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
      <p className="text-[13px] text-danger">{error}</p>
      <button onClick={onRetry} className={btn}>
        <RefreshCw className="size-3.5" /> Retry
      </button>
    </div>
  );
}

/** The back link above an opened set or ruleset. */
export function BackTo({ onBack }: { onBack: () => void }) {
  return (
    <button onClick={onBack} className="inline-flex items-center gap-1.5 text-[13px] text-muted transition hover:text-fg">
      <ArrowLeft className="size-3.5" /> Relevance
    </button>
  );
}

/** Loads `load` on mount and on `reload`; `value` is null while loading. */
export function useLoad<T>(load: () => Promise<T>) {
  const [value, setValue] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const reload = useCallback(() => {
    setError(null);
    load().then(setValue, (e) => setError(errText(e)));
  }, [load]);
  useEffect(reload, [reload]);
  return { value, error, reload, setValue };
}
