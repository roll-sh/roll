import { CornerDownRight, Folder, KeyRound, Layers } from "lucide-react";
import { useCallback, useEffect, useId, useMemo, useRef, useState, type TextareaHTMLAttributes } from "react";
import { accept, check, suggest, typing, type Listing, type Lookup, type Suggestion } from "@/lib/secrets-refs";
import { glass } from "../agents/ui/cn";
import { useSecrets } from "./secrets-page";

/** What the value editors complete `${` from: this folder's keys, the other environments, and their folders as they load. */
export type RefSource = { here: string[]; envs: string[]; lookup: Lookup; load: (env: string, path: string) => void };

/**
 * The folders references are completed and checked against, loaded lazily (each environment's
 * folder once, through `…/secrets?path=` and `…/folders?path=`) and kept while the environment
 * view is open.
 */
export function useRefSource(env: string, here: Set<string>): RefSource {
  const { api, envs } = useSecrets();
  const [cache, setCache] = useState<Map<string, Listing>>(() => new Map());
  const asked = useRef(new Set<string>());
  useEffect(() => {
    asked.current = new Set();
    setCache(new Map());
  }, [api]);
  const load = useCallback(
    (e: string, path: string) => {
      const k = `${e}:${path}`;
      if (asked.current.has(k)) return;
      asked.current.add(k);
      Promise.all([api.secrets(e, path), api.folders(e, path)]).then(
        ([secrets, folders]) => setCache((c) => new Map(c).set(k, { keys: [...new Set(secrets.map((s) => s.key))], folders: folders.folders.map((f) => f.path.split("/").pop() ?? f.name) })),
        // A folder that isn't there (or can't be read) completes to nothing.
        () => setCache((c) => new Map(c).set(k, { keys: [], folders: [] })),
      );
    },
    [api],
  );
  const lookup = useCallback((e: string, path: string) => cache.get(`${e}:${path}`), [cache]);
  const others = useMemo(() => envs.map((e) => e.slug).filter((s) => s !== env), [envs, env]);
  const keys = useMemo(() => [...here], [here]);
  return useMemo(() => ({ here: keys, envs: others, lookup, load }), [keys, others, lookup, load]);
}

/** Warnings about a draft's references (missing keys, itself, `${…}` that stays text), loading the folders it names. */
export function useRefWarnings(value: string, self: string, source: RefSource, on = true): string[] {
  const here = useMemo(() => new Set(source.here), [source.here]);
  const { warnings, need } = useMemo(() => (on ? check(value, self, here, source.lookup) : { warnings: [], need: [] }), [on, value, self, here, source.lookup]);
  useEffect(() => need.forEach((n) => source.load(n.env, n.path)), [need, source]);
  return warnings;
}

const ICON = { key: KeyRound, env: Layers, folder: Folder } as const;

/** A value's textarea where `${` opens completions: ↑/↓ to move, Enter or Tab to accept, Esc to close. */
export function RefInput({ value, onValue, source, ...props }: { value: string; onValue: (v: string) => void; source: RefSource } & Omit<TextareaHTMLAttributes<HTMLTextAreaElement>, "value" | "onChange">) {
  const ref = useRef<HTMLTextAreaElement>(null);
  const list = useId();
  const [caret, setCaret] = useState<number | null>(null);
  const [active, setActive] = useState(0);
  const [dismissed, setDismissed] = useState<number | null>(null);
  const at = caret === null ? null : typing(value, caret);
  const found = useMemo(() => (at ? suggest(at.query, source.here, source.envs, source.lookup) : { items: [] as Suggestion[] }), [at?.query, source]);
  const need = found.need;
  useEffect(() => {
    if (need) source.load(need.env, need.path);
  }, [need?.env, need?.path, source]);
  const open = !!at && at.start !== dismissed && (found.items.length > 0 || !!need);
  const items = found.items.slice(0, 50);
  const pick = Math.min(active, Math.max(0, items.length - 1));
  useEffect(() => setActive(0), [at?.query]);

  const sync = () => setCaret(ref.current?.selectionStart ?? null);
  const take = (s: Suggestion) => {
    if (caret === null) return;
    const next = accept(value, caret, s);
    onValue(next.value);
    setCaret(next.caret);
    requestAnimationFrame(() => {
      ref.current?.focus();
      ref.current?.setSelectionRange(next.caret, next.caret);
    });
  };

  return (
    <div className="relative">
      <textarea
        {...props}
        ref={ref}
        value={value}
        role="combobox"
        aria-expanded={open}
        aria-controls={open ? list : undefined}
        aria-autocomplete="list"
        aria-activedescendant={open && items.length ? `${list}-${pick}` : undefined}
        onChange={(e) => {
          onValue(e.target.value);
          setCaret(e.target.selectionStart);
          setDismissed(null);
        }}
        onSelect={sync}
        onClick={sync}
        onBlur={(e) => {
          props.onBlur?.(e);
          setCaret(null);
        }}
        onKeyDown={(e) => {
          if (open && items.length && !e.metaKey && !e.ctrlKey) {
            if (e.key === "ArrowDown" || e.key === "ArrowUp") {
              e.preventDefault();
              setActive((i) => (i + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length);
              return;
            }
            if (e.key === "Enter" || e.key === "Tab") {
              e.preventDefault();
              e.stopPropagation();
              take(items[pick]);
              return;
            }
          }
          if (open && at && e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            setDismissed(at.start);
            return;
          }
          props.onKeyDown?.(e);
        }}
      />
      {open && (
        <div id={list} role="listbox" aria-label="References" className={`fade-in absolute top-full left-0 z-50 mt-1 max-h-60 w-[min(22rem,100%)] overflow-y-auto rounded-lg p-1 ${glass}`}>
          {items.length === 0 ? (
            <p className="shimmer px-2 py-1.5 text-[12px] text-subtle">Loading {need ? `${need.env}${need.path === "/" ? "" : need.path}` : ""}…</p>
          ) : (
            items.map((s, i) => {
              const Icon = ICON[s.kind];
              return (
                <div
                  key={s.insert}
                  id={`${list}-${i}`}
                  role="option"
                  aria-selected={i === pick}
                  onMouseDown={(e) => {
                    e.preventDefault();
                    take(s);
                  }}
                  onMouseEnter={() => setActive(i)}
                  className={`flex min-h-7 cursor-pointer items-center gap-2 rounded-sm px-2 py-1 font-mono text-[12px] ${i === pick ? "bg-fg/[0.08] text-fg" : "text-soft"}`}
                >
                  <Icon className="size-3.5 shrink-0 text-muted" />
                  <span className="min-w-0 flex-1 truncate">{s.label}</span>
                  <span className="shrink-0 font-sans text-[10.5px] text-subtle">{s.kind === "key" ? (s.insert.includes(".") ? s.insert.split(".")[0] : "here") : s.kind === "env" ? "environment" : "folder"}</span>
                </div>
              );
            })
          )}
          <p className="flex items-center gap-1 border-t border-line px-2 pt-1 pb-0.5 text-[10.5px] text-subtle">
            <CornerDownRight className="size-3" /> Enter or Tab to insert · Esc to close
          </p>
        </div>
      )}
    </div>
  );
}
