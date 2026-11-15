import { useRef, useState } from "react";
import { complete, completions, openRef, type RefCatalog } from "@/lib/var-refs";
import { input } from "../ui";

/**
 * A variable's value, with references completed as they're typed (`${{ db.` offers db's
 * variables and roll's own): arrows to move, Enter or Tab to take one, Escape to close.
 */
export function RefInput({
  value,
  onChange,
  catalog,
  onWantCatalog,
  placeholder,
  autoFocus,
}: {
  value: string;
  onChange: (v: string) => void;
  catalog: RefCatalog | null;
  /** Asked once a reference is first typed: the catalog is read only when it's wanted. */
  onWantCatalog: () => void;
  placeholder?: string;
  autoFocus?: boolean;
}) {
  const ref = useRef<HTMLInputElement>(null);
  const [caret, setCaret] = useState<number | null>(null);
  const [pick, setPick] = useState(0);
  const [closed, setClosed] = useState(false);
  const before = value.slice(0, caret ?? value.length);
  const open = openRef(before);
  const options = open && catalog && !closed ? completions(open.partial, catalog) : [];
  const take = (choice: string) => {
    const head = complete(before, choice);
    const next = head + value.slice(before.length);
    onChange(next);
    setPick(0);
    requestAnimationFrame(() => {
      ref.current?.setSelectionRange(head.length, head.length);
      setCaret(head.length);
    });
  };
  return (
    <div className="relative min-w-0 flex-1">
      <input
        ref={ref}
        autoFocus={autoFocus}
        value={value}
        placeholder={placeholder ?? "value, or ${{ service.NAME }}"}
        spellCheck={false}
        className={`${input} w-full font-mono text-[12px]`}
        onChange={(e) => {
          onChange(e.target.value);
          setCaret(e.target.selectionStart);
          setClosed(false);
          setPick(0);
          if (openRef(e.target.value.slice(0, e.target.selectionStart ?? undefined))) onWantCatalog();
        }}
        onSelect={(e) => setCaret(e.currentTarget.selectionStart)}
        onBlur={() => setTimeout(() => setClosed(true), 120)}
        onKeyDown={(e) => {
          if (!options.length) return;
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            setPick((p) => (p + (e.key === "ArrowDown" ? 1 : options.length - 1)) % options.length);
          } else if (e.key === "Enter" || e.key === "Tab") {
            e.preventDefault();
            take(options[pick] ?? options[0]!);
          } else if (e.key === "Escape") {
            setClosed(true);
          }
        }}
        role="combobox"
        aria-expanded={options.length > 0}
        aria-autocomplete="list"
      />
      {options.length > 0 && (
        <ul role="listbox" className="absolute top-full left-0 z-20 mt-1 max-h-56 min-w-56 overflow-auto rounded-lg border border-line bg-panel py-1 font-mono text-[12px]">
          {options.map((o, i) => (
            <li
              key={o}
              role="option"
              aria-selected={i === pick}
              onMouseDown={(e) => {
                e.preventDefault();
                take(o);
              }}
              className={`cursor-pointer px-2.5 py-1 ${i === pick ? "bg-fg/[0.07] text-fg" : "text-muted"}`}
            >
              {o}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
