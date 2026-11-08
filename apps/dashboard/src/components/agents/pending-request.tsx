/**
 * A request answered without opening its conversation (on its card on the canvas, at the bottom of
 * a phone's canvas): which thread asks, the last steps that led to it in T3's work-log rows (the
 * earlier ones a click away), then the request itself, answerable right there (`RequestCard`). A
 * plan waiting for Implement shows the plan above its buttons, folded, so it's never approved blind.
 */

import { ArrowUpRight, ChevronRight, ListChecks } from "lucide-react";
import { useState, type ReactNode } from "react";
import type { Request, RequestResponse } from "@/lib/agents-api";
import { planBody, planTitle } from "@/lib/agents-plan";
import { nodeTree, type AgentsData } from "@/lib/agents-store";
import { stepsBefore } from "@/lib/agents-worklog";
import { Markdown } from "../code/markdown";
import { Collapse } from "./animated-height";
import { RequestCard } from "./request-card";
import { StepRow } from "./work-log";

/** How many of the steps before a request show at first (on a canvas card, `compact`: one, none over a plan). */
export const RECENT_STEPS = 3;
const CARD_STEPS = 1;

export function PendingRequest({
  request,
  data,
  onRespond,
  onOpen,
  extra,
  compact = false,
}: {
  request: Request;
  data: AgentsData;
  /** On a canvas card: one step, and a long plan folded shorter (the card stays a card's size). */
  compact?: boolean;
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
  const plan = planOf(request, data);
  // A canvas card stays about two cells tall: over a plan (the context it needs), only the toggle.
  const recent = compact ? (plan ? 0 : CARD_STEPS) : RECENT_STEPS;
  const shown = all ? steps : recent ? steps.slice(-recent) : [];
  const earlier = steps.length - shown.length;
  return (
    <div className="min-w-0 space-y-2">
      <button onClick={onOpen} title="Open the conversation" className="group/open flex max-w-full min-w-0 items-center gap-1.5 text-[11.5px] text-subtle hover:text-fg">
        <span data-mark className="size-1.5 shrink-0 rounded-full bg-accent" aria-hidden />
        <span data-line className="truncate">
          {thread?.title ?? "Waiting for you"}
        </span>
        {extra}
        <ArrowUpRight className="size-3 shrink-0 opacity-0 transition-opacity group-hover/open:opacity-100" />
      </button>
      {steps.length > 0 && (
        <div aria-label="Steps that led here" className="-mx-0.5">
          {steps.length > recent && (
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
      {plan && <PlanPreview text={plan} lines={compact ? CARD_PLAN_LINES : PREVIEW_LINES} />}
      <RequestCard request={request} compact onRespond={onRespond} />
    </div>
  );
}

/**
 * The plan a plan approval asks about: the newest plan the run wrote before the request, else the
 * approval's own (`input.plan`, Claude's ExitPlanMode).
 */
export function planOf(request: Request, data: AgentsData): string | null {
  if (request.body.type !== "approval" || request.body.action !== "plan") return null;
  const nodes = [...(data.nodes.get(request.thread)?.values() ?? [])].map((r) => r.value).filter((n) => n.run === request.run);
  const at = nodes.find((n) => n.id === request.node)?.ordinal ?? Infinity;
  const written = nodes.filter((n) => n.body.type === "plan" && n.ordinal < at).sort((a, b) => b.ordinal - a.ordinal)[0];
  if (written?.body.type === "plan" && written.body.text.trim()) return written.body.text;
  const input = request.body.input as { plan?: unknown } | null;
  return typeof input?.plan === "string" && input.plan.trim() ? input.plan : null;
}

/** How much of a plan shows before "Show the whole plan" (on a canvas card, less: its buttons stay in reach). */
const PREVIEW_LINES = 6;
const CARD_PLAN_LINES = 3;

function PlanPreview({ text, lines: keep }: { text: string; lines: number }) {
  const [open, setOpen] = useState(false);
  const title = planTitle(text);
  const body = title ? planBody(text) : text;
  const lines = body.split("\n");
  const long = lines.length > keep || body.length > keep * 80;
  const shown = long && !open ? `${lines.slice(0, keep).join("\n").trimEnd()}\n\n…` : body;
  return (
    <section data-box aria-label={title ? `Plan: ${title}` : "The plan"} className="rounded-lg border border-line bg-panel-2/60 px-3 py-2">
      <p className="mb-1 flex min-w-0 items-center gap-1.5 text-[12px] font-medium">
        <ListChecks className="size-3.5 shrink-0 text-muted" strokeWidth={1.75} />
        <span data-line className="truncate">
          {title ?? "The plan"}
        </span>
      </p>
      <div data-scroll data-nodrag className={open ? "max-h-80 overflow-auto" : long ? `${keep < PREVIEW_LINES ? "max-h-16 overflow-hidden" : ""} [mask-image:linear-gradient(to_bottom,black_65%,transparent)]` : ""}>
        <Markdown source={shown} className="markdown-sm" />
      </div>
      {long && (
        <button type="button" data-nodrag onClick={() => setOpen((o) => !o)} aria-expanded={open} className="mt-1 text-[12px] text-muted hover:text-fg">
          {open ? "Show less" : "Show the whole plan"}
        </button>
      )}
    </section>
  );
}
