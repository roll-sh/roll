"use client";

/**
 * Messages waiting for the active run (T3 Code's `QueuedRunsControl.tsx`, MIT), attached on top
 * of the composer: each one can go now ("Send now": into the running turn, T3's promote to
 * steer; on a stopped thread, as the next run), be edited in the composer, or be removed; drag a
 * row (or ⌥↑/⌥↓ on it) to reorder. Three show, the rest behind "Show all N queued". A stopped
 * thread holds its queue (T3's held runs): the panel says so, and Resume sends the first.
 */

import { ArrowUp, GripVertical, ListPlus, Pencil, X } from "lucide-react";
import { useState } from "react";
import type { QueuedMessage } from "@/lib/agents-api";
import { deliveredLabel } from "@/lib/agents-worklog";

const SHOWN = 3;

export interface QueueActions {
  sendNow: (q: QueuedMessage) => void;
  edit: (q: QueuedMessage) => void;
  remove: (q: QueuedMessage) => void;
  /** Put `q` in front of `before`, or last. */
  move: (q: QueuedMessage, before: QueuedMessage | null) => void;
}

/** Where a move by one step puts a message: the `before` for `message.reorder`, or `undefined` when it can't move. */
export function stepTarget(queued: readonly QueuedMessage[], id: string, by: -1 | 1): QueuedMessage | null | undefined {
  const i = queued.findIndex((q) => q.id === id);
  const to = i + by;
  if (i < 0 || to < 0 || to >= queued.length) return undefined;
  // Up: in front of the one above. Down: in front of the one after the next, or last.
  return by < 0 ? queued[to] : (queued[to + 1] ?? null);
}

export function QueuedMessages({
  queued,
  held,
  running,
  editing,
  actions,
  onResume,
}: {
  queued: readonly QueuedMessage[];
  /** Nothing runs and the queue waits (the thread was stopped, or its run failed). */
  held: boolean;
  running: boolean;
  /** The message being edited in the composer. */
  editing: string | null;
  actions: QueueActions;
  onResume: () => void;
}) {
  const [all, setAll] = useState(false);
  const [dragging, setDragging] = useState<string | null>(null);
  const shown = all ? queued : queued.slice(0, SHOWN);
  const drop = (before: QueuedMessage | null) => {
    const q = queued.find((m) => m.id === dragging);
    setDragging(null);
    if (q && q.id !== before?.id) actions.move(q, before);
  };
  return (
    <section aria-label="Queued messages" className="px-2">
      {held && (
        <p className="flex h-7 items-center gap-2 px-1.5 text-[12px] text-muted">
          <span className="min-w-0 flex-1 truncate">Queue paused: the last run stopped</span>
          <button type="button" onClick={onResume} className="rounded-md px-1.5 py-0.5 font-medium text-accent transition-colors hover:bg-accent/10">
            Resume
          </button>
        </p>
      )}
      <ul
        className="space-y-0.5"
        onDragOver={(e) => dragging && e.preventDefault()}
        onDrop={(e) => {
          e.preventDefault();
          drop(null);
        }}
      >
        {shown.map((q) => {
          // A wake (delegated tasks' results) is said as what it is, and isn't the user's to edit.
          const wake = !!q.delivers?.length;
          const text = wake ? deliveredLabel({ prompt: q.text, delivers: q.delivers }) : q.text;
          return (
            <li
              key={q.id}
              draggable
              tabIndex={0}
              aria-label={`Queued: ${text}`}
              onDragStart={(e) => {
                e.dataTransfer.effectAllowed = "move";
                setDragging(q.id);
              }}
              onDragEnd={() => setDragging(null)}
              onDragOver={(e) => dragging && e.preventDefault()}
              onDrop={(e) => {
                e.preventDefault();
                e.stopPropagation();
                drop(q);
              }}
              onKeyDown={(e) => {
                if (e.altKey && (e.key === "ArrowUp" || e.key === "ArrowDown")) {
                  e.preventDefault();
                  const before = stepTarget(queued, q.id, e.key === "ArrowUp" ? -1 : 1);
                  if (before !== undefined) actions.move(q, before);
                } else if (e.key === "Delete" || e.key === "Backspace") {
                  e.preventDefault();
                  actions.remove(q);
                } else if (e.key === "Enter" && !wake) {
                  e.preventDefault();
                  actions.edit(q);
                }
              }}
              className={`group/q flex h-7 items-center gap-1.5 rounded-md border border-dashed px-1 text-[12.5px] outline-none transition-[opacity,background-color] duration-150 starting:opacity-0 hover:bg-fg/[0.04] focus-visible:ring-1 focus-visible:ring-accent/50 ${
                dragging === q.id ? "opacity-40" : ""
              } ${editing === q.id ? "border-accent/50" : "border-line-2"}`}
            >
              <GripVertical className="size-3 shrink-0 cursor-grab text-faint opacity-0 transition-opacity group-hover/q:opacity-100 [@media(hover:none)]:hidden" aria-hidden />
              <ListPlus className="size-3.5 shrink-0 text-subtle" aria-hidden />
              {editing === q.id ? (
                <span className="min-w-0 flex-1 truncate text-accent">Editing in the composer</span>
              ) : (
                <span className="min-w-0 flex-1 truncate text-soft">
                  {text || `${q.attachments.length} image${q.attachments.length === 1 ? "" : "s"}`}
                  {q.text && q.attachments.length > 0 && <span className="text-subtle"> · {q.attachments.length} image{q.attachments.length === 1 ? "" : "s"}</span>}
                </span>
              )}
              <span className="flex shrink-0 items-center opacity-0 transition-opacity duration-150 group-hover/q:opacity-100 group-focus-within/q:opacity-100 [@media(hover:none)]:opacity-100">
                <RowButton label={running ? "Send now: into the current turn" : "Send now"} onClick={() => actions.sendNow(q)}>
                  <ArrowUp className="size-3.5" />
                </RowButton>
                {!wake && (
                  <RowButton label="Edit in the composer" onClick={() => actions.edit(q)}>
                    <Pencil className="size-3" />
                  </RowButton>
                )}
                <RowButton label="Remove from queue" onClick={() => actions.remove(q)}>
                  <X className="size-3.5" />
                </RowButton>
              </span>
            </li>
          );
        })}
      </ul>
      {queued.length > SHOWN && (
        <button type="button" onClick={() => setAll((a) => !a)} className="mt-0.5 h-6 rounded-md px-1.5 text-[12px] text-muted transition-colors hover:text-fg">
          {all ? "Show fewer" : `Show all ${queued.length} queued`}
        </button>
      )}
    </section>
  );
}

function RowButton({ label, onClick, children }: { label: string; onClick: () => void; children: React.ReactNode }) {
  return (
    <button type="button" onClick={onClick} title={label} aria-label={label} className="grid size-5 place-items-center rounded text-subtle transition-colors hover:bg-fg/[0.06] hover:text-fg">
      {children}
    </button>
  );
}
