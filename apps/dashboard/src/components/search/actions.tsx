import { SquareTerminal, Trash2 } from "lucide-react";
import { useState, type ReactNode } from "react";

/** Small pieces the search tabs' tables and forms share: one action at a time, an in-place
 * delete, an "open in console" button and a labelled checkbox. */

/** Runs one action at a time, keyed so its button can say so; errors land in `error`. */
export function useBusy(reload: () => Promise<void>) {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const act = async (key: string, f: () => Promise<unknown>, after = true) => {
    setBusy(key);
    setError(null);
    try {
      await f();
      if (after) await reload();
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setBusy(null);
    }
  };
  return { busy, error, setError, act };
}

/** A delete that asks once more in place: the first click arms it, the second acts. */
export function DeleteButton({ label, busy, onDelete }: { label: string; busy: boolean; onDelete: () => void }) {
  const [armed, setArmed] = useState(false);
  if (busy) return <span className="shimmer text-[12px] text-muted">Deleting…</span>;
  return armed ? (
    <button onClick={onDelete} onBlur={() => setArmed(false)} autoFocus className="rounded-md px-1.5 py-0.5 text-[12px] text-danger transition hover:bg-danger/10">
      Delete?
    </button>
  ) : (
    <button onClick={() => setArmed(true)} className="rounded-md p-1 text-subtle transition hover:text-danger" title={`Delete ${label}`} aria-label={`Delete ${label}`}>
      <Trash2 className="size-3.5" />
    </button>
  );
}

export function ConsoleButton({ text, onOpenConsole }: { text: string; onOpenConsole: (text: string) => void }) {
  return (
    <button onClick={() => onOpenConsole(text)} className="rounded-md p-1 text-subtle transition hover:text-fg" title="Open in console" aria-label="Open in console">
      <SquareTerminal className="size-3.5" />
    </button>
  );
}

export function Check({ checked, onChange, children }: { checked: boolean; onChange: (v: boolean) => void; children: ReactNode }) {
  return (
    <label className="flex items-center gap-2 text-[13px] text-muted">
      <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} className="accent-accent" />
      {children}
    </label>
  );
}
