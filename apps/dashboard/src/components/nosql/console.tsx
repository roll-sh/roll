"use client";

import { HorizonMark } from "@roll/brand";
import { History, Play, TriangleAlert, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { isCqlError, nosqlApi, type CqlResult, type CqlRows, type NosqlDb } from "@/lib/nosql-api";
import {
  applyCompletion,
  complete,
  shellCommand,
  splitStatements,
  traceEventsQuery,
  type Completion,
  type CompletionResult,
} from "@/lib/nosql-cql";
import { useMarkState } from "../mark-state";
import { btn, input, Panel, primary } from "../ui";
import type { SchemaState } from "./database";
import { CqlValue } from "./data";

interface Trace {
  state: "loading" | "done" | "none" | "error";
  events?: (string | null)[][];
  error?: string;
}

interface Entry {
  id: number;
  statement: string;
  keyspace: string | null;
  result?: CqlResult;
  /** Extra pages fetched with "more". */
  more?: (string | null)[][];
  pageState?: string | null;
  /** A transport or API error (not a CQL error). */
  error?: string;
  note?: string;
  ms: number;
  trace?: Trace;
}

const HISTORY_MAX = 100;
const historyKey = (id: string) => `roll.nosql.history.${id}`;

/** cqlsh's names for the native protocol error codes. */
const EXCEPTION: Record<string, string> = {
  SERVER_ERROR: "ServerError",
  PROTOCOL_ERROR: "ProtocolError",
  BAD_CREDENTIALS: "AuthenticationFailed",
  UNAVAILABLE: "Unavailable",
  OVERLOADED: "Overloaded",
  IS_BOOTSTRAPPING: "IsBootstrapping",
  TRUNCATE_ERROR: "TruncateError",
  WRITE_TIMEOUT: "WriteTimeout",
  READ_TIMEOUT: "ReadTimeout",
  READ_FAILURE: "ReadFailure",
  FUNCTION_FAILURE: "FunctionFailure",
  WRITE_FAILURE: "WriteFailure",
  SYNTAX_ERROR: "SyntaxException",
  UNAUTHORIZED: "Unauthorized",
  INVALID: "InvalidRequest",
  CONFIG_ERROR: "ConfigurationException",
  ALREADY_EXISTS: "AlreadyExists",
  UNPREPARED: "Unprepared",
};

export function CqlConsole({ db, schema: st, prefill }: { db: NosqlDb; schema: SchemaState; prefill: { text: string; n: number } | null }) {
  const { setState, play } = useMarkState();
  const [text, setText] = useState("");
  const [keyspace, setKeyspace] = useState<string | null>(null);
  const [trace, setTrace] = useState(false);
  const [entries, setEntries] = useState<Entry[]>([]);
  const [running, setRunning] = useState<{ i: number; n: number; statement: string } | null>(null);
  const [history, setHistory] = useState<string[]>([]);
  const [showHistory, setShowHistory] = useState(false);
  const [comp, setComp] = useState<(CompletionResult & { active: number; nav: boolean; x: number; y: number }) | null>(null);
  const field = useRef<HTMLTextAreaElement>(null);
  const out = useRef<HTMLDivElement>(null);
  const cursor = useRef(-1);
  const seq = useRef(0);

  useEffect(() => {
    try {
      const h = JSON.parse(localStorage.getItem(historyKey(db.id)) ?? "[]");
      if (Array.isArray(h)) setHistory(h.filter((x) => typeof x === "string").slice(0, HISTORY_MAX));
    } catch {}
  }, [db.id]);

  useEffect(() => {
    if (prefill) {
      setText(prefill.text);
      requestAnimationFrame(() => field.current?.focus());
    }
  }, [prefill]);

  useEffect(() => {
    out.current?.scrollTo({ top: out.current.scrollHeight });
  }, [entries, running]);

  const remember = (t: string) => {
    setHistory((h) => {
      const next = [t, ...h.filter((x) => x !== t)].slice(0, HISTORY_MAX);
      try {
        localStorage.setItem(historyKey(db.id), JSON.stringify(next));
      } catch {}
      return next;
    });
    cursor.current = -1;
  };

  const patch = (id: number, p: Partial<Entry>) => setEntries((es) => es.map((e) => (e.id === id ? { ...e, ...p } : e)));

  const fetchTrace = useCallback(
    async (id: number, sessionId: string) => {
      // Trace events are written asynchronously, as cqlsh waits for them.
      for (let attempt = 0; attempt < 4; attempt++) {
        try {
          const r = await nosqlApi.cql(db.id, { query: traceEventsQuery(sessionId), page_size: 500 });
          if (isCqlError(r)) return patch(id, { trace: { state: "error", error: r.error } });
          if (r.kind === "rows" && r.rows.length) return patch(id, { trace: { state: "done", events: r.rows } });
        } catch (e) {
          return patch(id, { trace: { state: "error", error: e instanceof Error ? e.message : String(e) } });
        }
        await new Promise((ok) => setTimeout(ok, 250 * (attempt + 1)));
      }
      patch(id, { trace: { state: "done", events: [] } });
    },
    [db.id],
  );

  const run = async (source: string) => {
    const statements = splitStatements(source);
    if (!statements.length || running) return;
    remember(source.trim());
    setComp(null);
    setShowHistory(false);
    let ks = keyspace;
    let tracing = trace;
    let failed = false;
    let ddl = false;
    setState("thinking");
    for (let i = 0; i < statements.length; i++) {
      const s = statements[i];
      const id = ++seq.current;
      const shell = shellCommand(s);
      if (shell) {
        let note: string;
        if (shell.cmd === "CLEAR" || shell.cmd === "CLS") {
          setEntries([]);
          continue;
        } else if (shell.cmd === "TRACING") {
          const v = shell.arg.toUpperCase();
          tracing = v === "ON" ? true : v === "OFF" ? false : tracing;
          setTrace(tracing);
          note = v === "ON" || v === "OFF" ? `Tracing is now ${tracing ? "enabled" : "disabled"}` : `Tracing is currently ${tracing ? "enabled" : "disabled"}. Use TRACING ON or OFF.`;
        } else {
          note = `${shell.cmd} is a cqlsh shell command; the console runs CQL only. Use roll nosql cqlsh ${db.name} for the full shell.`;
        }
        setEntries((es) => [...es.slice(-49), { id, statement: s, keyspace: ks, note, ms: 0 }]);
        continue;
      }
      setRunning({ i, n: statements.length, statement: s });
      const t0 = performance.now();
      try {
        const r = await nosqlApi.cql(db.id, { query: s, ...(ks ? { keyspace: ks } : {}), ...(tracing ? { trace: true } : {}) });
        const ms = r.elapsed_ms ?? performance.now() - t0;
        if (isCqlError(r)) failed = true;
        if (r.keyspace !== undefined) ks = r.keyspace;
        if (r.kind === "set_keyspace") ks = r.keyspace;
        if (r.kind === "schema_change") ddl = true;
        const traceState: Trace | undefined = tracing ? (r.tracing_id ? { state: "loading" } : { state: "none" }) : undefined;
        setEntries((es) => [
          ...es.slice(-49),
          { id, statement: s, keyspace: ks, result: r, ms, pageState: r.kind === "rows" ? r.page_state : null, trace: traceState },
        ]);
        if (tracing && r.tracing_id) fetchTrace(id, r.tracing_id);
      } catch (e) {
        failed = true;
        setEntries((es) => [...es.slice(-49), { id, statement: s, keyspace: ks, error: e instanceof Error ? e.message : String(e), ms: performance.now() - t0 }]);
      }
    }
    setKeyspace(ks);
    setRunning(null);
    play([[failed ? "error" : "success", failed ? 1400 : 1200]]);
    if (ddl) st.reload();
    requestAnimationFrame(() => field.current?.focus());
  };

  const more = async (e: Entry) => {
    if (!e.pageState || running) return;
    setState("thinking");
    try {
      const r = await nosqlApi.cql(db.id, { query: e.statement, page_state: e.pageState, ...(e.keyspace ? { keyspace: e.keyspace } : {}) });
      if (r.kind === "rows") {
        patch(e.id, { more: [...(e.more ?? []), ...r.rows], pageState: r.page_state });
        play([["success", 900]]);
      } else {
        patch(e.id, { pageState: null, note: isCqlError(r) ? r.error : undefined });
        play([["error", 1200]]);
      }
    } catch (err) {
      patch(e.id, { note: err instanceof Error ? err.message : String(err) });
      play([["error", 1200]]);
    }
  };

  // ---- completion ----------------------------------------------------------------------------

  const suggest = (force = false) => {
    const el = field.current;
    if (!el || el.selectionStart !== el.selectionEnd) return setComp(null);
    const pos = el.selectionStart;
    const r = complete(el.value, pos, st.schema, keyspace);
    const typed = /[A-Za-z0-9_."]$/.test(el.value.slice(0, pos));
    if (!r.items.length || (!force && !typed && !/\b(FROM|INTO|UPDATE|USE|TABLE|ON)\s+$/i.test(el.value.slice(0, pos)))) return setComp(null);
    const { x, y } = caretXY(el, r.from);
    setComp({ ...r, active: 0, nav: false, x, y });
  };

  const accept = (c: Completion) => {
    const el = field.current;
    if (!el || !comp) return;
    const next = applyCompletion(el.value, comp, c);
    setText(next.text);
    setComp(null);
    requestAnimationFrame(() => {
      el.focus();
      el.setSelectionRange(next.pos, next.pos);
    });
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (comp) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        const d = e.key === "ArrowDown" ? 1 : -1;
        setComp({ ...comp, nav: true, active: (comp.active + d + comp.items.length) % comp.items.length });
        return;
      }
      // Tab accepts; Enter only once a suggestion was picked with the arrows (else it is a newline).
      if (e.key === "Tab" || (e.key === "Enter" && comp.nav && !e.metaKey && !e.ctrlKey && !e.shiftKey)) {
        e.preventDefault();
        accept(comp.items[comp.active]);
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        setComp(null);
        return;
      }
      if (e.key === "Enter") setComp(null);
    }
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      const el = e.currentTarget;
      const selected = el.value.slice(el.selectionStart, el.selectionEnd);
      run(selected.trim() ? selected : el.value);
      return;
    }
    if (e.key === " " && e.ctrlKey) {
      e.preventDefault();
      suggest(true);
      return;
    }
    if ((e.key === "ArrowUp" || e.key === "ArrowDown") && (e.ctrlKey || e.metaKey) && history.length) {
      e.preventDefault();
      cursor.current = Math.max(-1, Math.min(history.length - 1, cursor.current + (e.key === "ArrowUp" ? 1 : -1)));
      setText(cursor.current < 0 ? "" : history[cursor.current]);
      return;
    }
    if (e.key === "l" && e.ctrlKey) {
      e.preventDefault();
      setEntries([]);
    }
  };

  const keyspaces = useMemo(() => st.schema?.keyspaces.map((k) => k.name) ?? [], [st.schema]);
  const prompt = `cqlsh${keyspace ? `:${keyspace}` : ""}>`;

  return (
    <div className="space-y-4">
      <Panel
        title="CQL console"
        aside={
          <div className="flex items-center gap-3">
            <label className="flex items-center gap-1.5 text-[12px] text-subtle">
              <input type="checkbox" checked={trace} onChange={(e) => setTrace(e.target.checked)} className="accent-current" />
              Tracing
            </label>
            <select
              value={keyspace ?? ""}
              onChange={(e) => setKeyspace(e.target.value || null)}
              aria-label="Session keyspace"
              className={`${input} py-1 font-mono text-[12px]`}
            >
              <option value="">no keyspace</option>
              {keyspaces.map((k) => (
                <option key={k} value={k}>
                  {k}
                </option>
              ))}
              {keyspace && !keyspaces.includes(keyspace) && <option value={keyspace}>{keyspace}</option>}
            </select>
            {entries.length > 0 && (
              <button onClick={() => setEntries([])} title="Clear output" aria-label="Clear output" className="p-1 text-subtle hover:text-fg">
                <X className="size-3.5" />
              </button>
            )}
          </div>
        }
      >
        <div ref={out} className="max-h-[34rem] min-h-40 overflow-y-auto px-5 py-3 font-mono text-[12px] leading-relaxed">
          {entries.length === 0 && !running && (
            <div className="space-y-1 text-subtle">
              <p>
                Run CQL as <span className="text-muted">{db.credentials.role}</span>, the database&apos;s superuser.
                Several statements run in order; DESCRIBE works as in cqlsh.
              </p>
              <p>
                <Kbd>⌘/Ctrl ⏎</Kbd> run (the selection, if any) · <Kbd>Tab</Kbd> complete · <Kbd>Ctrl Space</Kbd> suggest ·{" "}
                <Kbd>⌘/Ctrl ↑ ↓</Kbd> history · <span className="text-muted">TRACING ON</span> to trace
              </p>
            </div>
          )}
          {entries.map((e) => (
            <EntryView key={e.id} e={e} onMore={() => more(e)} busy={running !== null} />
          ))}
          {running && (
            <div className="py-1.5" aria-live="polite">
              <p className="flex gap-2">
                <span className="text-subtle">{prompt}</span>
                <span className="min-w-0 flex-1 truncate whitespace-pre-wrap">{running.statement}</span>
              </p>
              <p className="shimmer">
                {running.n > 1 ? `Running statement ${running.i + 1} of ${running.n}…` : "Running…"}
              </p>
            </div>
          )}
        </div>
        <form
          className="relative border-t border-line"
          onSubmit={(e) => {
            e.preventDefault();
            run(text);
          }}
        >
          <div className="flex items-start gap-2 px-5 pt-3">
            <span className="pt-px font-mono text-[12px] text-subtle select-none">{prompt}</span>
            <textarea
              ref={field}
              value={text}
              onChange={(e) => {
                setText(e.target.value);
                cursor.current = -1;
              }}
              onKeyDown={onKeyDown}
              onKeyUp={(e) => {
                if (!["ArrowUp", "ArrowDown", "Enter", "Tab", "Escape", "Control", "Meta", "Shift"].includes(e.key)) suggest();
              }}
              onClick={() => setComp(null)}
              onBlur={() => setTimeout(() => setComp(null), 120)}
              rows={Math.min(14, Math.max(3, text.split("\n").length))}
              placeholder="SELECT * FROM system.local;"
              spellCheck={false}
              autoComplete="off"
              aria-label="CQL"
              aria-autocomplete="list"
              aria-expanded={comp !== null}
              aria-controls="cql-completions"
              className="min-w-0 flex-1 resize-none bg-transparent font-mono text-[12px] leading-relaxed outline-none placeholder:text-subtle"
            />
          </div>
          {comp && (
            <ul
              id="cql-completions"
              role="listbox"
              className="rise absolute z-20 max-h-60 w-72 overflow-y-auto rounded-lg border border-line-2 bg-panel py-1 font-mono text-[12px]"
              style={{ left: comp.x, top: comp.y }}
            >
              {comp.items.map((c, i) => (
                <li
                  key={`${c.kind}-${c.label}`}
                  role="option"
                  aria-selected={i === comp.active}
                  onMouseDown={(ev) => {
                    ev.preventDefault();
                    accept(c);
                  }}
                  className={`flex cursor-pointer items-center gap-2 px-2.5 py-1 ${i === comp.active ? "bg-panel-2 text-fg" : "text-muted"}`}
                >
                  <span className="w-12 shrink-0 text-[10px] text-subtle">{c.kind}</span>
                  <span className="min-w-0 flex-1 truncate">{c.label}</span>
                  {c.detail && <span className="shrink-0 truncate text-[10px] text-subtle">{c.detail}</span>}
                </li>
              ))}
            </ul>
          )}
          <div className="flex items-center justify-between gap-2 px-5 py-3">
            <div className="relative">
              <button type="button" onClick={() => setShowHistory((s) => !s)} disabled={!history.length} className={btn} aria-expanded={showHistory}>
                <History className="size-3.5" /> History
              </button>
              {showHistory && (
                <ul className="rise absolute bottom-full left-0 z-20 mb-2 max-h-72 w-[min(36rem,80vw)] overflow-y-auto rounded-lg border border-line-2 bg-panel py-1 font-mono text-[12px]">
                  {history.map((h, i) => (
                    <li key={i}>
                      <button
                        type="button"
                        onClick={() => {
                          setText(h);
                          setShowHistory(false);
                          field.current?.focus();
                        }}
                        className="block w-full truncate px-3 py-1.5 text-left text-muted hover:bg-panel-2 hover:text-fg"
                      >
                        {h.replace(/\s+/g, " ")}
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </div>
            <button type="submit" disabled={running !== null || !text.trim()} className={primary}>
              {running ? (
                <>
                  <HorizonMark state="thinking" size={14} interactive={false} /> Running
                </>
              ) : (
                <>
                  <Play className="size-3" /> Run
                </>
              )}
            </button>
          </div>
        </form>
      </Panel>
      {st.error && <p className="text-[12px] text-subtle">Completion is limited: the schema couldn&apos;t be read ({st.error}).</p>}
    </div>
  );
}

function Kbd({ children }: { children: ReactNode }) {
  return <kbd className="rounded border border-line px-1 font-mono text-[10px] text-muted">{children}</kbd>;
}

/** The caret's position (below the line) in a textarea, via a mirror element. */
function caretXY(el: HTMLTextAreaElement, pos: number) {
  const cs = getComputedStyle(el);
  const mirror = document.createElement("div");
  for (const p of ["fontFamily", "fontSize", "fontWeight", "lineHeight", "letterSpacing", "paddingTop", "paddingLeft", "paddingRight", "borderWidth", "boxSizing", "whiteSpace", "wordWrap", "tabSize"] as const) {
    mirror.style[p] = cs[p];
  }
  mirror.style.position = "absolute";
  mirror.style.visibility = "hidden";
  mirror.style.whiteSpace = "pre-wrap";
  mirror.style.width = `${el.clientWidth}px`;
  mirror.textContent = el.value.slice(0, pos);
  const mark = document.createElement("span");
  mark.textContent = "​";
  mirror.appendChild(mark);
  document.body.appendChild(mirror);
  const lh = parseFloat(cs.lineHeight) || 18;
  const x = el.offsetLeft + mark.offsetLeft - el.scrollLeft;
  const y = el.offsetTop + mark.offsetTop - el.scrollTop + lh + 4;
  mirror.remove();
  return { x: Math.min(x, Math.max(0, el.offsetLeft + el.clientWidth - 288)), y };
}

function EntryView({ e, onMore, busy }: { e: Entry; onMore: () => void; busy: boolean }) {
  const r = e.result;
  return (
    <div className="rise border-b border-line/60 py-2 last:border-0">
      <p className="flex gap-2">
        <span className="shrink-0 text-subtle">cqlsh{e.keyspace ? `:${e.keyspace}` : ""}&gt;</span>
        <span className="min-w-0 flex-1 break-all whitespace-pre-wrap">{e.statement}</span>
        <span className="shrink-0 text-[10px] text-subtle">{e.ms ? `${e.ms.toFixed(1)} ms` : ""}</span>
      </p>
      {e.error && <p className="mt-1 text-danger">{e.error}</p>}
      {e.note && <p className="mt-1 text-muted">{e.note}</p>}
      {r && <ResultView r={r} more={e.more} pageState={e.pageState ?? null} onMore={onMore} busy={busy} />}
      {r?.warnings && r.warnings.length > 0 && (
        <div className="mt-2 space-y-1 rounded-lg border border-line px-3 py-2 text-muted">
          {r.warnings.map((w, i) => (
            <p key={i} className="flex gap-2">
              <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-subtle" aria-label="Warning" />
              <span className="min-w-0 break-words">{w}</span>
            </p>
          ))}
        </div>
      )}
      {e.trace && <TraceView t={e.trace} id={r?.tracing_id ?? null} />}
    </div>
  );
}

function ResultView({
  r,
  more,
  pageState,
  onMore,
  busy,
}: {
  r: CqlResult;
  more?: (string | null)[][];
  pageState: string | null;
  onMore: () => void;
  busy: boolean;
}) {
  if (isCqlError(r)) {
    return (
      <p className="mt-1 break-words text-danger">
        <span className="font-medium">{EXCEPTION[r.code_name] ?? r.code_name}</span>: {r.error}{" "}
        <span className="text-[10px] text-subtle">code 0x{r.code.toString(16).padStart(4, "0")}</span>
      </p>
    );
  }
  switch (r.kind) {
    case "void":
      return <p className="mt-1 text-subtle">OK</p>;
    case "set_keyspace":
      return <p className="mt-1 text-subtle">Now using keyspace {r.keyspace}</p>;
    case "schema_change": {
      const c = r.schema_change;
      return (
        <p className="mt-1 text-subtle">
          {c.change} {c.target} {c.keyspace}
          {c.name ? `.${c.name}` : ""}
        </p>
      );
    }
    case "rows":
      return <RowsView r={r} more={more} pageState={pageState} onMore={onMore} busy={busy} />;
  }
}

function RowsView({ r, more, pageState, onMore, busy }: { r: CqlRows; more?: (string | null)[][]; pageState: string | null; onMore: () => void; busy: boolean }) {
  const rows = more ? [...r.rows, ...more] : r.rows;
  const ci = r.columns.findIndex((c) => c.name === "create_statement");
  // DESCRIBE answers: the CQL text, as cqlsh prints it.
  if (ci >= 0 && r.columns.some((c) => c.name === "keyspace_name")) {
    return <pre className="mt-1 overflow-x-auto whitespace-pre-wrap text-muted">{rows.map((row) => row[ci] ?? "").join("\n\n")}</pre>;
  }
  return (
    <div className="mt-1.5">
      {r.columns.length > 0 && (
        <div className="overflow-x-auto rounded-lg border border-line">
          <table className="w-full border-collapse">
            <thead>
              <tr className="border-b border-line text-left">
                {r.columns.map((c, i) => (
                  <th key={i} className="px-3 py-1.5 align-bottom font-normal whitespace-nowrap">
                    <span className="block text-fg">{c.name}</span>
                    <span className="block text-[10px] text-subtle">{c.type}</span>
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {rows.map((row, i) => (
                <tr key={i} className="border-b border-line last:border-0">
                  {row.map((v, j) => (
                    <td key={j} className="max-w-80 truncate px-3 py-1 align-top" title={v ?? "null"}>
                      <CqlValue value={v} />
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <p className="mt-1 flex items-center gap-3 text-subtle">
        ({rows.length} row{rows.length === 1 ? "" : "s"})
        {pageState && (
          <button onClick={onMore} disabled={busy} className="text-accent hover:underline disabled:opacity-50">
            More rows
          </button>
        )}
      </p>
    </div>
  );
}

function TraceView({ t, id }: { t: Trace; id: string | null }) {
  return (
    <div className="mt-2 rounded-lg border border-line">
      <p className="flex items-center justify-between gap-3 border-b border-line px-3 py-1.5 text-[11px] text-subtle">
        <span>Tracing session{id ? <span className="ml-1.5 text-muted">{id}</span> : null}</span>
        {t.events && t.events.length > 0 && <span>{t.events.length} events</span>}
      </p>
      {t.state === "loading" && (
        <p className="px-3 py-2 text-subtle">
          <span className="shimmer">Reading trace events…</span>
        </p>
      )}
      {t.state === "none" && <p className="px-3 py-2 text-subtle">The server returned no trace for this statement.</p>}
      {t.state === "error" && <p className="px-3 py-2 text-danger">{t.error}</p>}
      {t.state === "done" &&
        (t.events!.length === 0 ? (
          <p className="px-3 py-2 text-subtle">No trace events were recorded.</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse">
              <thead>
                <tr className="border-b border-line text-left text-[10px] text-subtle">
                  <th className="px-3 py-1 font-normal">activity</th>
                  <th className="px-3 py-1 font-normal">source</th>
                  <th className="px-3 py-1 text-right font-normal">elapsed µs</th>
                  <th className="px-3 py-1 font-normal">thread</th>
                </tr>
              </thead>
              <tbody>
                {t.events!.map((ev, i) => (
                  <tr key={i} className="border-b border-line last:border-0">
                    <td className="px-3 py-1 text-fg">{ev[0]}</td>
                    <td className="px-3 py-1 text-muted">{ev[1]}</td>
                    <td className="px-3 py-1 text-right text-muted tabular-nums">{ev[2]}</td>
                    <td className="px-3 py-1 text-subtle">{ev[3]}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ))}
    </div>
  );
}
