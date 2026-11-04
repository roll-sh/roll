/**
 * Plan mode, as T3 Code does it (`ProposedPlanCard.tsx`, `ComposerPlanFollowUpBanner.tsx`,
 * `ComposerPrimaryActions.tsx`, `useProposedPlanSend.ts`; MIT): the proposed plan is a card in
 * the transcript (its title, a menu to copy it, download it or save it into the workspace, long
 * ones folded); while a plan is
 * ready the composer says so above itself and its button becomes "Implement" (with "Implement in
 * a new thread" beside it), or "Refine" once something is typed. The logic is
 * `lib/agents-plan.ts`.
 */

import { Check, ChevronDown, Copy, Download, FileDown, ListChecks, MoreHorizontal } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { Machine, Thread } from "@/lib/agents-api";
import { downloadPlan, implementationPrompt, implementationThreadTitle, planBody, planFilename, planTitle, planToSave, type ReadyPlan } from "@/lib/agents-plan";
import { Markdown } from "../code/markdown";
import { Collapse } from "./animated-height";
import { useHeightGlide } from "./slide-pages";
import { Menu, MenuItem, MenuPopup, MenuTrigger } from "./ui";
import { useAgents } from "./use-agents";

/** T3 folds a plan over 900 characters or 20 lines down to its first 10 lines. */
const LONG_CHARS = 900;
const LONG_LINES = 20;
const PREVIEW_LINES = 10;

/**
 * The plan card (T3's `ProposedPlanCard`): a "Plan" badge and the plan's title, its menu, the plan.
 * `onSave` writes it into the thread's workspace (T3's "Save to workspace": a path relative to the
 * workspace, the title as the file name by default); unset where nothing may be written (a share).
 */
export function PlanCard({ text, running, onSave }: { text: string; running: boolean; onSave?: (path: string, text: string) => Promise<string> }) {
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const [saving, setSaving] = useState(false);
  const title = planTitle(text);
  const body = title ? planBody(text) : text;
  const lines = body.split("\n");
  const long = !running && (body.length > LONG_CHARS || lines.length > LONG_LINES);
  const shown = long && !open ? `${lines.slice(0, PREVIEW_LINES).join("\n").trimEnd()}\n\n…` : body;
  // Expand plan / Collapse plan: the body's height eases between the preview and the whole plan.
  const bodyRef = useRef<HTMLDivElement>(null);
  useHeightGlide(bodyRef, String(open));
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
            {onSave && !running && (
              <MenuItem onClick={() => setSaving(true)}>
                <FileDown /> Save to workspace
              </MenuItem>
            )}
          </MenuPopup>
        </Menu>
      </div>
      {/* Folds open, and away once it has said where it saved (it vanished after 2.4 s). */}
      <Collapse open={saving && !!onSave}>{onSave && <SavePlan text={text} onSave={onSave} onDone={() => setSaving(false)} />}</Collapse>
      <div ref={bodyRef} className={long ? `overflow-hidden ${open ? "" : "[mask-image:linear-gradient(to_bottom,black_70%,transparent)]"}` : ""}>
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

/**
 * "Save plan to workspace" (T3's dialog, inline in the card): a path relative to the workspace,
 * prefilled with the plan's file name; Enter saves, Esc cancels. Once saved it says where, briefly.
 */
function SavePlan({ text, onSave, onDone }: { text: string; onSave: (path: string, text: string) => Promise<string>; onDone: () => void }) {
  const [path, setPath] = useState(() => planFilename(text));
  const [state, setState] = useState<{ kind: "editing" | "saving" } | { kind: "error"; message: string } | { kind: "saved"; path: string }>({ kind: "editing" });
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    // The name without `.md` selected, so typing replaces it.
    const el = input.current;
    if (!el) return;
    el.focus();
    const dot = el.value.lastIndexOf(".md");
    el.setSelectionRange(0, dot > 0 ? dot : el.value.length);
  }, []);
  useEffect(() => {
    if (state.kind !== "saved") return;
    const t = setTimeout(onDone, 2400);
    return () => clearTimeout(t);
  }, [state, onDone]);
  const save = async () => {
    const rel = path.trim();
    if (!rel) return setState({ kind: "error", message: "Enter a workspace path" });
    setState({ kind: "saving" });
    try {
      setState({ kind: "saved", path: await onSave(rel, planToSave(text)) });
    } catch (e) {
      setState({ kind: "error", message: `Could not save plan: ${e instanceof Error ? e.message : String(e)}` });
    }
  };
  if (state.kind === "saved") {
    return (
      <p role="status" className="rise mb-2 flex min-w-0 items-center gap-1.5 text-[12.5px] text-muted">
        <Check className="size-3.5 shrink-0 text-accent" aria-hidden /> Plan saved to workspace <code className="min-w-0 truncate font-mono text-[12px] text-fg">{state.path}</code>
      </p>
    );
  }
  return (
    <form
      aria-label="Save plan to workspace"
      className="rise mb-3 rounded-xl border border-line bg-bg/60 p-2.5"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.preventDefault();
          e.stopPropagation();
          onDone();
        }
      }}
    >
      <label className="mb-1.5 block text-[12px] text-muted" htmlFor="plan-save-path">
        Workspace path <span className="text-subtle">· relative to the thread&apos;s folder</span>
      </label>
      <div className="flex items-center gap-1.5">
        <input
          ref={input}
          id="plan-save-path"
          value={path}
          onChange={(e) => {
            setPath(e.target.value);
            if (state.kind === "error") setState({ kind: "editing" });
          }}
          spellCheck={false}
          autoComplete="off"
          className="h-8 min-w-0 flex-1 rounded-md border border-line bg-panel px-2 font-mono text-[12.5px] outline-none focus-visible:border-accent/60"
        />
        <button type="button" onClick={onDone} className="h-8 rounded-md px-2.5 text-[12.5px] text-muted hover:bg-fg/[0.06] hover:text-fg">
          Cancel
        </button>
        <button type="submit" disabled={state.kind === "saving"} className="h-8 rounded-md bg-fg px-3 text-[12.5px] font-medium text-bg transition hover:opacity-90 disabled:opacity-60">
          {state.kind === "saving" ? "Saving..." : "Save"}
        </button>
      </div>
      {state.kind === "error" && (
        <p role="alert" className="mt-1.5 text-[12px] text-danger">
          {state.message}
        </p>
      )}
    </form>
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
