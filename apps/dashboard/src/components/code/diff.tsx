"use client";

import { ChevronDown, ChevronRight, MessageSquare } from "lucide-react";
import { Fragment, useEffect, useMemo, useState, type ReactNode } from "react";
import type { FileDiff, Thread, ThreadSide } from "@/lib/code-api";
import { parsePatch, splitRows, type Line, type Side } from "@/lib/diff";
import {
  draftsOnRow,
  endsSelection,
  inSelection,
  newSideText,
  placeSplit,
  placeUnified,
  rowSpots,
  selectLine,
  stepThread,
  threadOrder,
  threadsForFile,
  type DraftComment,
  type LineSelection,
  type PendingReview,
  type Placement,
} from "@/lib/review";
import { ThreadCard, type ThreadActions } from "./thread";

/** Files over this many diff lines start collapsed (unless they have threads). */
const COLLAPSE_LINES = 400;

export type DiffMode = "unified" | "split";

/** Threads in the diff, and on the dashboard, writing them. */
export interface DiffReview {
  threads: Thread[];
  /** Replying, resolving, applying suggestions; absent on read-only pages. */
  actions?: ThreadActions;
  /** Commenting on lines: clickable line numbers, pending comments and the line composer. */
  compose?: {
    selection: LineSelection | null;
    onSelect: (sel: LineSelection | null) => void;
    pending: PendingReview;
    renderComposer: (sel: LineSelection, original: string[] | null) => ReactNode;
    renderDraft: (draft: DraftComment, original: string[] | null) => ReactNode;
  };
  /** The thread `n`/`p` landed on; controlled by the page when it handles more keys. */
  focused?: string | null;
  onFocus?: (thread: string | null) => void;
}

const rowCls: Record<Line["kind"], string> = {
  hunk: "bg-panel-2 text-subtle",
  add: "bg-ok/10",
  del: "bg-danger/10",
  ctx: "",
  note: "text-subtle italic",
};

const sign: Record<Line["kind"], string> = { hunk: "", add: "+", del: "−", ctx: "", note: "" };
const signCls = (k: Line["kind"]) => (k === "add" ? "text-ok" : k === "del" ? "text-danger" : "");
const num = "w-10 select-none border-r border-line px-2 text-right align-top text-[11px] text-subtle tabular-nums";
const selected = "bg-accent/10";

interface FileCtx {
  file: FileDiff;
  lines: Line[];
  review?: DiffReview;
  focused: string | null;
}

/** A line number; on the dashboard it selects the line (shift-click for a range). */
function NumCell({ n, side, ctx, className = "" }: { n: number | null; side: ThreadSide; ctx: FileCtx; className?: string }) {
  const compose = ctx.review?.compose;
  if (!compose || n == null) return <td className={`${num} ${className}`}>{n ?? ""}</td>;
  return (
    <td className={`${num} ${className} p-0`}>
      <button
        type="button"
        onClick={(e) => compose.onSelect(selectLine(compose.selection, { path: ctx.file.path, side, line: n }, e.shiftKey))}
        className="group/num relative block w-full px-2 text-right hover:text-accent"
        title="Comment on this line (shift-click for a range)"
        aria-label={`Comment on ${side === "old" ? "old " : ""}line ${n}`}
      >
        <MessageSquare className="absolute top-1/2 left-0.5 size-3 -translate-y-1/2 opacity-0 transition-opacity group-hover/num:opacity-100" />
        {n}
      </button>
    </td>
  );
}

/** What goes under a row: its threads, pending comments, and the composer when the selection ends here. */
function rowExtras(ctx: FileCtx, threads: Thread[], l: Line | null, side?: ThreadSide): ReactNode[] {
  const out: ReactNode[] = [];
  const compose = ctx.review?.compose;
  for (const t of threads)
    out.push(
      <ThreadCard
        key={t.id}
        thread={t}
        original={t.side === "new" ? newSideText(ctx.lines, t.start_line, t.line) : null}
        actions={ctx.review?.actions}
        focused={ctx.focused === t.id}
      />,
    );
  if (compose) {
    for (const d of draftsOnRow(compose.pending, ctx.file.path, l, side)) {
      const original = (d.side ?? "new") === "new" ? newSideText(ctx.lines, d.start_line ?? d.line, d.line) : null;
      out.push(<Fragment key={d.id}>{compose.renderDraft(d, original)}</Fragment>);
    }
    if (compose.selection && endsSelection(compose.selection, ctx.file.path, l, side)) {
      const sel = compose.selection;
      const original = sel.side === "new" ? newSideText(ctx.lines, sel.start, sel.end) : null;
      out.push(<Fragment key="composer">{compose.renderComposer(sel, original)}</Fragment>);
    }
  }
  return out;
}

function Unified({ ctx, placement }: { ctx: FileCtx; placement: Placement | null }) {
  const sel = ctx.review?.compose?.selection ?? null;
  return (
    <table className="w-full border-collapse font-mono text-[12px] leading-5">
      <tbody>
        {ctx.lines.map((l, i) => {
          const extras = rowExtras(ctx, placement?.inline.get(i) ?? [], l);
          const isSel = rowSpots(l).some((s) => inSelection(sel, ctx.file.path, s.side, s.line) && s.side === sel?.side);
          return (
            <Fragment key={i}>
              <tr className={isSel ? selected : rowCls[l.kind]}>
                <NumCell n={l.kind === "hunk" || l.kind === "note" ? null : l.old} side="old" ctx={ctx} />
                <NumCell n={l.kind === "hunk" || l.kind === "note" ? null : l.new} side="new" ctx={ctx} />
                <td className={`w-4 select-none pl-2 ${signCls(l.kind)}`}>{sign[l.kind]}</td>
                <td className="pr-4 pl-1 whitespace-pre">{l.text}</td>
              </tr>
              {extras.length > 0 && (
                <tr>
                  <td colSpan={4} className="border-y border-line bg-bg px-3 py-3">
                    <div className="max-w-3xl space-y-2 whitespace-normal">{extras}</div>
                  </td>
                </tr>
              )}
            </Fragment>
          );
        })}
      </tbody>
    </table>
  );
}

function Split({ ctx, threads }: { ctx: FileCtx; threads: Thread[] }) {
  const rows = useMemo(() => splitRows(ctx.lines), [ctx.lines]);
  const placement = useMemo(() => placeSplit(rows, threads), [rows, threads]);
  const sel = ctx.review?.compose?.selection ?? null;
  const cell = (l: Side, side: "old" | "new") => {
    if (!l) return <td colSpan={2} className="border-r border-line bg-panel-2/40 last:border-r-0" />;
    if (l.kind === "hunk" || l.kind === "note") {
      return side === "old" ? (
        <td colSpan={4} className={`px-3 ${rowCls[l.kind]}`}>
          {l.text}
        </td>
      ) : null;
    }
    const n = side === "old" ? l.old : l.new;
    const on = (side === "old" && (l.kind === "del" || l.kind === "ctx")) || (side === "new" && (l.kind === "add" || l.kind === "ctx"));
    const cls = inSelection(sel, ctx.file.path, side, n) && on ? selected : l.kind === "ctx" ? "" : rowCls[l.kind];
    return (
      <>
        <NumCell n={on ? n : null} side={side} ctx={ctx} className={cls} />
        <td className={`w-1/2 border-r border-line pr-3 pl-2 whitespace-pre-wrap break-all last:border-r-0 ${cls}`}>
          <span className={`mr-2 select-none ${signCls(l.kind)}`}>{sign[l.kind] || " "}</span>
          {l.text}
        </td>
      </>
    );
  };
  return (
    <table className="w-full table-fixed border-collapse font-mono text-[12px] leading-5">
      <colgroup>
        <col className="w-10" />
        <col />
        <col className="w-10" />
        <col />
      </colgroup>
      <tbody>
        {rows.map(([a, b], i) => {
          const here = placement.inline.get(i) ?? [];
          const left = rowExtras(ctx, here.filter((t) => t.side === "old"), a, "old");
          const right = rowExtras(ctx, here.filter((t) => t.side === "new"), b, "new");
          return (
            <Fragment key={i}>
              <tr>
                {cell(a, "old")}
                {cell(b, "new")}
              </tr>
              {(left.length > 0 || right.length > 0) && (
                <tr>
                  <td colSpan={2} className="border-y border-r border-line bg-bg px-3 py-3 align-top">
                    <div className="space-y-2 whitespace-normal">{left}</div>
                  </td>
                  <td colSpan={2} className="border-y border-line bg-bg px-3 py-3 align-top">
                    <div className="space-y-2 whitespace-normal">{right}</div>
                  </td>
                </tr>
              )}
            </Fragment>
          );
        })}
      </tbody>
    </table>
  );
}

export function FileDiffView({
  file,
  mode = "unified",
  review,
  focused = null,
}: {
  file: FileDiff;
  mode?: DiffMode;
  review?: DiffReview;
  focused?: string | null;
}) {
  const lines = useMemo(() => (file.patch ? parsePatch(file.patch) : []), [file.patch]);
  const threads = useMemo(() => (review ? threadsForFile(file, review.threads) : []), [file, review]);
  const placement = useMemo(() => (review ? placeUnified(lines, threads) : null), [lines, threads, review]);
  const [open, setOpen] = useState(lines.length <= COLLAPSE_LINES || threads.length > 0);
  const [showOutdated, setShowOutdated] = useState(false);
  const focusedHere = focused != null && threads.some((t) => t.id === focused);
  const focusedOutdated = focused != null && (placement?.outdated.some((t) => t.id === focused) ?? false);

  useEffect(() => {
    if (focusedHere) setOpen(true);
    if (focusedOutdated) setShowOutdated(true);
  }, [focusedHere, focusedOutdated]);

  const ctx: FileCtx = { file, lines, review, focused };
  const kind = file.change === "added" ? "A" : file.change === "deleted" ? "D" : "M";
  const unresolved = threads.filter((t) => !t.resolved).length;
  const offDiff = placement?.offDiff ?? [];
  const outdated = placement?.outdated ?? [];

  return (
    <div id={`diff-${file.path}`} className="min-w-0 scroll-mt-20 overflow-hidden rounded-lg border border-line">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 border-b border-line bg-panel-2 px-3 py-2 text-left"
      >
        {open ? <ChevronDown className="size-3.5 text-subtle" /> : <ChevronRight className="size-3.5 text-subtle" />}
        <span
          className={`w-3 font-mono text-[11px] ${kind === "A" ? "text-ok" : kind === "D" ? "text-danger" : "text-muted"}`}
          title={file.change}
        >
          {kind}
        </span>
        <span className="min-w-0 flex-1 truncate font-mono text-[12px]">
          {file.old_path && file.old_path !== file.path && <span className="text-subtle">{file.old_path} → </span>}
          {file.path}
        </span>
        {threads.length > 0 && (
          <span className={`inline-flex items-center gap-1 font-mono text-[11px] ${unresolved ? "text-accent" : "text-subtle"}`} title="threads">
            <MessageSquare className="size-3" /> {unresolved || threads.length}
          </span>
        )}
        {file.old_mode && file.mode && file.old_mode !== file.mode && (
          <span className="font-mono text-[11px] text-subtle">
            {file.old_mode} → {file.mode}
          </span>
        )}
        <span className="font-mono text-[11px] tabular-nums">
          <span className="text-ok">+{file.additions}</span> <span className="text-danger">−{file.deletions}</span>
        </span>
      </button>
      {open &&
        (file.binary ? (
          <p className="px-4 py-3 text-[12px] text-subtle">Binary file</p>
        ) : !file.patch ? (
          <p className="px-4 py-3 text-[12px] text-subtle">Diff too large to show</p>
        ) : (
          <div className="overflow-x-auto">{mode === "split" ? <Split ctx={ctx} threads={threads} /> : <Unified ctx={ctx} placement={placement} />}</div>
        ))}
      {open && (offDiff.length > 0 || outdated.length > 0) && (
        <div className="space-y-2 border-t border-line bg-bg px-3 py-3">
          {offDiff.map((t) => (
            <ThreadCard key={t.id} thread={t} actions={review?.actions} focused={focused === t.id} location />
          ))}
          {outdated.length > 0 && (
            <div>
              <button
                type="button"
                onClick={() => setShowOutdated((s) => !s)}
                className="inline-flex items-center gap-1.5 text-[12px] text-subtle transition hover:text-fg"
                aria-expanded={showOutdated}
              >
                {showOutdated ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
                <span className="rounded-full border border-line px-1.5 py-px text-[11px]">outdated</span>
                {outdated.length} thread{outdated.length === 1 ? "" : "s"} on lines that changed since
              </button>
              {showOutdated && (
                <div className="mt-2 space-y-2">
                  {outdated.map((t) => (
                    <ThreadCard key={t.id} thread={t} actions={review?.actions} focused={focused === t.id} location />
                  ))}
                </div>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

export function DiffList({
  files,
  mode = "unified",
  review,
  focused = null,
}: {
  files: FileDiff[];
  mode?: DiffMode;
  review?: DiffReview;
  focused?: string | null;
}) {
  if (!files.length) return <p className="text-[13px] text-subtle">No file changes.</p>;
  return (
    <div className="space-y-3">
      {files.map((f) => (
        <FileDiffView key={f.path} file={f} mode={mode} review={review} focused={focused} />
      ))}
    </div>
  );
}

const MODE_KEY = "roll.diff-mode";

/**
 * Diff with a unified/split toggle and a file summary; the choice is remembered. With `review`,
 * threads sit at their lines and `n`/`p` walk them.
 */
export function DiffViewer({ files, review }: { files: FileDiff[]; review?: DiffReview }) {
  const [mode, setMode] = useState<DiffMode>("unified");
  const [innerFocus, setInnerFocus] = useState<string | null>(null);
  const focused = review?.focused !== undefined ? review.focused : innerFocus;
  const setFocus = review?.onFocus ?? setInnerFocus;
  useEffect(() => {
    const saved = localStorage.getItem(MODE_KEY);
    if (saved === "split" || saved === "unified") setMode(saved);
  }, []);
  const choose = (m: DiffMode) => {
    setMode(m);
    localStorage.setItem(MODE_KEY, m);
  };

  const order = useMemo(() => (review ? threadOrder(files, review.threads) : []), [files, review]);
  useEffect(() => {
    if (!order.length) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      const el = e.target as HTMLElement | null;
      if (el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName))) return;
      if (e.key === "n" || e.key === "p") {
        e.preventDefault();
        setFocus(stepThread(order, focused, e.key === "n" ? 1 : -1));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [order, focused, setFocus]);

  // Threads on files this diff doesn't show (renamed away, or gone).
  const elsewhere = useMemo(() => {
    if (!review) return [];
    const shown = new Set(files.flatMap((f) => threadsForFile(f, review.threads).map((t) => t.id)));
    return review.threads.filter((t) => !shown.has(t.id));
  }, [files, review]);

  const adds = files.reduce((s, f) => s + f.additions, 0);
  const dels = files.reduce((s, f) => s + f.deletions, 0);
  const open = review ? review.threads.filter((t) => !t.resolved).length : 0;
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="font-mono text-[12px] text-subtle">
          {files.length} file{files.length === 1 ? "" : "s"} changed · <span className="text-ok">+{adds}</span>{" "}
          <span className="text-danger">−{dels}</span>
          {review && review.threads.length > 0 && (
            <span>
              {" "}
              · {open} open thread{open === 1 ? "" : "s"}
              {order.length > 0 && <span className="text-line-2"> · n / p to step</span>}
            </span>
          )}
        </p>
        <div className="flex rounded-lg border border-line bg-panel p-0.5" role="radiogroup" aria-label="Diff layout">
          {(["unified", "split"] as const).map((m) => (
            <button
              key={m}
              type="button"
              role="radio"
              aria-checked={m === mode}
              onClick={() => choose(m)}
              className={`rounded-md px-2.5 py-1 text-[12px] transition ${m === mode ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"}`}
            >
              {m}
            </button>
          ))}
        </div>
      </div>
      <DiffList files={files} mode={mode} review={review} focused={focused} />
      {elsewhere.length > 0 && (
        <div className="space-y-2">
          <p className="text-[12px] text-subtle">Threads on files this revision no longer changes</p>
          {elsewhere.map((t) => (
            <ThreadCard key={t.id} thread={t} actions={review?.actions} focused={focused === t.id} location />
          ))}
        </div>
      )}
    </div>
  );
}
