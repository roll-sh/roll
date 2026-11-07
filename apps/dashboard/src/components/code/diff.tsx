"use client";

import { ChevronDown, ChevronRight, MessageSquare } from "lucide-react";
import { Fragment, memo, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
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
import { BLOCK, codeOf, useHighlight, useRowWindow } from "./diff-window";
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

/** The blocks that stay rendered wherever they scroll: threads, drafts and the composer live in them. */
function useForced(ctx: FileCtx, inline: Map<number, Thread[]> | undefined, rows: number, hasExtra: (i: number) => boolean) {
  const compose = ctx.review?.compose;
  const path = ctx.file.path;
  const busy = !!compose && (compose.selection?.path === path || compose.pending.comments.some((c) => c.path === path));
  const extra = useRef(hasExtra);
  extra.current = hasExtra;
  return useMemo(() => {
    const s = new Set<number>();
    for (const i of inline?.keys() ?? []) s.add(Math.floor(i / BLOCK));
    if (busy) for (let i = 0; i < rows; i++) if (extra.current(i)) s.add(Math.floor(i / BLOCK));
    return s;
  }, [inline, busy, compose?.selection, compose?.pending, rows, ctx.lines]);
}

/** A line's code, highlighted once its block's tokens are in. */
function Code({ l }: { l: Line }) {
  const t = codeOf(l);
  if (!t) return <>{l.text}</>;
  return <>{t.map((k, i) => (k.c ? <span key={i} className={k.c}>{k.t}</span> : k.t))}</>;
}

interface BlockProps {
  ctx: FileCtx;
  start: number;
  end: number;
  live: boolean;
  seen: boolean;
  height: number;
  tbody: (el: HTMLTableSectionElement | null) => void;
}

/** A block that isn't near the viewport: one row of its height. */
const Spacer = ({ tbody, height }: Pick<BlockProps, "tbody" | "height">) => (
  <tbody ref={tbody} aria-hidden>
    <tr style={{ height }}>
      <td colSpan={4} />
    </tr>
  </tbody>
);

const UnifiedBlock = memo(function UnifiedBlock({ ctx, start, end, live, seen, height, tbody, placement }: BlockProps & { placement: Placement | null }) {
  const slice = useMemo(() => ctx.lines.slice(start, end), [ctx.lines, start, end]);
  useHighlight(ctx.file.path, slice, live && seen);
  if (!live) return <Spacer tbody={tbody} height={height} />;
  const sel = ctx.review?.compose?.selection ?? null;
  return (
    <tbody ref={tbody} data-live="">
      {slice.map((l, k) => {
        const i = start + k;
        const extras = rowExtras(ctx, placement?.inline.get(i) ?? [], l);
        const isSel = rowSpots(l).some((s) => inSelection(sel, ctx.file.path, s.side, s.line) && s.side === sel?.side);
        return (
          <Fragment key={i}>
            <tr className={isSel ? selected : rowCls[l.kind]}>
              <NumCell n={l.kind === "hunk" || l.kind === "note" ? null : l.old} side="old" ctx={ctx} />
              <NumCell n={l.kind === "hunk" || l.kind === "note" ? null : l.new} side="new" ctx={ctx} />
              <td className={`w-4 select-none pl-2 ${signCls(l.kind)}`}>{sign[l.kind]}</td>
              <td className="pr-4 pl-1 whitespace-pre">
                <Code l={l} />
              </td>
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
  );
});

function Unified({ ctx, placement }: { ctx: FileCtx; placement: Placement | null }) {
  const compose = ctx.review?.compose;
  const forced = useForced(
    ctx,
    placement?.inline,
    ctx.lines.length,
    (i) => !!compose && (draftsOnRow(compose.pending, ctx.file.path, ctx.lines[i]).length > 0 || endsSelection(compose.selection, ctx.file.path, ctx.lines[i])),
  );
  const win = useRowWindow(forced);
  const n = ctx.lines.length;
  return (
    <div ref={win.frame} className="overflow-x-auto">
      <table className="w-full border-collapse font-mono text-[12px] leading-5">
        {Array.from({ length: Math.ceil(n / BLOCK) }, (_, b) => {
          const start = b * BLOCK;
          const end = Math.min(start + BLOCK, n);
          return (
            <UnifiedBlock
              key={b}
              ctx={ctx}
              placement={placement}
              start={start}
              end={end}
              live={win.live(b)}
              seen={win.seen(b)}
              height={win.height(b, end - start)}
              tbody={win.block(b)}
            />
          );
        })}
      </table>
    </div>
  );
}

const SplitBlock = memo(function SplitBlock({
  ctx,
  start,
  end,
  live,
  seen,
  height,
  tbody,
  rows,
  placement,
}: BlockProps & { rows: [Side, Side][]; placement: Placement }) {
  const slice = useMemo(() => {
    const here = new Set<Line>();
    for (const [a, b] of rows.slice(start, end)) {
      if (a) here.add(a);
      if (b) here.add(b);
    }
    return [...here];
  }, [rows, start, end]);
  useHighlight(ctx.file.path, slice, live && seen);
  if (!live) return <Spacer tbody={tbody} height={height} />;
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
          <Code l={l} />
        </td>
      </>
    );
  };
  return (
    <tbody ref={tbody} data-live="">
      {rows.slice(start, end).map(([a, b], k) => {
        const i = start + k;
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
  );
});

function Split({ ctx, threads }: { ctx: FileCtx; threads: Thread[] }) {
  const rows = useMemo(() => splitRows(ctx.lines), [ctx.lines]);
  const placement = useMemo(() => placeSplit(rows, threads), [rows, threads]);
  const compose = ctx.review?.compose;
  const forced = useForced(ctx, placement.inline, rows.length, (i) => {
    if (!compose) return false;
    const [a, b] = rows[i];
    const p = ctx.file.path;
    return (
      draftsOnRow(compose.pending, p, a, "old").length > 0 ||
      draftsOnRow(compose.pending, p, b, "new").length > 0 ||
      endsSelection(compose.selection, p, a, "old") ||
      endsSelection(compose.selection, p, b, "new")
    );
  });
  const win = useRowWindow(forced);
  const n = rows.length;
  return (
    <div ref={win.frame} className="overflow-x-auto">
      <table className="w-full table-fixed border-collapse font-mono text-[12px] leading-5">
        <colgroup>
          <col className="w-10" />
          <col />
          <col className="w-10" />
          <col />
        </colgroup>
        {Array.from({ length: Math.ceil(n / BLOCK) }, (_, b) => {
          const start = b * BLOCK;
          const end = Math.min(start + BLOCK, n);
          return (
            <SplitBlock
              key={b}
              ctx={ctx}
              rows={rows}
              placement={placement}
              start={start}
              end={end}
              live={win.live(b)}
              seen={win.seen(b)}
              height={win.height(b, end - start)}
              tbody={win.block(b)}
            />
          );
        })}
      </table>
    </div>
  );
}

/** A patch's row count without parsing it (a collapsed file never parses). */
function patchRows(patch: string) {
  let n = 1;
  for (let i = patch.indexOf("\n"); i !== -1; i = patch.indexOf("\n", i + 1)) n++;
  return n;
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
  const threads = useMemo(() => (review ? threadsForFile(file, review.threads) : []), [file, review]);
  const [open, setOpen] = useState(() => !file.patch || patchRows(file.patch) <= COLLAPSE_LINES || threads.length > 0);
  const lines = useMemo(() => (open && file.patch ? parsePatch(file.patch) : []), [open, file.patch]);
  const placement = useMemo(() => (review ? placeUnified(lines, threads) : null), [lines, threads, review]);
  const [showOutdated, setShowOutdated] = useState(false);
  const focusedHere = focused != null && threads.some((t) => t.id === focused);
  const focusedOutdated = focused != null && (placement?.outdated.some((t) => t.id === focused) ?? false);

  useEffect(() => {
    if (focusedHere) setOpen(true);
    if (focusedOutdated) setShowOutdated(true);
  }, [focusedHere, focusedOutdated]);

  const ctx: FileCtx = useMemo(() => ({ file, lines, review, focused }), [file, lines, review, focused]);
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
          mode === "split" ? <Split ctx={ctx} threads={threads} /> : <Unified ctx={ctx} placement={placement} />
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
