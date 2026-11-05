/**
 * Proposed plans (plan mode), ported from T3 Code's `apps/web/src/proposedPlan.ts` and
 * `packages/shared/src/proposedPlanText.ts` (MIT): the plan's title, the message that carries it
 * out ("Implement"), the new thread's title ("Implement in a new thread"), the file it downloads
 * as, and which plan is ready to act on in a conversation.
 */

import type { Node, Request, Run, Thread } from "./agents-api";
import type { AgentsData } from "./agents-store";
import { nodeTree, runsOf } from "./agents-store";

/** T3's `PLAN_IMPLEMENTATION_PROMPT_PREFIX`: what "Implement" sends in front of the plan. */
export const PLAN_IMPLEMENTATION_PREFIX = "PLEASE IMPLEMENT THIS PLAN:\n";

/** The plan's first heading, if it has one (T3's `proposedPlanTitle`). */
export function planTitle(markdown: string): string | null {
  const heading = markdown.match(/^\s{0,3}#{1,6}\s+(.+)$/m)?.[1]?.trim();
  return heading ? heading : null;
}

/** The plan without its title heading (and a leading "Summary" heading), as the card shows it under its own title. */
export function planBody(markdown: string): string {
  const lines = markdown.trimEnd().split(/\r?\n/);
  let start = lines[0] && /^\s{0,3}#{1,6}\s+/.test(lines[0]) ? 1 : 0;
  while (lines[start]?.trim() === "") start++;
  if (lines[start]?.match(/^\s{0,3}#{1,6}\s+(.+)$/)?.[1]?.trim().toLowerCase() === "summary") {
    start++;
    while (lines[start]?.trim() === "") start++;
  }
  return lines.slice(start).join("\n");
}

/** What "Implement" sends: the prefix, then the plan. */
export function implementationPrompt(markdown: string): string {
  return `${PLAN_IMPLEMENTATION_PREFIX}${markdown.trim()}`;
}

/** T3's `buildPlanImplementationThreadTitle`. */
export function implementationThreadTitle(markdown: string): string {
  const title = planTitle(markdown);
  return title ? `Implement ${title}` : "Implement plan";
}

/** T3's `buildProposedPlanMarkdownFilename`: the title as a file name. */
export function planFilename(markdown: string): string {
  const segment = (planTitle(markdown) ?? "plan")
    .toLowerCase()
    .replace(/[`'".,!?()[\]{}]+/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return `${segment || "plan"}.md`;
}

/** Download the plan as a markdown file (T3's "Download as markdown"). */
export function downloadPlan(markdown: string): void {
  const url = URL.createObjectURL(new Blob([`${markdown.trimEnd()}\n`], { type: "text/markdown;charset=utf-8" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = planFilename(markdown);
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 0);
}

/**
 * The plan a conversation can act on now (T3's composer "Plan ready" banner): the newest run's
 * last plan, in a thread still in plan mode. Either the provider asks to leave plan mode with it
 * (Claude's ExitPlanMode: a pending `plan` approval, answered by Implement or Refine), or the run
 * ended with it (Codex, ACP: Implement sends it as the next message in build mode). A thread that
 * left plan mode, or a newer run, retires it.
 */
export interface ReadyPlan {
  /** The plan node. */
  node: Node;
  text: string;
  title: string | null;
  run: Run;
  /** The pending approval that carries it (Claude), if any. */
  approval: Request | null;
}

export function readyPlan(data: AgentsData, thread: Thread): ReadyPlan | null {
  const run = runsOf(data, thread.id).at(-1);
  if (!run || run.status === "rolled_back") return null;
  const approval =
    [...data.requests.values()].map((r) => r.value).find((r) => r.thread === thread.id && r.status === "pending" && r.body.type === "approval" && r.body.action === "plan") ?? null;
  // Without an approval, the run has to have ended in plan mode; while it goes, the plan is still forming.
  if (!approval && (thread.interaction_mode !== "plan" || run.status !== "completed")) return null;
  const node = nodeTree(data, thread.id, run.id)
    .map((t) => t.node)
    .filter((n) => n.body.type === "plan" && n.body.text.trim())
    .at(-1);
  if (!node || node.body.type !== "plan") return null;
  return { node, text: node.body.text, title: planTitle(node.body.text), run, approval };
}
