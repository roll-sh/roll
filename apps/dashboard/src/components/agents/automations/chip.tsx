"use client";

/**
 * The small tag on a thread's canvas card when an automation started it: the automation's name,
 * opening its run. Reads the automations store (`ThreadOrigin`); nothing when there's none.
 */

import { Zap } from "lucide-react";
import { useAutomationsState, type ThreadOrigin } from "@/lib/automations-store";
import { setAgentsView } from "@/lib/agents-surface";
import { useAutomationsStore } from "./shared";

/** The automation run that started `thread`, if one did. */
export function useThreadOrigin(thread: string): ThreadOrigin | undefined {
  return useAutomationsState(useAutomationsStore()).threads.get(thread);
}

export function AutomationChip({ origin }: { origin: ThreadOrigin }) {
  return (
    <button
      type="button"
      data-nodrag
      onClick={(e) => {
        e.stopPropagation();
        setAgentsView({ kind: "automations", open: origin.automation, tab: "runs", run: origin.run });
      }}
      onKeyDown={(e) => e.stopPropagation()}
      title={`Started by the automation ${origin.name}: open its run`}
      aria-label={`Started by ${origin.name}`}
      className="inline-flex h-[18px] max-w-40 min-w-0 shrink items-center gap-1 rounded-[5px] border border-line-2 bg-fg/[0.03] px-1.5 text-[10.5px] text-muted transition-colors hover:border-line-3 hover:text-fg"
    >
      <Zap className="size-2.5 shrink-0" strokeWidth={2} />
      <span className="truncate">{origin.name}</span>
    </button>
  );
}
