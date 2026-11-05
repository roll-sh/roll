"use client";

/**
 * A project's shelf on the canvas (docs/code/agents.md, "Settling"): its settled, snoozed and
 * archived conversations, which have left the canvas, listed on the project's card. A row opens
 * the conversation in the side panel, transcript intact; its button restores it in one click
 * (un-settle, wake or unarchive) with T3's undo notice. T3 has these in its sidebar's Settled
 * shelf (`Sidebar.tsx`, `olderSettledShelf`) and its archive in settings
 * (`ArchivedThreadsPanel.tsx`); roll puts them where the project is.
 */

import { AlarmClock, AlarmClockOff, Archive, ArchiveRestore, Check, ChevronDown, ChevronRight, Eraser, Undo2 } from "lucide-react";
import { useMemo, useState } from "react";
import type { ProjectInbox, Shelf } from "@/lib/agents-inbox";
import { projectInbox, restoreOf, SETTLED_FIRST, SETTLED_PAGE, shortAgo, snoozeLeft, type InboxRow } from "@/lib/agents-inbox";
import { setShelfOpen, useShelfOpen } from "@/lib/agents-shelf";
import { useInboxActions } from "./threads-sidebar";
import { Segmented } from "./ui";
import { useAgents } from "./use-agents";

type Parked = Exclude<Shelf, "active">;
const LABEL: Record<Parked, string> = { settled: "Settled", snoozed: "Snoozed", archived: "Archived" };
const ORDER: Parked[] = ["settled", "snoozed", "archived"];

/** What's on a project's shelves, for the card; `null` when nothing is. */
export function useProjectShelf(project: string): ProjectInbox | null {
  const { client } = useAgents();
  const data = client.data;
  // Shelves only change with the data; a snooze running out is the daemon's to record.
  return useMemo(() => {
    const g = projectInbox(data, project, Date.now());
    return g && g.settled.length + g.snoozed.length + g.archived.length > 0 ? g : null;
  }, [data, project]);
}

export function ProjectShelf({ project, group }: { project: string; group: ProjectInbox }) {
  const open = useShelfOpen(project);
  const counts = ORDER.filter((s) => group[s].length > 0);
  const [tab, setTab] = useState<Parked>(counts[0] ?? "settled");
  const shown = group[tab].length > 0 ? tab : (counts[0] ?? "settled");
  const summary = counts.map((s) => `${LABEL[s]} ${group[s].length}`).join(" · ");
  return (
    <div data-detail className="mx-3 mb-3 border-t border-line pt-2">
      <button
        type="button"
        data-nodrag
        aria-expanded={open}
        onClick={() => setShelfOpen(project, !open)}
        className="-mx-1 flex w-[calc(100%+0.5rem)] items-center gap-1.5 rounded-md px-1 py-0.5 text-left text-[11.5px] text-subtle transition hover:bg-panel-2 hover:text-muted"
        title={open ? "Hide the settled and archived conversations" : "Show the settled and archived conversations"}
      >
        {open ? <ChevronDown className="size-3 shrink-0" /> : <ChevronRight className="size-3 shrink-0" />}
        <span className="min-w-0 truncate">{summary}</span>
      </button>
      {open && (
        <div className="mt-1.5 space-y-1.5" data-nodrag>
          {counts.length > 1 && (
            <Segmented
              label="Shelf"
              value={shown}
              onChange={setTab}
              className="w-full [&>button]:flex-1 [&>button]:text-[11px]"
              options={counts.map((s) => ({ value: s, label: `${LABEL[s]} ${group[s].length}` }))}
            />
          )}
          <ShelfRows key={shown} rows={group[shown]} />
        </div>
      )}
    </div>
  );
}

function ShelfRows({ rows }: { rows: InboxRow[] }) {
  const [count, setCount] = useState(SETTLED_FIRST);
  const hidden = rows.length - count;
  return (
    <ul className="space-y-px" aria-label="Conversations on the shelf">
      {rows.slice(0, count).map((r) => (
        <ShelfRow key={r.thread.id} row={r} />
      ))}
      {hidden > 0 && (
        <li>
          <button type="button" onClick={() => setCount((n) => n + SETTLED_PAGE)} className="w-full rounded-md px-1.5 py-1 text-left text-[11.5px] text-subtle transition hover:bg-panel-2 hover:text-fg">
            Show {Math.min(hidden, SETTLED_PAGE)} more
          </button>
        </li>
      )}
    </ul>
  );
}

const RESTORE_ICON: Record<Parked, typeof Undo2> = { settled: Undo2, snoozed: AlarmClockOff, archived: ArchiveRestore };
const SHELF_ICON: Record<Parked, typeof Check> = { settled: Check, snoozed: AlarmClock, archived: Archive };

function ShelfRow({ row }: { row: InboxRow }) {
  const { openThread } = useAgents();
  const actions = useInboxActions();
  const t = row.thread;
  const shelf = row.shelf as Parked;
  const r = restoreOf(shelf)!;
  const now = Date.now();
  const Icon = SHELF_ICON[shelf];
  const Restore = RESTORE_ICON[shelf];
  const cleaned = !!t.worktree?.removed_at;
  return (
    <li className="group/s flex h-7 items-center rounded-md transition-colors hover:bg-panel-2">
      <button type="button" onClick={() => openThread(t.id)} className="flex h-full min-w-0 flex-1 items-center gap-1.5 pl-1.5 text-left" title={`Open “${t.title}”`}>
        <Icon className="size-3 shrink-0 text-subtle" strokeWidth={1.5} aria-hidden />
        <span className="min-w-0 flex-1 truncate text-[12px] text-muted group-hover/s:text-fg">{t.title}</span>
        {cleaned && <Eraser className="size-3 shrink-0 text-subtle" strokeWidth={1.5} aria-label="Worktree cleaned up" />}
        <span className={`shrink-0 pr-1.5 font-mono text-[10.5px] text-subtle ${actions.readOnly ? "" : "group-hover/s:hidden group-focus-within/s:hidden [@media(hover:none)]:hidden"}`}>
          {shelf === "snoozed" ? snoozeLeft(row.at, now) : shortAgo(row.at, now)}
        </span>
      </button>
      {!actions.readOnly && (
        <button
          type="button"
          title={`${r.label} “${t.title}”`}
          aria-label={`${r.label} ${t.title}`}
          onClick={() => void actions.restore(t)}
          className="mr-0.5 hidden size-6 shrink-0 place-items-center rounded-md text-subtle transition group-hover/s:grid group-focus-within/s:grid hover:bg-panel hover:text-fg [@media(hover:none)]:grid"
        >
          <Restore className="size-3.5" strokeWidth={1.5} />
        </button>
      )}
    </li>
  );
}
