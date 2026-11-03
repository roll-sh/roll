"use client";

import { Check, Copy, History, Link2 } from "lucide-react";
import Link from "next/link";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { blameNote, blameRows, isBlameKey, type BlameRow } from "@/lib/blame";
import { codeApi, firstLine, shortId, type Blame } from "@/lib/code-api";
import type { Token } from "@/lib/highlight";
import { ago } from "@/lib/time";
import { changesetHref } from "./paths";

type Range = [number, number] | null;

/** `#L10` or `#L10-L20` → [10, 20]. */
function parseHash(hash: string): Range {
  const m = /^#L(\d+)(?:-L?(\d+))?$/.exec(hash);
  if (!m) return null;
  const a = Number(m[1]);
  const b = m[2] ? Number(m[2]) : a;
  return [Math.min(a, b), Math.max(a, b)];
}

const toHash = (r: Range) => (!r ? "" : r[0] === r[1] ? `#L${r[0]}` : `#L${r[0]}-L${r[1]}`);

/**
 * A file with line numbers. Click a number to select a line, shift-click to extend; the
 * selection lives in the URL hash (`#L10-L20`) so it can be shared, and `y` pins it to a
 * changeset. Highlighted tokens come from the server; plain text otherwise. With `blame`, a Blame
 * toggle (or `b`) adds a gutter naming the changeset that last changed each run of lines.
 */
export function CodeLines({
  tokens,
  text,
  permalink,
  info,
  actions,
  blame,
}: {
  tokens: Token[][] | null;
  text: string;
  permalink: string | null;
  /** Left of the toolbar: lines, size, language. */
  info?: ReactNode;
  /** Extra toolbar buttons (Raw, History). */
  actions?: ReactNode;
  /** Where to read blame from; without it there's no Blame toggle. */
  blame?: { repo: string; rev: string; path: string };
}) {
  const lines = useMemo(() => {
    const plain = text.replace(/\n$/, "").split("\n");
    return tokens ? tokens.slice(0, plain.length) : plain;
  }, [tokens, text]);
  const [range, setRange] = useState<Range>(null);
  const anchor = useRef<number | null>(null);
  const [copied, setCopied] = useState<"code" | "link" | null>(null);

  useEffect(() => {
    const read = (scroll: boolean) => {
      const r = parseHash(window.location.hash);
      setRange(r);
      if (r && scroll) document.getElementById(`L${r[0]}`)?.scrollIntoView({ block: "center" });
    };
    read(true);
    const onHash = () => read(false);
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  const select = useCallback((n: number, extend: boolean) => {
    const r: Range = extend && anchor.current != null ? [Math.min(anchor.current, n), Math.max(anchor.current, n)] : [n, n];
    if (!extend) anchor.current = n;
    setRange(r);
    window.history.replaceState(null, "", `${window.location.pathname}${window.location.search}${toHash(r)}`);
  }, []);

  const bl = useBlame(blame, lines.length);

  const flash = (k: "code" | "link") => {
    setCopied(k);
    setTimeout(() => setCopied((c) => (c === k ? null : c)), 1200);
  };

  return (
    <div className="min-w-0">
      <div className="flex flex-wrap items-center gap-1 border-b border-line px-3 py-1.5">
        <div className="mr-auto min-w-0 pl-1 font-mono text-[11px] text-subtle">{info}</div>
        {actions}
        {blame && <BlameToggle on={bl.on} onToggle={bl.toggle} />}
        {permalink && (
          <button
            type="button"
            onClick={() => {
              navigator.clipboard?.writeText(`${window.location.origin}${permalink}${toHash(range)}`);
              flash("link");
            }}
            className="inline-flex items-center gap-1.5 rounded-md px-2 py-1 text-[12px] text-subtle transition hover:bg-panel-2 hover:text-fg"
            title="Copy permalink (y pins the URL)"
          >
            {copied === "link" ? <Check className="size-3.5 text-ok" /> : <Link2 className="size-3.5" />}
            Permalink
          </button>
        )}
        <button
          type="button"
          onClick={() => {
            navigator.clipboard?.writeText(text);
            flash("code");
          }}
          className="inline-flex items-center gap-1.5 rounded-md px-2 py-1 text-[12px] text-subtle transition hover:bg-panel-2 hover:text-fg"
        >
          {copied === "code" ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
          Copy
        </button>
      </div>
      <BlameNoteBar blame={bl} />
      <div className="overflow-x-auto py-2">
        <table className="code w-full border-collapse font-mono text-[12.5px] leading-[1.65]">
          <tbody>
            {lines.map((line, i) => {
              const n = i + 1;
              const on = !!range && n >= range[0] && n <= range[1];
              const b = bl.rows?.[i];
              return (
                <tr key={n} id={`L${n}`} className={`scroll-mt-24 ${on ? "bg-accent/[0.08]" : ""}`}>
                  {bl.rows && blame && <BlameCell row={b} repo={blame.repo} now={bl.now} first={i === 0} />}
                  <td
                    className={`w-0 border-r pr-3 pl-4 text-right align-top select-none ${
                      on ? "border-accent/50 text-fg" : "border-line text-subtle"
                    }`}
                  >
                    <a
                      href={`#L${n}`}
                      onClick={(e) => {
                        e.preventDefault();
                        select(n, e.shiftKey);
                      }}
                      className="block min-w-[2.5ch] tabular-nums hover:text-fg"
                    >
                      {n}
                    </a>
                  </td>
                  <td className="pr-6 pl-4 whitespace-pre">
                    {typeof line === "string"
                      ? line || " "
                      : line.length === 0
                        ? " "
                        : line.map((t, j) =>
                            t.c ? (
                              <span key={j} className={t.c}>
                                {t.t}
                              </span>
                            ) : (
                              t.t
                            ),
                          )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}

// ── blame ──

export interface BlameState {
  on: boolean;
  toggle: () => void;
  /** One row per line while blame is on and loaded (and the file isn't binary). */
  rows: BlameRow[] | null;
  data: Blame | null;
  error: string | null;
  note: string | null;
  now: number;
}

/**
 * Blame for one file, read when first switched on (and again when the file changes). `b` toggles it
 * unless the user is typing.
 */
export function useBlame(blame: { repo: string; rev: string; path: string } | undefined, lines: number): BlameState {
  const [on, setOn] = useState(false);
  const [data, setData] = useState<Blame | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [bRepo, bRev, bPath] = [blame?.repo, blame?.rev, blame?.path];
  useEffect(() => {
    setData(null);
    setError(null);
  }, [bRepo, bRev, bPath]);
  useEffect(() => {
    if (!bRepo || !bRev || !bPath || !on || data) return;
    let alive = true;
    codeApi.blame(bRepo, bRev, bPath).then(
      (b) => alive && (setData(b), setError(null)),
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [bRepo, bRev, bPath, on, data]);
  const enabled = !!blame;
  useEffect(() => {
    if (!enabled) return;
    const onKey = (e: KeyboardEvent) => {
      if (!isBlameKey(e, e.target as HTMLElement | null)) return;
      e.preventDefault();
      setOn((v) => !v);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [enabled]);
  const rows = useMemo(() => (on && data && !data.binary ? blameRows(data, lines) : null), [on, data, lines]);
  const toggle = useCallback(() => setOn((v) => !v), []);
  return { on, toggle, rows, data, error, note: on && data ? blameNote(data) : null, now: Date.now() / 1000 };
}

export function BlameToggle({ on, onToggle }: { on: boolean; onToggle: () => void }) {
  return (
    <button
      type="button"
      onClick={onToggle}
      aria-pressed={on}
      className={`inline-flex items-center gap-1.5 rounded-md px-2 py-1 text-[12px] transition hover:bg-panel-2 hover:text-fg ${on ? "bg-panel-2 text-fg" : "text-subtle"}`}
      title="Blame (b)"
    >
      <History className="size-3.5" />
      Blame
    </button>
  );
}

/** Loading, errors, and the notes for binary files and truncated history. */
export function BlameNoteBar({ blame }: { blame: BlameState }) {
  if (!blame.on || (!blame.error && !blame.note && blame.data)) return null;
  return (
    <p className="border-b border-line px-4 py-1.5 text-[11px] text-subtle">
      {blame.error ? <span className="text-danger">{blame.error}</span> : !blame.data ? <span className="shimmer">Reading history…</span> : blame.note}
    </p>
  );
}

/** The gutter cell: the changeset, author and age at the start of each run, in alternating bands. */
export function BlameCell({ row, repo, now, first }: { row: BlameRow | undefined; repo: string; now: number; first: boolean }) {
  return (
    <td
      className={`w-0 max-w-72 min-w-56 border-r border-line px-3 align-top text-[11px] whitespace-nowrap select-none ${row?.band ? "bg-panel-2/50" : ""} ${
        row?.first && !first ? "border-t border-t-line/60" : ""
      }`}
    >
      {row?.first && row.range ? (
        <Link
          href={changesetHref(repo, row.range.changeset)}
          title={`${firstLine(row.range.message) || "(no message)"}\n${row.range.author.name} · ${shortId(row.range.changeset)}`}
          className="flex items-center gap-2 text-subtle transition hover:text-fg"
        >
          <span className="text-muted">{shortId(row.range.changeset)}</span>
          <span className="min-w-0 flex-1 truncate font-sans">{row.range.author.name}</span>
          <span className="shrink-0 font-sans">{ago(row.range.time, now)}</span>
        </Link>
      ) : null}
    </td>
  );
}
