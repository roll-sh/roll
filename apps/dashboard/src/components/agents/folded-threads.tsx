/**
 * The canvas at scale (docs/code/agents.md, "Canvas"; `foldQuiet` and `groupByStatus` in
 * lib/agents-graph.ts). In the overview a busy project shows what's working, waiting on you,
 * failed or open, the most recently active fill up to six cards, and the rest are one card: "22
 * more threads", stacked. Hovering its summary peeks at the list (a row opens that conversation,
 * whose card comes back while it's open); a click goes into the project's space, where its threads
 * sit under status headers (`StatusHeader`) and only a long Done or Quiet group folds, its card
 * expanding the group in place. Like the machine's "3 more projects" (`machine-projects.tsx`), T3's
 * sidebar (a project's newest threads, the rest a click away) and Linear's board columns.
 */

import { ChevronDown, ChevronUp, CornerDownRight } from "lucide-react";
import { useMemo, useState } from "react";
import type { Thread } from "@/lib/agents-api";
import { newestRuns, statusGroupLabel, type StatusGroup } from "@/lib/agents-graph";
import { useCanvasFolds } from "./canvas-folds";
import { projectColor } from "@/lib/agents-identity";
import { Card, THREAD_H } from "./nodes";
import { ago } from "./shared";
import { fitWords } from "./fit-words";
import { threadDone } from "./thread-status";
import { Popover, PopoverPopup, PopoverTrigger } from "./ui";
import { useAgents } from "./use-agents";

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

/** The fold card of a container: a project's (into its space) or a status group's (in place). */
export function FoldedThreads({ container }: { container: string }) {
  const { client } = useAgents();
  const { folded, toggle, drill } = useCanvasFolds();
  const data = client.data;
  const fold = folded.get(container);
  const group = container.startsWith("g:") ? (container.split(":")[2] as StatusGroup) : null;
  const projectId = container.startsWith("p:") ? container.slice(2) : container.split(":")[1];
  const project = data.projects.get(projectId)?.value;
  const threads = useMemo(() => (fold?.threads ?? []).map((t) => data.threads.get(t)?.value).filter((t): t is Thread => !!t), [fold, data]);
  if (!project || !fold) return null;
  const { expanded } = fold;
  const newest = newestRuns(data);
  const done = threads.filter((t) => threadDone(t, newest.get(t.id))).length;
  const what = group === "done" ? "done" : group === "quiet" ? "quiet" : "";
  const label = expanded ? "Show fewer" : plural(threads.length, `more ${what ? `${what} ` : ""}thread`, `more ${what ? `${what} ` : ""}threads`);
  const open = group || !drill ? () => toggle(container) : () => drill(project.id);
  return (
    <div className="relative">
      {/* The rest of the pile: two sheets of the same card under it, so it reads as many. */}
      {!expanded && (
        <>
          <span aria-hidden className="absolute inset-x-5 -bottom-3 h-6 rounded-b-xl border border-t-0 border-line-2 bg-panel" />
          <span aria-hidden className="absolute inset-x-2.5 -bottom-1.5 h-6 rounded-b-xl border border-t-0 border-line-2 bg-panel" />
        </>
      )}
      <Card
        width={296}
        height={THREAD_H}
        tint={projectColor(project)}
        edge
        onClick={open}
        label={expanded ? `Fold these ${threads.length} threads again` : group || !drill ? `${label}: show them here` : `${label}: open ${project.name}'s space`}
      >
        <div className="flex h-full flex-col px-3.5 pt-2.5 pb-2.5">
          {/* Under its project already: when they last did something, not whose they are (top
              right, where a thread card has its time). */}
          <div className="flex h-4 min-w-0 items-center justify-end gap-1.5 text-[11px] text-subtle">
            {threads[0] && <span data-line className="shrink-0 tabular-nums">{expanded ? plural(threads.length, "thread", "threads") : `last active ${ago(activity(threads[0], newest.get(threads[0].id)))}`}</span>}
          </div>
          <div className="mt-1.5 flex items-center gap-2">
            <p data-title className="min-w-0 flex-1 text-[13.5px] leading-snug font-medium tracking-[-0.01em]">{label}</p>
            {expanded ? <ChevronUp className="size-4 shrink-0 text-subtle" /> : group || !drill ? <ChevronDown className="size-4 shrink-0 text-subtle" /> : <CornerDownRight className="size-4 shrink-0 text-subtle" />}
          </div>
          {!expanded && <Peek threads={threads} done={group ? 0 : done} name={project.name} />}
          {/* The newest of them, so the card says what's in the pile. */}
          {!expanded && (
            <ul className="mt-auto space-y-0.5 text-[11.5px] text-subtle">
              {threads.slice(0, 2).map((t) => (
                <li key={t.id} data-line className="truncate">
                  {fitWords(t.title, 266, 11.5)}
                </li>
              ))}
            </ul>
          )}
        </div>
      </Card>
    </div>
  );
}

/**
 * A status group's header in a project's space: a section's header row (its dot, its name, how
 * many) over a hairline as wide as its cards (`--section-w`, set by the canvas). Not a card: no
 * frame, no fill, no edge to each card under it.
 */
export function StatusHeader({ group, count }: { group: StatusGroup; count: number }) {
  const dot = group === "needs" || group === "running" ? "bg-accent" : group === "failed" ? "bg-danger" : group === "done" ? "bg-ok" : "bg-subtle/60";
  return (
    <div data-drag style={{ width: "var(--section-w, 296px)" }} className="relative flex h-10 items-center gap-2 border-b border-fg/15 pr-1 pl-3.5">
      <span aria-hidden data-mark className={`size-2 shrink-0 rounded-full ${dot} ${group === "running" ? "motion-safe:animate-pulse" : ""}`} />
      {/* A card title's size: it names everything under it, and greeked it stays a bar. */}
      <span data-title className="text-[14px] font-semibold tracking-[-0.015em]">{statusGroupLabel[group]}</span>
      <span data-line className="font-mono text-[11.5px] text-subtle tabular-nums">
        {count}
      </span>
    </div>
  );
}

function activity(t: Thread, run: { requested_at: number; completed_at: number | null } | undefined) {
  return Math.max(t.created_at, run?.requested_at ?? 0, run?.completed_at ?? 0);
}

/** The card's summary line; hovering or clicking it lists the folded threads. */
function Peek({ threads, done, name }: { threads: readonly Thread[]; done: number; name: string }) {
  const { openThread, client } = useAgents();
  const [open, setOpen] = useState(false);
  const newest = newestRuns(client.data);
  const quiet = threads.length - done;
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger
        openOnHover
        delay={250}
        closeDelay={180}
        render={
          <button
            type="button"
            data-nodrag
            onClick={(e) => e.stopPropagation()}
            aria-label={`List ${name}'s ${threads.length} folded threads`}
            className="-mx-1 mt-1 flex w-[calc(100%+0.5rem)] items-center gap-1.5 rounded-md px-1 py-0.5 text-left text-[12px] text-muted transition-colors duration-150 hover:bg-fg/[0.04] hover:text-fg data-popup-open:bg-fg/[0.06] data-popup-open:text-fg"
          />
        }
      >
        <span data-line className="min-w-0 truncate">
          {done > 0 && (
            <>
              <span className="mr-1 inline-block size-1.5 rounded-full bg-ok align-middle" aria-hidden />
              {done} done{quiet > 0 && " · "}
            </>
          )}
          {quiet > 0 && (done > 0 ? `${quiet} quiet` : "Newest first")}
        </span>
      </PopoverTrigger>
      <PopoverPopup side="right" align="start" sideOffset={10} aria-label={`${name}: folded threads`} className="w-[320px] flex-col p-1 text-[12.5px]">
        <p className="px-2 pt-1 pb-1.5 text-[11px] font-medium text-subtle">Not on the canvas, newest first</p>
        <ul className="max-h-[min(60vh,420px)] overflow-y-auto">
          {threads.map((t) => {
            const run = newest.get(t.id);
            const finished = threadDone(t, run);
            return (
              <li key={t.id}>
                <button
                  type="button"
                  onClick={() => (setOpen(false), openThread(t.id))}
                  className="flex w-full min-w-0 items-center gap-2 rounded-md px-2 py-1.5 text-left transition-colors duration-100 hover:bg-fg/[0.06] focus-visible:bg-fg/[0.06] focus-visible:outline-none"
                >
                  <span aria-hidden className={`size-1.5 shrink-0 rounded-full ${finished ? "bg-ok" : "bg-subtle/60"}`} />
                  <span className="min-w-0 flex-1 truncate text-fg">{t.title}</span>
                  <span className="shrink-0 font-mono text-[10.5px] text-subtle tabular-nums">{ago(activity(t, run))}</span>
                </button>
              </li>
            );
          })}
        </ul>
      </PopoverPopup>
    </Popover>
  );
}
