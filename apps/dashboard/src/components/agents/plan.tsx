"use client";

/**
 * Plan mode, as T3 Code does it (`ProposedPlanCard.tsx`, `ComposerPlanFollowUpBanner.tsx`,
 * `ComposerPrimaryActions.tsx`, `useProposedPlanSend.ts`; MIT): the proposed plan is a card in
 * the transcript (its title, a menu to copy or download it, long ones folded); while a plan is
 * ready the composer says so above itself and its button becomes "Implement" (with "Implement in
 * a new thread" beside it), or "Refine" once something is typed. The logic is
 * `lib/agents-plan.ts`.
 */

import { Check, ChevronDown, Copy, Download, ListChecks, MoreHorizontal } from "lucide-react";
import { useState } from "react";
import type { Machine, Thread } from "@/lib/agents-api";
import { downloadPlan, implementationPrompt, implementationThreadTitle, planBody, planTitle, type ReadyPlan } from "@/lib/agents-plan";
import { Markdown } from "../code/markdown";
import { Menu, MenuItem, MenuPopup, MenuTrigger } from "./ui";
import { useAgents } from "./use-agents";

/** T3 folds a plan over 900 characters or 20 lines down to its first 10 lines. */
const LONG_CHARS = 900;
const LONG_LINES = 20;
const PREVIEW_LINES = 10;

/** The plan card (T3's `ProposedPlanCard`): a "Plan" badge and the plan's title, its menu, the plan. */
export function PlanCard({ text, running }: { text: string; running: boolean }) {
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const title = planTitle(text);
  const body = title ? planBody(text) : text;
  const lines = body.split("\n");
  const long = !running && (body.length > LONG_CHARS || lines.length > LONG_LINES);
  const shown = long && !open ? `${lines.slice(0, PREVIEW_LINES).join("\n").trimEnd()}\n\n…` : body;
  return (
    <div className="rounded-2xl border border-line bg-raised p-4 sm:p-5" aria-label={title ? `Plan: ${title}` : "Proposed plan"}>
      <div className="mb-2 flex items-center gap-2">
        <span className="shrink-0 rounded-full border border-line-2 px-2 py-px text-[11px] font-medium text-muted">Plan</span>
        <h3 className="min-w-0 flex-1 truncate text-[13.5px] font-medium">{title ?? "Proposed plan"}</h3>
        <Menu>
          <MenuTrigger
            render={
              <button type="button" aria-label="Plan actions" title="Plan actions" className="grid size-6 shrink-0 place-items-center rounded-md text-subtle transition-colors hover:bg-fg/[0.06] hover:text-fg">
                <MoreHorizontal className="size-3.5" />
              </button>
            }
          />
          <MenuPopup align="end" className="w-52">
            <MenuItem
              onClick={() => {
                void navigator.clipboard?.writeText(text);
                setCopied(true);
                setTimeout(() => setCopied(false), 1200);
              }}
            >
              {copied ? <Check /> : <Copy />} {copied ? "Copied!" : "Copy to clipboard"}
            </MenuItem>
            <MenuItem onClick={() => downloadPlan(text)}>
              <Download /> Download as markdown
            </MenuItem>
          </MenuPopup>
        </Menu>
      </div>
      <div className={long && !open ? "[mask-image:linear-gradient(to_bottom,black_70%,transparent)]" : ""}>
        <Markdown source={shown} className="markdown-sm" />
        {running && <span className="caret inline-block" aria-hidden />}
      </div>
      {long && (
        <button type="button" onClick={() => setOpen((o) => !o)} className="mt-2 text-[12.5px] text-muted hover:text-fg">
          {open ? "Collapse plan" : "Expand plan"}
        </button>
      )}
    </div>
  );
}

/** Above the composer while a plan is ready (T3's `ComposerPlanFollowUpBanner`). */
export function PlanBanner({ ready }: { ready: ReadyPlan }) {
  return (
    <p className="flex h-7 min-w-0 items-center gap-2 px-3.5 text-[12.5px]" role="status">
      <ListChecks className="size-3.5 shrink-0 text-accent" aria-hidden />
      <span className="shrink-0 font-medium">Plan ready</span>
      {ready.title && <span className="min-w-0 truncate text-muted">· {ready.title}</span>}
    </p>
  );
}

/**
 * What the plan's buttons do. A plan Claude asked to carry out (its ExitPlanMode approval) is
 * answered: Implement accepts it (Claude goes on in build mode), Refine declines it with what was
 * typed. A plan a run ended with (Codex, ACP) goes back as a message: Implement switches the
 * thread to build mode and sends T3's "PLEASE IMPLEMENT THIS PLAN:" prompt, Refine sends what was
 * typed in plan mode. "Implement in a new thread" starts a thread in build mode with that prompt
 * (the same project, provider, model, permissions; a worktree of its own if this one has one),
 * after letting go of an approval, and opens it.
 */
export function usePlanActions(thread: Thread | null, machine: Machine | undefined, ready: ReadyPlan | null) {
  const { client, openThread } = useAgents();
  const mid = machine?.id;
  const run = (command: Parameters<typeof client.run>[1]) => client.run(mid!, command);
  return {
    implement: async () => {
      if (!ready || !mid || !thread) return;
      if (ready.approval) return run({ type: "request.respond", request: ready.approval.id, response: { type: "decision", decision: "accept" } });
      await run({ type: "thread.configure", thread: thread.id, interaction_mode: "default" });
      await run({ type: "message.send", thread: thread.id, text: implementationPrompt(ready.text) });
    },
    refine: async (text: string) => {
      if (!ready || !mid || !thread) return;
      if (ready.approval) return run({ type: "request.respond", request: ready.approval.id, response: { type: "decision", decision: "decline", message: text } });
      await run({ type: "message.send", thread: thread.id, text });
    },
    implementInNewThread: async () => {
      if (!ready || !mid || !thread) return;
      const a = ready.approval;
      if (a && a.body.type === "approval") {
        const decision = a.body.decisions.includes("cancel") ? "cancel" : "decline";
        await run({ type: "request.respond", request: a.id, response: { type: "decision", decision, message: "The user is implementing this plan in a new thread." } });
      }
      const created = await client.run<{ type: "created"; id: string }>(
        mid,
        {
          type: "thread.create",
          project: thread.project,
          provider: thread.provider,
          ...(thread.model ? { model: thread.model } : {}),
          ...(thread.effort ? { effort: thread.effort } : {}),
          runtime_mode: thread.runtime_mode,
          interaction_mode: "default",
          worktree: !!thread.worktree,
          title: implementationThreadTitle(ready.text),
          prompt: implementationPrompt(ready.text),
        },
        "created",
      );
      openThread(created.id);
    },
  };
}

/** The composer's button while a plan is ready (T3's split pill): Implement, and a menu with "Implement in a new thread". */
export function ImplementButton({ busy, disabled, onImplement, onNewThread }: { busy: boolean; disabled: boolean; onImplement: () => void; onNewThread: () => void }) {
  return (
    <span className="inline-flex h-8 shrink-0 items-stretch overflow-hidden rounded-full bg-accent text-white shadow-xs">
      <button type="button" onClick={onImplement} disabled={disabled} className="px-3.5 text-[12.5px] font-medium transition hover:brightness-110 active:scale-[0.97] disabled:opacity-50 motion-reduce:transition-none">
        {busy ? <span className="shimmer">Sending…</span> : "Implement"}
      </button>
      <Menu>
        <MenuTrigger
          render={
            <button type="button" disabled={disabled} aria-label="More ways to implement" title="More ways to implement" className="border-l border-white/25 px-2 transition hover:brightness-110 disabled:opacity-50">
              <ChevronDown className="size-3.5" />
            </button>
          }
        />
        <MenuPopup align="end" side="top" className="w-56">
          <MenuItem onClick={onNewThread}>Implement in a new thread</MenuItem>
        </MenuPopup>
      </Menu>
    </span>
  );
}
