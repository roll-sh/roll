"use client";

import { HorizonMark } from "@roll/brand";
import { Play, RefreshCw, Search, SquareTerminal, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { formatBytes } from "@/lib/roll-api";
import {
  bytesText,
  dataApi,
  formatCount,
  quote,
  respArray,
  respText,
  splitArgs,
  type RedisDb,
  type Resp,
} from "@/lib/redis-api";
import { useMarkState } from "../mark-state";
import { btn, CopyButton, input, Panel, primary, useCopy } from "../ui";

type Api = ReturnType<typeof dataApi>;

/** Keys fetched per scan page; values fetched per page of a hash, list, set, sorted set or stream. */
const KEY_PAGE = 100;
const VALUE_PAGE = 100;
const STREAM_PAGE = 50;
/** Strings beyond this are shown truncated (GETRANGE), JSON documents beyond it cut. */
const STRING_LIMIT = 64 * 1024;

const collator = new Intl.Collator("en", { numeric: true });

const TYPES = ["string", "hash", "list", "set", "zset", "stream", "ReJSON-RL"];

const MODULE_HINTS: Record<string, string> = {
  "ReJSON-RL": "JSON.GET",
  "TSDB-TYPE": "TS.INFO",
  "MBbloom--": "BF.INFO",
  MBbloomCF: "CF.INFO",
  "TopK-TYPE": "TOPK.INFO",
  "CMSk-TYPE": "CMS.INFO",
  "TDIS-TYPE": "TDIGEST.INFO",
  vectorset: "VINFO",
};

function fail(r: Resp | undefined) {
  if (r?.t === "error") throw new Error(r.v);
  return r;
}

/** Key browser and console over the database's HTTP data API (with its REST token). */
export function DataBrowser({ db }: { db: RedisDb }) {
  const { rest_url, rest_token } = db;
  const api = useMemo(() => dataApi({ rest_url, rest_token }), [rest_url, rest_token]);
  const [prefill, setPrefill] = useState<{ line: string; n: number } | null>(null);
  const counter = useRef(0);
  const openInConsole = (line: string) => {
    setPrefill({ line, n: ++counter.current });
    requestAnimationFrame(() => document.getElementById("redis-console")?.scrollIntoView({ block: "nearest" }));
  };
  return (
    <div className="space-y-4">
      <KeyBrowser api={api} active={db.status.state === "active"} openInConsole={openInConsole} />
      <Console api={api} name={db.name} prefill={prefill} />
    </div>
  );
}

// --- Key browser ----------------------------------------------------------------------------------

interface KeyRow {
  key: string;
  type: string;
  /** PTTL: -1 no expiry, -2 gone. */
  ttl: number;
  size: number | null;
}

function KeyBrowser({ api, active, openInConsole }: { api: Api; active: boolean; openInConsole: (l: string) => void }) {
  const [pattern, setPattern] = useState("*");
  const [type, setType] = useState("");
  const [keys, setKeys] = useState<KeyRow[] | null>(null);
  const [cursor, setCursor] = useState("0");
  const [scanning, setScanning] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const autoScanned = useRef(false);

  const scan = useCallback(
    async (from: string) => {
      setScanning(true);
      setErr(null);
      try {
        let cur = from;
        const found: string[] = [];
        let rounds = 0;
        do {
          const args = ["SCAN", cur, "MATCH", pattern || "*", "COUNT", "500"];
          if (type) args.push("TYPE", type);
          const [c, ks] = respArray(fail(await api.run(args)));
          cur = respText(c) ?? "0";
          found.push(...respArray(ks).map((k) => respText(k) ?? ""));
          rounds++;
        } while (cur !== "0" && found.length < KEY_PAGE && rounds < 50);
        const details = await api.pipeline(found.flatMap((k) => [["TYPE", k], ["PTTL", k], ["MEMORY", "USAGE", k]]));
        const rows: KeyRow[] = found.map((key, i) => {
          const [t, ttl, size] = details.slice(i * 3, i * 3 + 3);
          return {
            key,
            type: respText(t) ?? "?",
            ttl: Number(respText(ttl) ?? -1),
            size: size?.t === "int" ? Number(size.v) : null,
          };
        });
        setKeys((prev) => {
          const base = from === "0" ? [] : (prev ?? []);
          const seen = new Set(base.map((r) => r.key));
          const merged = [...base, ...rows.filter((r) => !seen.has(r.key) && r.ttl !== -2)];
          return merged.sort((a, b) => collator.compare(a.key, b.key));
        });
        setCursor(cur);
        if (from === "0") setSelected(null);
      } catch (e) {
        setErr(e instanceof Error ? e.message : String(e));
      } finally {
        setScanning(false);
      }
    },
    [api, pattern, type],
  );

  // Browse right away only if the database is already loaded: scanning wakes it from zero.
  useEffect(() => {
    if (active && !autoScanned.current) {
      autoScanned.current = true;
      scan("0");
    }
  }, [active, scan]);

  const row = keys?.find((k) => k.key === selected) ?? null;

  return (
    <Panel
      title="Keys"
      aside={
        keys && (
          <span className="font-mono text-[12px] text-subtle">
            {formatCount(keys.length)}
            {cursor !== "0" ? "+" : ""}
          </span>
        )
      }
    >
      <form
        className="flex flex-wrap items-center gap-2 border-b border-line px-5 py-3"
        onSubmit={(e) => {
          e.preventDefault();
          scan("0");
        }}
      >
        <Search className="size-3.5 text-subtle" />
        <input
          value={pattern}
          onChange={(e) => setPattern(e.target.value)}
          placeholder="MATCH pattern, e.g. user:*"
          spellCheck={false}
          aria-label="Key pattern"
          className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
        />
        <select value={type} onChange={(e) => setType(e.target.value)} aria-label="Key type" className={`${input} text-[12px]`}>
          <option value="">all types</option>
          {TYPES.map((t) => (
            <option key={t} value={t}>
              {t === "ReJSON-RL" ? "json" : t}
            </option>
          ))}
        </select>
        <button type="submit" disabled={scanning} className={btn}>
          {scanning ? <span className="shimmer">Scanning…</span> : "Scan"}
        </button>
      </form>

      {err && <p className="rise border-b border-line px-5 py-3 font-mono text-[12px] text-danger">{err}</p>}

      {keys === null ? (
        <p className="px-5 py-6 text-[13px] text-muted">
          {scanning ? (
            <span className="shimmer">{active ? "Scanning…" : "Waking the database from zero…"}</span>
          ) : active ? (
            "Scan to list keys."
          ) : (
            <>
              This database is scaled to zero. <span className="text-fg">Scan</span> loads it into memory (it scales back
              to zero once idle).
            </>
          )}
        </p>
      ) : (
        <div className="grid min-h-80 lg:grid-cols-5">
          <div className="min-w-0 border-line lg:col-span-2 lg:border-r">
            {keys.length === 0 ? (
              <p className="px-5 py-6 text-[13px] text-muted">No keys match.</p>
            ) : (
              <ul className="max-h-[32rem] overflow-y-auto">
                {keys.map((k) => (
                  <li key={k.key}>
                    <button
                      onClick={() => setSelected(k.key)}
                      className={`flex w-full items-center gap-2 border-b border-line px-5 py-2 text-left transition ${
                        k.key === selected ? "bg-panel-2" : "hover:bg-panel-2/60"
                      }`}
                    >
                      <TypeBadge type={k.type} />
                      <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{k.key}</span>
                      <span className="shrink-0 font-mono text-[10px] text-subtle">
                        {k.ttl >= 0 ? formatTtl(k.ttl) : ""}
                        {k.ttl >= 0 && k.size != null ? " · " : ""}
                        {k.size != null ? formatBytes(k.size) : ""}
                      </span>
                    </button>
                  </li>
                ))}
              </ul>
            )}
            {cursor !== "0" && (
              <div className="px-5 py-3">
                <button onClick={() => scan(cursor)} disabled={scanning} className={btn}>
                  {scanning ? <span className="shimmer">Scanning…</span> : "Load more keys"}
                </button>
              </div>
            )}
          </div>
          <div className="min-w-0 border-t border-line lg:col-span-3 lg:border-t-0">
            {row ? (
              <KeyValue
                key={row.key}
                api={api}
                row={row}
                openInConsole={openInConsole}
                onDeleted={() => {
                  setKeys((ks) => ks?.filter((k) => k.key !== row.key) ?? null);
                  setSelected(null);
                }}
              />
            ) : (
              <p className="px-5 py-6 text-[13px] text-subtle">Select a key to see its value.</p>
            )}
          </div>
        </div>
      )}
    </Panel>
  );
}

function TypeBadge({ type }: { type: string }) {
  const label = type === "ReJSON-RL" ? "json" : type.length > 8 ? type.slice(0, 7) + "…" : type;
  return (
    <span className="w-12 shrink-0 rounded border border-line px-1 text-center font-mono text-[10px] text-muted" title={type}>
      {label}
    </span>
  );
}

function formatTtl(ms: number) {
  if (ms < 0) return "no expiry";
  const s = Math.ceil(ms / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

type Page =
  | { kind: "text"; text: string; total: number; json: boolean }
  | { kind: "rows"; columns: string[]; rows: string[][]; total: number; next: string | null }
  | { kind: "unknown" };

/** Load one page of a value: `from` is a cursor (hash/set), an index (list/zset) or a stream id. */
async function loadPage(api: Api, key: string, type: string, from: string | null): Promise<Page> {
  switch (type) {
    case "string": {
      const [len, val] = await api.pipeline([
        ["STRLEN", key],
        ["GETRANGE", key, "0", String(STRING_LIMIT - 1)],
      ]);
      const text = respText(fail(val)) ?? "";
      const total = Number(respText(len) ?? 0);
      return { kind: "text", text, total, json: total <= STRING_LIMIT && looksJson(text) };
    }
    case "ReJSON-RL": {
      const r = fail(await api.run(["JSON.GET", key]));
      const text = respText(r) ?? "null";
      return { kind: "text", text: text.slice(0, STRING_LIMIT), total: text.length, json: text.length <= STRING_LIMIT };
    }
    case "hash":
    case "set": {
      const hash = type === "hash";
      const [len, page] = await api.pipeline([
        [hash ? "HLEN" : "SCARD", key],
        [hash ? "HSCAN" : "SSCAN", key, from ?? "0", "COUNT", String(VALUE_PAGE)],
      ]);
      const [c, items] = respArray(fail(page));
      const flat = respArray(items).map((x) => respText(x) ?? "");
      const rows = hash ? pairs(flat) : flat.map((m) => [m]);
      const next = respText(c) ?? "0";
      return {
        kind: "rows",
        columns: hash ? ["field", "value"] : ["member"],
        rows,
        total: Number(respText(len) ?? 0),
        next: next === "0" ? null : next,
      };
    }
    case "list":
    case "zset": {
      const list = type === "list";
      const start = Number(from ?? 0);
      const stop = String(start + VALUE_PAGE - 1);
      const [len, page] = await api.pipeline([
        [list ? "LLEN" : "ZCARD", key],
        list ? ["LRANGE", key, String(start), stop] : ["ZRANGE", key, String(start), stop, "WITHSCORES"],
      ]);
      const flat = respArray(fail(page)).map((x) => respText(x) ?? "");
      const total = Number(respText(len) ?? 0);
      const rows = list ? flat.map((v, i) => [String(start + i), v]) : pairs(flat).map(([m, s], i) => [String(start + i), m, s]);
      return {
        kind: "rows",
        columns: list ? ["index", "value"] : ["rank", "member", "score"],
        rows,
        total,
        next: start + VALUE_PAGE < total ? String(start + VALUE_PAGE) : null,
      };
    }
    case "stream": {
      const [len, page] = await api.pipeline([
        ["XLEN", key],
        ["XRANGE", key, from ? `(${from}` : "-", "+", "COUNT", String(STREAM_PAGE)],
      ]);
      const entries = respArray(fail(page)).map((e) => {
        const [id, fields] = respArray(e);
        const f = respArray(fields).map((x) => respText(x) ?? "");
        return [respText(id) ?? "", pairs(f).map(([k, v]) => `${k}=${v}`).join("  ")];
      });
      const last = entries.at(-1)?.[0];
      return {
        kind: "rows",
        columns: ["id", "fields"],
        rows: entries,
        total: Number(respText(len) ?? 0),
        next: entries.length === STREAM_PAGE && last ? last : null,
      };
    }
    default:
      return { kind: "unknown" };
  }
}

function pairs(flat: string[]) {
  const out: string[][] = [];
  for (let i = 0; i + 1 < flat.length; i += 2) out.push([flat[i], flat[i + 1]]);
  return out;
}

function looksJson(s: string) {
  const t = s.trim();
  if (!(t.startsWith("{") || t.startsWith("["))) return false;
  try {
    JSON.parse(t);
    return true;
  } catch {
    return false;
  }
}

function pretty(s: string) {
  try {
    return JSON.stringify(JSON.parse(s), null, 2);
  } catch {
    return s;
  }
}

function KeyValue({
  api,
  row,
  openInConsole,
  onDeleted,
}: {
  api: Api;
  row: KeyRow;
  openInConsole: (line: string) => void;
  onDeleted: () => void;
}) {
  const [page, setPage] = useState<Page | null>(null);
  const [ttl, setTtl] = useState(row.ttl);
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [raw, setRaw] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const { copied, copy } = useCopy();

  const load = useCallback(
    async (from: string | null) => {
      setLoading(true);
      setErr(null);
      try {
        const [p, t] = await Promise.all([loadPage(api, row.key, row.type, from), api.run(["PTTL", row.key])]);
        setTtl(Number(respText(t) ?? -1));
        setPage((prev) =>
          from && prev?.kind === "rows" && p.kind === "rows" ? { ...p, rows: [...prev.rows, ...p.rows] } : p,
        );
      } catch (e) {
        setErr(e instanceof Error ? e.message : String(e));
      } finally {
        setLoading(false);
      }
    },
    [api, row.key, row.type],
  );

  useEffect(() => {
    load(null);
  }, [load]);

  const hint = MODULE_HINTS[row.type];

  return (
    <div className="flex h-full flex-col">
      <div className="space-y-2 border-b border-line px-5 py-3">
        <div className="flex items-start gap-2">
          <p className="min-w-0 flex-1 font-mono text-[12px] break-all">{row.key}</p>
          <CopyButton id="key" text={row.key} copied={copied} onCopy={copy} />
          <button onClick={() => load(null)} title="Reload" aria-label="Reload" className="p-1 text-subtle hover:text-fg">
            <RefreshCw className="size-3.5" />
          </button>
          <button
            onClick={() => setConfirmDelete(true)}
            title="Delete key"
            aria-label="Delete key"
            className="p-1 text-subtle hover:text-danger"
          >
            <Trash2 className="size-3.5" />
          </button>
        </div>
        <p className="flex flex-wrap gap-x-3 font-mono text-[11px] text-subtle">
          <span>{row.type}</span>
          <span>{ttl === -2 ? "expired" : ttl < 0 ? "no expiry" : `expires in ${formatTtl(ttl)}`}</span>
          {row.size != null && <span>{formatBytes(row.size)} in memory</span>}
          {page && page.kind !== "unknown" && (
            <span>
              {page.kind === "text"
                ? `${formatBytes(page.total)}`
                : `${formatCount(page.total)} ${row.type === "hash" ? "fields" : row.type === "stream" ? "entries" : "items"}`}
            </span>
          )}
        </p>
        {confirmDelete && (
          <div className="rise flex flex-wrap items-center gap-2 rounded-lg border border-danger/40 bg-danger/5 px-3 py-2 text-[12px]">
            <span className="flex-1 text-muted">Delete this key? This can&apos;t be undone.</span>
            <button
              onClick={async () => {
                const r = await api.run(["UNLINK", row.key]).catch((e: Error) => ({ t: "error", v: e.message }) as Resp);
                if (r.t === "error") setErr(r.v);
                else onDeleted();
              }}
              className="rounded-md bg-danger px-2 py-1 font-medium text-white"
            >
              Delete
            </button>
            <button onClick={() => setConfirmDelete(false)} className="px-2 py-1 text-subtle hover:text-fg">
              Cancel
            </button>
          </div>
        )}
      </div>

      {err && <p className="px-5 py-3 font-mono text-[12px] text-danger">{err}</p>}

      <div className="min-h-0 flex-1">
        {!page ? (
          loading && (
            <p className="px-5 py-4 text-[13px] text-subtle">
              <span className="shimmer">Loading value…</span>
            </p>
          )
        ) : page.kind === "text" ? (
          <div>
            {page.json && (
              <div className="flex justify-end px-5 pt-2">
                <button onClick={() => setRaw((r) => !r)} className="text-[11px] text-subtle hover:text-fg">
                  {raw ? "Format JSON" : "Raw"}
                </button>
              </div>
            )}
            <pre className="max-h-[28rem] overflow-auto px-5 py-3 font-mono text-[12px] leading-relaxed break-all whitespace-pre-wrap">
              {page.json && !raw ? pretty(page.text) : page.text}
            </pre>
            {page.total > page.text.length && (
              <p className="border-t border-line px-5 py-2 text-[11px] text-subtle">
                Showing the first {formatBytes(page.text.length)} of {formatBytes(page.total)}.
              </p>
            )}
          </div>
        ) : page.kind === "rows" ? (
          <div>
            <div className="max-h-[28rem] overflow-auto">
              <table className="w-full text-left font-mono text-[11px]">
                <thead className="sticky top-0 bg-panel">
                  <tr>
                    {page.columns.map((c) => (
                      <th key={c} className="border-b border-line px-5 py-2 font-medium text-muted">
                        {c}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {page.rows.map((r, i) => (
                    <tr key={i} className="border-b border-line last:border-0 align-top">
                      {r.map((v, j) => (
                        <Cell key={j} value={v} narrow={j === 0 && page.columns.length > 1 && ["index", "rank", "id"].includes(page.columns[0])} />
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
              {page.rows.length === 0 && <p className="px-5 py-4 text-[13px] text-subtle">Empty.</p>}
            </div>
            <div className="flex items-center justify-between border-t border-line px-5 py-2 text-[11px] text-subtle">
              <span className="font-mono">
                {formatCount(page.rows.length)} of {formatCount(page.total)}
              </span>
              {page.next && (
                <button onClick={() => load(page.next)} disabled={loading} className={btn}>
                  {loading ? <span className="shimmer">Loading…</span> : "Load more"}
                </button>
              )}
            </div>
          </div>
        ) : (
          <div className="space-y-3 px-5 py-4 text-[13px] text-muted">
            <p>
              No viewer for <span className="font-mono text-fg">{row.type}</span> values yet.
            </p>
            {hint && (
              <button onClick={() => openInConsole(`${hint} ${quoteArg(row.key)}`)} className={btn}>
                <SquareTerminal className="size-3.5" /> {hint} in the console
              </button>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

/** A value cell: clamped to three lines, click to expand. */
function Cell({ value, narrow }: { value: string; narrow?: boolean }) {
  const [open, setOpen] = useState(false);
  const long = value.length > 160 || value.includes("\n");
  return (
    <td
      onClick={() => long && setOpen((o) => !o)}
      className={`px-5 py-1.5 ${narrow ? "w-16 text-subtle" : ""} ${long ? "cursor-pointer" : ""}`}
      title={long && !open ? "Click to expand" : undefined}
    >
      <div className={`break-all whitespace-pre-wrap ${open ? "" : "line-clamp-3"}`}>{value}</div>
    </td>
  );
}

function quoteArg(s: string) {
  return /^[^\s"'\\]+$/.test(s) ? s : quote(s);
}

// --- Console ----------------------------------------------------------------------------------------

interface Entry {
  id: number;
  line: string;
  reply?: Resp;
  error?: string;
  ms: number;
}

const STREAMING = new Set(["SUBSCRIBE", "PSUBSCRIBE", "SSUBSCRIBE", "MONITOR"]);
const EXAMPLES = ["PING", "DBSIZE", "INFO keyspace", "SCAN 0 COUNT 20", "MEMORY STATS"];
/** Most elements and lines rendered per reply; the rest is summarised. */
const MAX_ITEMS = 500;
const MAX_LINES = 2000;
const MAX_STRING = 16 * 1024;

function Console({ api, name, prefill }: { api: Api; name: string; prefill: { line: string; n: number } | null }) {
  const { setState, play } = useMarkState();
  const [line, setLine] = useState("");
  const [entries, setEntries] = useState<Entry[]>([]);
  const [running, setRunning] = useState(false);
  const history = useRef<string[]>([]);
  const cursor = useRef(-1);
  const out = useRef<HTMLDivElement>(null);
  const field = useRef<HTMLInputElement>(null);
  const id = useRef(0);

  useEffect(() => {
    if (prefill) {
      setLine(prefill.line);
      field.current?.focus();
    }
  }, [prefill]);

  useEffect(() => {
    out.current?.scrollTo({ top: out.current.scrollHeight });
  }, [entries]);

  const exec = async (text: string) => {
    const l = text.trim();
    if (!l || running) return;
    if (/^clear$/i.test(l)) {
      setEntries([]);
      setLine("");
      return;
    }
    history.current = [l, ...history.current.filter((h) => h !== l)].slice(0, 100);
    cursor.current = -1;
    setLine("");
    const n = ++id.current;
    let args: string[];
    try {
      args = splitArgs(l);
    } catch (e) {
      setEntries((es) => [...es, { id: n, line: l, error: e instanceof Error ? e.message : String(e), ms: 0 }]);
      return;
    }
    if (STREAMING.has(args[0]?.toUpperCase())) {
      setEntries((es) => [
        ...es,
        { id: n, line: l, error: `${args[0].toUpperCase()} streams; use the REST API's /subscribe or /monitor endpoints`, ms: 0 },
      ]);
      return;
    }
    setRunning(true);
    setState("thinking");
    const t0 = performance.now();
    try {
      const reply = await api.run(args);
      setEntries((es) => [...es.slice(-199), { id: n, line: l, reply, ms: performance.now() - t0 }]);
      play([[reply.t === "error" ? "error" : "success", reply.t === "error" ? 1400 : 1200]]);
    } catch (e) {
      setEntries((es) => [...es.slice(-199), { id: n, line: l, error: e instanceof Error ? e.message : String(e), ms: performance.now() - t0 }]);
      play([["error", 1400]]);
    } finally {
      setRunning(false);
      requestAnimationFrame(() => field.current?.focus());
    }
  };

  return (
    <div id="redis-console">
      <Panel
        title="Console"
        aside={
          <div className="flex items-center gap-1">
            {EXAMPLES.map((e) => (
              <button
                key={e}
                onClick={() => exec(e)}
                disabled={running}
                className="hidden rounded px-1.5 font-mono text-[11px] text-subtle hover:text-fg sm:inline"
              >
                {e.split(" ")[0].toLowerCase()}
              </button>
            ))}
            {entries.length > 0 && (
              <button onClick={() => setEntries([])} title="Clear" aria-label="Clear" className="p-1 text-subtle hover:text-fg">
                <X className="size-3.5" />
              </button>
            )}
          </div>
        }
      >
        <div ref={out} className="max-h-[28rem] min-h-40 overflow-y-auto px-5 py-3 font-mono text-[12px] leading-relaxed">
          {entries.length === 0 && (
            <p className="text-subtle">
              Run any Redis command over the REST API, like <span className="text-muted">redis-cli</span>. ↑ ↓ for
              history, <span className="text-muted">clear</span> to clear.
            </p>
          )}
          {entries.map((e) => (
            <div key={e.id} className="rise py-1">
              <p className="flex gap-2">
                <span className="text-subtle">{name}&gt;</span>
                <span className="min-w-0 flex-1 break-all">{e.line}</span>
                <span className="shrink-0 text-[10px] text-subtle">{e.ms ? `${e.ms.toFixed(0)} ms` : ""}</span>
              </p>
              {e.error ? (
                <p className="text-danger">(error) {e.error}</p>
              ) : (
                e.reply && <Reply reply={e.reply} />
              )}
            </div>
          ))}
        </div>
        <form
          className="flex items-center gap-2 border-t border-line px-5 py-3"
          onSubmit={(e) => {
            e.preventDefault();
            exec(line);
          }}
        >
          <span className="font-mono text-[12px] text-subtle">{name}&gt;</span>
          <input
            ref={field}
            value={line}
            onChange={(e) => setLine(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "ArrowUp" || e.key === "ArrowDown") {
                const h = history.current;
                if (!h.length) return;
                e.preventDefault();
                cursor.current = Math.max(-1, Math.min(h.length - 1, cursor.current + (e.key === "ArrowUp" ? 1 : -1)));
                setLine(cursor.current < 0 ? "" : h[cursor.current]);
              } else if (e.key === "l" && e.ctrlKey) {
                e.preventDefault();
                setEntries([]);
              }
            }}
            placeholder='SET greeting "hello from roll"'
            spellCheck={false}
            autoComplete="off"
            aria-label="Redis command"
            className="min-w-0 flex-1 bg-transparent font-mono text-[12px] outline-none placeholder:text-subtle"
          />
          <button type="submit" disabled={running || !line.trim()} className={primary}>
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
        </form>
      </Panel>
    </div>
  );
}

interface Line {
  text: string;
  kind: "plain" | "error" | "meta";
}

/** A reply the way redis-cli prints it: numbered nested arrays, (integer), (nil), (error). */
function renderResp(r: Resp, out: Line[]): Line[] {
  const push = (text: string, kind: Line["kind"] = "plain") => out.push({ text, kind });
  switch (r.t) {
    case "status":
      push(r.v);
      break;
    case "error":
      push(`(error) ${r.v}`, "error");
      break;
    case "int":
      push(`(integer) ${r.v}`);
      break;
    case "double":
      push(`(double) ${r.v}`);
      break;
    case "bool":
      push(r.v ? "(true)" : "(false)");
      break;
    case "nil":
      push("(nil)", "meta");
      break;
    case "bulk": {
      const cut = r.v.length > MAX_STRING;
      push(quote(bytesText(cut ? r.v.subarray(0, MAX_STRING) : r.v)) + (cut ? ` … (${formatBytes(r.v.length - MAX_STRING)} more)` : ""));
      break;
    }
    case "array":
    case "map": {
      const items: [string, Resp][] =
        r.t === "array"
          ? r.v.map((x, i) => [`${i + 1})`, x])
          : r.v.flatMap(([k, v], i) => [[`${i * 2 + 1})`, k] as [string, Resp], [`${i * 2 + 2})`, v] as [string, Resp]]);
      if (items.length === 0) {
        push(r.t === "array" ? "(empty array)" : "(empty hash)", "meta");
        break;
      }
      const width = String(items.length).length + 1;
      for (const [label, x] of items.slice(0, MAX_ITEMS)) {
        if (out.length > MAX_LINES) break;
        const prefix = `${label.padStart(width)} `;
        const sub = renderResp(x, []);
        sub.forEach((l, i) => push((i === 0 ? prefix : " ".repeat(prefix.length)) + l.text, l.kind));
      }
      if (items.length > MAX_ITEMS) push(`… ${formatCount(items.length - MAX_ITEMS)} more elements`, "meta");
      break;
    }
  }
  return out;
}

function Reply({ reply }: { reply: Resp }) {
  const lines = useMemo(() => renderResp(reply, []), [reply]);
  const shown = lines.slice(0, MAX_LINES);
  return (
    <pre className="font-mono text-[12px] break-all whitespace-pre-wrap">
      {shown.map((l, i) => (
        <span key={i} className={l.kind === "error" ? "text-danger" : l.kind === "meta" ? "text-subtle" : "text-muted"}>
          {l.text}
          {i < shown.length - 1 ? "\n" : ""}
        </span>
      ))}
      {lines.length > MAX_LINES && <span className="text-subtle">… {formatCount(lines.length - MAX_LINES)} more lines</span>}
    </pre>
  );
}
