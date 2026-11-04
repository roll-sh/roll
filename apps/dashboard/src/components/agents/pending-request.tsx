"use client";

/**
 * A request answered without opening its conversation (on its card on the canvas, at the bottom of
 * a phone's canvas): which thread asks, the last steps that led to it in T3's work-log rows (the
 * earlier ones a click away), then the request itself, answerable right there (`RequestCard`).
 */

import { ArrowUpRight, ChevronRight } from "lucide-react";
import { useState, type ReactNode } from "react";
import type { Request, RequestResponse } from "@/lib/agents-api";
import { nodeTree, type AgentsData } from "@/lib/agents-store";
import { stepsBefore } from "@/lib/agents-worklog";
import { Collapse } from "./animated-height";
import { RequestCard } from "./request-card";
import { StepRow } from "./work-log";

/** How many of the steps before a request show at first. */
export const RECENT_STEPS = 3;

export function PendingRequest({
  request,
  data,
  onRespond,
  onOpen,
  extra,
}: {
  request: Request;
  data: AgentsData;
  /** Unset: read-only. */
  onRespond?: (r: RequestResponse) => Promise<unknown>;
  /** Open the conversation. */
  onOpen: () => void;
  /** After the thread's name (how many more wait, on a phone). */
  extra?: ReactNode;
}) {
  const [all, setAll] = useState(false);
  const thread = data.threads.get(request.thread)?.value;
  const { steps } = stepsBefore(nodeTree(data, request.thread, request.run), request.node, Infinity);
  const shown = all ? steps : steps.slice(-RECENT_STEPS);
  const earlier = steps.length - shown.length;
  return (
    <div className="min-w-0 space-y-2">
      <button onClick={onOpen} title="Open the conversation" className="group/open flex max-w-full min-w-0 items-center gap-1.5 text-[11.5px] text-subtle hover:text-fg">
        <span className="size-1.5 shrink-0 rounded-full bg-accent" aria-hidden />
        <span className="truncate">{thread?.title ?? "Waiting for you"}</span>
        {extra}
        <ArrowUpRight className="size-3 shrink-0 opacity-0 transition-opacity group-hover/open:opacity-100" />
      </button>
      {steps.length > 0 && (
        <div aria-label="Steps that led here" className="-mx-0.5">
          {steps.length > RECENT_STEPS && (
            <button
              onClick={() => setAll((a) => !a)}
              aria-expanded={all}
              className="flex min-h-6 items-center gap-1.5 rounded-md px-0.5 text-[12px] text-subtle transition-colors hover:text-fg"
            >
              <span className="grid w-6 place-items-center">
                <ChevronRight className={`size-3 transition-transform duration-200 motion-reduce:transition-none ${all ? "rotate-90" : ""}`} />
              </span>
              {all ? "Show fewer steps" : `${earlier} earlier step${earlier === 1 ? "" : "s"}`}
            </button>
          )}
          <Collapse open>
            {shown.map((n) => (
              <StepRow key={n.id} node={n} grouped />
            ))}
          </Collapse>
        </div>
      )}
      <RequestCard request={request} compact onRespond={onRespond} />
    </div>
  );
}
