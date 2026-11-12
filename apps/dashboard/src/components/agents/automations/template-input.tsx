/**
 * A template field (docs/code/automations.md, "Templates"): text where `{{ … }}` reads a variable.
 * The variables show in the accent as you type them; typing `{{` (or the `{}` button) opens a
 * picker of what this step can read (the event's fields, the run, earlier steps' outputs), with a
 * sample value from the test event when there is one (Zapier's mapped-data pills, n8n's preview).
 * ↑↓ move, Enter or Tab inserts, Esc closes.
 */

import { Braces } from "lucide-react";
import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { insertVariable, matchVariables, openTemplate, templateParts, type Variable } from "@/lib/automations-ui";
import { useDismiss } from "../dismiss";
import { Pop } from "../pop";
import { cn } from "../ui";

const box =
  "w-full rounded-lg border bg-panel-2 px-2.5 text-[13px] leading-[20px] tracking-[-0.005em] outline-none transition-colors hover:border-line-3 focus-within:border-line-3";

export function TemplateInput({
  label,
  value,
  onChange,
  variables,
  multiline = false,
  placeholder,
  sample,
  preview,
  error,
  mono = false,
  rows = 3,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  variables: readonly Variable[];
  multiline?: boolean;
  placeholder?: string;
  /** A value for a path from the test event, for the picker's hints. */
  sample?: (path: string) => string | undefined;
  /** What it rendered to in the last test. */
  preview?: string | null;
  error?: string;
  mono?: boolean;
  rows?: number;
}) {
  const field = useRef<HTMLTextAreaElement & HTMLInputElement>(null);
  const back = useRef<HTMLDivElement>(null);
  const [pick, setPick] = useState<{ start: number; query: string } | null>(null);
  const [index, setIndex] = useState(0);
  const caretAfter = useRef<number | null>(null);
  const list = useId();
  const options = useMemo(() => (pick ? matchVariables(variables, pick.query).slice(0, 40) : []), [pick, variables]);
  // An open picker sits under the field: bring it into the panel's view (jsdom has no scrolling).
  const listEl = useRef<HTMLUListElement>(null);
  // The variables close on a press or focus outside, Escape or a pick; never on a blur alone (Safari).
  const wrap = useRef<HTMLDivElement>(null);
  useDismiss(!!pick, [wrap], () => setPick(null));
  const open = !!pick && options.length > 0;
  useEffect(() => {
    if (open) listEl.current?.scrollIntoView?.({ block: "nearest" });
  }, [open]);

  useLayoutEffect(() => {
    const c = caretAfter.current;
    if (c === null || !field.current) return;
    caretAfter.current = null;
    field.current.focus();
    field.current.setSelectionRange(c, c);
  });

  const sync = () => {
    const f = field.current;
    if (!f || !back.current) return;
    back.current.scrollTop = f.scrollTop;
    back.current.scrollLeft = f.scrollLeft;
  };
  const look = () => {
    const f = field.current;
    if (!f) return;
    const at = openTemplate(f.value, f.selectionStart ?? f.value.length);
    setPick((p) => (at?.start === p?.start && at?.query === p?.query ? p : at));
    if (!at || at.query !== pick?.query) setIndex(0);
    sync();
  };
  const choose = (v: Variable) => {
    const f = field.current;
    if (!f || !pick) return;
    const r = insertVariable(value, pick.start, f.selectionStart ?? value.length, v.path);
    caretAfter.current = r.caret;
    setPick(null);
    onChange(r.text);
  };
  const onKeyDown = (e: KeyboardEvent) => {
    if (!pick || !options.length) {
      if (e.key === "Escape" && pick) {
        e.preventDefault();
        e.stopPropagation();
        setPick(null);
      }
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      setIndex((i) => (i + (e.key === "ArrowDown" ? 1 : options.length - 1)) % options.length);
    } else if (e.key === "Enter" || e.key === "Tab") {
      e.preventDefault();
      choose(options[Math.min(index, options.length - 1)]);
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      setPick(null);
    }
  };
  const openPicker = () => {
    const f = field.current;
    if (!f) return;
    const at = f.selectionStart ?? value.length;
    const next = `${value.slice(0, at)}{{${value.slice(f.selectionEnd ?? at)}`;
    caretAfter.current = at + 2;
    onChange(next);
    setPick({ start: at, query: "" });
    setIndex(0);
  };

  const shared = {
    ref: field,
    value,
    "aria-label": label,
    "aria-invalid": error ? true : undefined,
    "aria-autocomplete": "list" as const,
    "aria-expanded": !!pick && options.length > 0,
    "aria-controls": pick ? list : undefined,
    "aria-activedescendant": pick && options.length ? `${list}-${Math.min(index, options.length - 1)}` : undefined,
    role: "combobox",
    placeholder,
    spellCheck: false,
    onChange: (e: { target: { value: string } }) => {
      onChange(e.target.value);
      // The field already holds the new text and caret.
      look();
    },
    onKeyDown,
    onKeyUp: (e: KeyboardEvent) => !["ArrowDown", "ArrowUp", "Enter", "Tab", "Escape"].includes(e.key) && look(),
    onClick: look,
    onScroll: sync,
    className: cn(
      "relative block w-full resize-none bg-transparent text-transparent caret-fg outline-none placeholder:text-faint selection:bg-accent/30 selection:text-transparent",
      multiline ? "py-[7px]" : "h-8 py-[5px] whitespace-pre",
      mono && "font-mono text-[12px]",
    ),
  };
  const parts = templateParts(value);
  return (
    <div ref={wrap} className="relative">
      <div className={cn(box, "relative pr-8", error && !open ? "border-danger/50" : "border-line-2")}>
        <div
          ref={back}
          aria-hidden
          className={cn("pointer-events-none absolute inset-0 overflow-hidden px-2.5 text-fg", multiline ? "py-[7px] break-words whitespace-pre-wrap" : "py-[5px] whitespace-pre", mono && "font-mono text-[12px]")}
        >
          {parts.map((p, i) =>
            p.token ? (
              <span key={i} className="rounded-[3px] bg-accent/12 text-accent">
                {p.text}
              </span>
            ) : (
              <span key={i}>{p.text}</span>
            ),
          )}
          {multiline && "​"}
        </div>
        {multiline ? <textarea rows={rows} {...shared} /> : <input type="text" {...shared} />}
        <button
          type="button"
          tabIndex={-1}
          onMouseDown={(e) => e.preventDefault()}
          onClick={openPicker}
          title="Insert a variable ({{)"
          aria-label={`Insert a variable into ${label}`}
          className={cn("absolute right-1.5 grid size-6 place-items-center rounded-md text-subtle transition-colors hover:bg-fg/[0.06] hover:text-fg", multiline ? "top-1.5" : "top-1")}
        >
          <Braces className="size-3.5" strokeWidth={1.75} />
        </button>
      </div>
      <Pop
      open={!!pick && options.length > 0}
      as="ul"
      id={list}
      ref={listEl}
      role="listbox"
      aria-label="Variables"
      className="absolute inset-x-0 top-full z-[140] mt-1 max-h-64 overflow-y-auto rounded-[10px] border border-outline bg-panel-2 p-1 shadow-pop inset-shadow-highlight"
      onMouseDown={(e) => e.preventDefault()}
    >
        {options.map((v, i) => {
          const groupStart = i === 0 || options[i - 1].group !== v.group;
          const s = sample?.(v.path);
          return (
            <li key={v.path} role="presentation">
              {groupStart && <p className="px-2 pt-1.5 pb-1 text-[11px] font-medium text-subtle">{v.group}</p>}
              <div
                id={`${list}-${i}`}
                role="option"
                aria-selected={i === index}
                onMouseEnter={() => setIndex(i)}
                onClick={() => choose(v)}
                className={cn("flex min-h-7 cursor-pointer items-center gap-2 rounded-md px-2 text-[12px]", i === index && "bg-fg/[0.08]")}
              >
                <span className="min-w-0 truncate font-mono">
                  {v.path.startsWith("event.data.") ? (
                    <>
                      <span className="text-subtle">event.data.</span>
                      {v.path.slice(11)}
                    </>
                  ) : (
                    v.path
                  )}
                </span>
                <span className="ml-auto min-w-0 shrink truncate text-[11.5px] text-subtle">{s ?? v.hint}</span>
              </div>
            </li>
          );
        })}
      </Pop>
      {error && !open ? (
        <p className="mt-1 text-[11.5px] text-danger">{error}</p>
      ) : preview != null ? (
        <p className="mt-1 line-clamp-2 font-mono text-[11px] break-all text-subtle" title={preview}>
          <span className="text-faint">→ </span>
          {preview || <span className="text-faint">(empty)</span>}
        </p>
      ) : null}
    </div>
  );
}
