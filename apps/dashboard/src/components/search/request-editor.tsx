import { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState } from "react";
import { esJson, type SearchCluster } from "@/lib/search-api";
import { applyCompletion, complete, mappingFields, type Completion, type FieldInfo } from "@/lib/search-console";
import { API_PATHS, COMMON_PARAMS, PARAMS } from "@/lib/search-params.generated";

const PARAM_TABLE = { params: PARAMS, common: COMMON_PARAMS, apis: API_PATHS };

/**
 * The console's request editor: a plain textarea with completion (methods, paths, index names,
 * body keys, query and aggregation types, field names; see `lib/search-console.ts`).
 *
 * The list opens while typing and with Ctrl+Space; ↑/↓ move, Enter or Tab accept, Esc closes.
 * Index names and mappings are fetched through the console only when a completion first needs
 * them, so opening the console never wakes a sleeping cluster by itself.
 */

/** The first path segment of the request's first line, as typed (`logs-*`, `a,b`). */
function rawIndex(text: string): string | null {
  const first = text.split("\n", 1)[0].trim();
  const m = /^[A-Za-z]+\s+\/?([^/?\s]+)/.exec(first);
  if (!m || m[1].startsWith("_")) return null;
  return m[1];
}

/** Pixel position of character `pos` in a textarea (a hidden mirror with the same layout). */
function caretXY(el: HTMLTextAreaElement, pos: number): { left: number; top: number; height: number } {
  const cs = window.getComputedStyle(el);
  const div = document.createElement("div");
  for (const p of [
    "boxSizing",
    "width",
    "paddingTop",
    "paddingRight",
    "paddingBottom",
    "paddingLeft",
    "borderTopWidth",
    "borderRightWidth",
    "borderBottomWidth",
    "borderLeftWidth",
    "fontFamily",
    "fontSize",
    "fontWeight",
    "lineHeight",
    "letterSpacing",
    "tabSize",
  ] as const) {
    div.style[p] = cs[p];
  }
  div.style.position = "absolute";
  div.style.visibility = "hidden";
  div.style.whiteSpace = "pre-wrap";
  div.style.overflowWrap = "break-word";
  div.style.top = "0";
  div.style.left = "-9999px";
  div.textContent = el.value.slice(0, pos);
  const mark = document.createElement("span");
  mark.textContent = el.value.slice(pos, pos + 1) || ".";
  div.appendChild(mark);
  document.body.appendChild(div);
  const r = { left: mark.offsetLeft - el.scrollLeft, top: mark.offsetTop - el.scrollTop, height: parseFloat(cs.lineHeight) || 18 };
  document.body.removeChild(div);
  return r;
}

const KIND_MARK: Record<string, string> = {
  method: "m",
  api: "/",
  index: "i",
  key: "k",
  query: "q",
  agg: "a",
  field: "f",
  value: "v",
  param: "?",
};

export interface RequestEditorHandle {
  focus: () => void;
}

export const RequestEditor = forwardRef<
  RequestEditorHandle,
  { cluster: SearchCluster; value: string; onChange: (v: string) => void; onSubmit: () => void }
>(function RequestEditor({ cluster, value, onChange, onSubmit }, ref) {
  const area = useRef<HTMLTextAreaElement>(null);
  const [comp, setComp] = useState<Completion | null>(null);
  const [sel, setSel] = useState(0);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);
  const indices = useRef<string[] | null>(null);
  const fields = useRef(new Map<string, FieldInfo[]>());
  const loading = useRef(new Set<string>());
  /** Bumped when a fetch lands, to recompute an open list. */
  const [sourcesGen, setSourcesGen] = useState(0);
  const open = useRef(false);
  /** A completion was asked for while its sources were still loading. */
  const waiting = useRef(false);

  useImperativeHandle(ref, () => ({ focus: () => area.current?.focus() }), []);

  // Sources belong to the cluster.
  useEffect(() => {
    indices.current = null;
    fields.current = new Map();
    loading.current = new Set();
  }, [cluster.id]);

  /**
   * Starts fetching whatever the completion at the cursor needs (recomputed when it arrives);
   * true while something it needs is still loading.
   */
  const ensureSources = useCallback(
    (text: string, cursor: number): boolean => {
      const onFirstLine = cursor <= (text.indexOf("\n") < 0 ? text.length : text.indexOf("\n"));
      if (onFirstLine && indices.current === null && !loading.current.has("*indices")) {
        loading.current.add("*indices");
        esJson<{ index: string }[]>(cluster.id, "GET", "/_cat/indices?format=json&h=index&s=index&expand_wildcards=all")
          .then(
            (rows) => (indices.current = Array.isArray(rows) ? rows.map((r) => r.index) : []),
            () => (indices.current = []),
          )
          .finally(() => {
            loading.current.delete("*indices");
            setSourcesGen((g) => g + 1);
          });
      }
      const idx = rawIndex(text);
      if (!onFirstLine && idx && !fields.current.has(idx) && !loading.current.has(idx)) {
        loading.current.add(idx);
        esJson(cluster.id, "GET", `/${idx}/_mapping`)
          .then(
            (m) => fields.current.set(idx, mappingFields(m)),
            () => fields.current.set(idx, []),
          )
          .finally(() => {
            loading.current.delete(idx);
            setSourcesGen((g) => g + 1);
          });
      }
      return onFirstLine ? loading.current.has("*indices") : !!idx && loading.current.has(idx);
    },
    [cluster.id],
  );

  const compute = useCallback(
    (text: string, cursor: number) => {
      waiting.current = ensureSources(text, cursor);
      const idx = rawIndex(text);
      const c = complete(text, cursor, { indices: indices.current ?? [], fields: (idx && fields.current.get(idx)) || [], params: PARAM_TABLE });
      setComp(c);
      setSel(0);
      open.current = c !== null;
      if (c && area.current) {
        const xy = caretXY(area.current, c.from);
        setPos({ left: Math.max(0, xy.left), top: xy.top + xy.height + 2 });
      }
    },
    [ensureSources],
  );

  // Sources arrived while the list is open: refresh it in place.
  useEffect(() => {
    const el = area.current;
    if (el && (open.current || waiting.current) && document.activeElement === el) compute(el.value, el.selectionStart);
  }, [sourcesGen, compute]);

  const close = () => {
    open.current = false;
    waiting.current = false;
    setComp(null);
  };

  const accept = (i: number) => {
    const el = area.current;
    if (!comp || !el) return;
    const item = comp.items[i];
    if (!item) return;
    const r = applyCompletion(el.value, comp, item);
    onChange(r.text);
    close();
    requestAnimationFrame(() => {
      el.focus();
      el.setSelectionRange(r.cursor, r.cursor);
      // A path segment ending in `/` continues straight into the next one.
      if (item.label.endsWith("/")) compute(r.text, r.cursor);
    });
  };

  return (
    <div className="relative flex min-h-80 flex-1 flex-col">
      <textarea
        ref={area}
        value={value}
        onChange={(e) => {
          onChange(e.target.value);
          const ev = e.nativeEvent as InputEvent;
          const typed = ev.inputType === "insertText" && ev.data != null && /[\w"./\s-]/.test(ev.data) && ev.data !== "\n";
          const erased = ev.inputType?.startsWith("delete") && open.current;
          if (typed || erased) compute(e.target.value, e.target.selectionStart);
          else close();
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
            e.preventDefault();
            close();
            onSubmit();
            return;
          }
          if (e.key === " " && e.ctrlKey) {
            e.preventDefault();
            compute(e.currentTarget.value, e.currentTarget.selectionStart);
            return;
          }
          if (!comp) return;
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            const n = comp.items.length;
            setSel((s) => (e.key === "ArrowDown" ? (s + 1) % n : (s - 1 + n) % n));
          } else if (e.key === "Enter" || e.key === "Tab") {
            e.preventDefault();
            accept(sel);
          } else if (e.key === "Escape") {
            e.preventDefault();
            close();
          } else if (e.key === "ArrowLeft" || e.key === "ArrowRight" || e.key === "Home" || e.key === "End") {
            close();
          }
        }}
        onBlur={close}
        onClick={close}
        onScroll={close}
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
        aria-label="Request: method and path on the first line, body below. Ctrl+Space for suggestions."
        aria-autocomplete="list"
        aria-expanded={comp !== null}
        aria-controls="search-console-completions"
        className="min-h-80 w-full flex-1 resize-y bg-transparent px-5 py-4 font-mono text-[12px] leading-relaxed outline-none"
      />
      {comp && pos && (
        <ul
          id="search-console-completions"
          role="listbox"
          className="rise absolute z-20 max-h-56 min-w-56 max-w-[min(28rem,90%)] overflow-auto rounded-lg border border-line-2 bg-panel py-1 font-mono text-[12px]"
          style={{ left: pos.left, top: pos.top }}
          // Keep the textarea focused while picking with the mouse.
          onMouseDown={(e) => e.preventDefault()}
        >
          {comp.items.map((s, i) => (
            <li key={`${s.kind}:${s.label}`} role="option" aria-selected={i === sel}>
              <button
                type="button"
                tabIndex={-1}
                onMouseEnter={() => setSel(i)}
                onClick={() => accept(i)}
                className={`flex w-full items-baseline gap-2 px-2.5 py-1 text-left ${i === sel ? "bg-panel-2 text-fg" : "text-muted"}`}
              >
                <span className="w-3 shrink-0 text-center text-[10px] text-subtle">
                  {KIND_MARK[s.kind]}
                </span>
                <span className="min-w-0 flex-1 truncate">{s.label}</span>
                {s.detail && <span className="shrink-0 font-sans text-[11px] text-subtle">{s.detail}</span>}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
});
