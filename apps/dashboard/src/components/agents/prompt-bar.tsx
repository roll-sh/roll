"use client";

import { ArrowUp, Bot, FolderPlus, Workflow, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode, type Ref } from "react";
import { reachable } from "@/lib/agents-api";
import { openPalette } from "@/lib/agents-palette";
import { ProjectIcon } from "./identity";
import { ModelPicker, type ModelChoice } from "./model-picker";
import { choiceOn, installedProviders, newThreadSettings, placedProjects, targetLabel, type Target } from "./new-thread";
import { editGoalDraft, emptyGoalDraft, goalDraftKey, mayHaveStarted, parseGoalDraft, sentGoalDraft, useGoalTargets, useLaunchGoal, type GoalDraft, type GoalKind } from "./orchestrate";
import { Select, type SelectOption } from "./ui";
import { useAgents, useAgentsSnapshot, usePrefs } from "./use-agents";

const ADD = "+";
const KINDS = [
  ["orchestrate", "Orchestrate", Workflow, "An orchestrator plans it and spins up agents"],
  ["agent", "One agent", Bot, "One agent with this as its prompt"],
] as const;

/**
 * "What should the agents get done?" at the bottom of the canvas: a goal typed here starts an
 * orchestrator that plans it and spins up agents under it on the canvas, or (switched to "One
 * agent") a single agent with it as its prompt. Where it goes and what it runs on follow the
 * launcher's rules (`new-thread.ts`); once there's a draft the project is pinned, and only the
 * project chip moves it. The draft is kept per owner (`GoalDraft`), so it survives the
 * conversation that opens on Enter, a reload, and the phone's panels.
 *
 * A quiet pill until it's focused or holds something; then T3's composer shape: the text, a line
 * for what went wrong, and a toolbar. Centred over the canvas when it's wide; across it, above the
 * zoom controls, when it's narrow (a side panel open, a small window, a phone), with `above`
 * (the phone's "Waiting for you") stacked over it. `ref` is the whole dock, for the canvas to keep
 * its cards clear of.
 */
export function PromptBar({ ref, above }: { ref?: Ref<HTMLDivElement>; above?: ReactNode }) {
  const { client, owner } = useAgents();
  // Its own subscription: what it shows can't hang on whoever renders it.
  const data = useAgentsSnapshot(client)?.data ?? client.data;
  const prefs = usePrefs();
  const { target: fallback, targets } = useGoalTargets();
  const launch = useLaunchGoal();
  const [draft, setDraft] = useState<GoalDraft>(emptyGoalDraft);
  const [loaded, setLoaded] = useState(false);
  const [focused, setFocused] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const area = useRef<HTMLTextAreaElement>(null);
  const box = useRef<HTMLFormElement>(null);
  // The start being waited on; Cancel stops waiting for it (it may still start: the draft keeps its id).
  const waiting = useRef(0);

  // The draft, kept per owner in this browser.
  useEffect(() => {
    try {
      setDraft(parseGoalDraft(localStorage.getItem(goalDraftKey(owner))));
    } catch {}
    setLoaded(true);
  }, [owner]);
  useEffect(() => {
    if (!loaded) return;
    const t = setTimeout(() => {
      try {
        localStorage.setItem(goalDraftKey(owner), JSON.stringify(draft));
      } catch {}
    }, 300);
    return () => clearTimeout(t);
  }, [draft, owner, loaded]);

  // Where it goes: the pinned project (wherever its machine is now; a removed one pins nothing),
  // else the default.
  const { projects, machines } = useMemo(() => placedProjects(data), [data]);
  const pinnedProject = draft.project ? projects.find((p) => p.id === draft.project) : undefined;
  const pinnedMachine = pinnedProject && machines.get(pinnedProject.machine);
  const pinned: Target | undefined = pinnedProject && pinnedMachine ? { project: pinnedProject, machine: pinnedMachine } : undefined;
  const target = pinned ?? fallback;
  const defaults = target ? newThreadSettings(target.project, target.machine, prefs.defaults) : undefined;
  const choice = target && defaults?.provider ? choiceOn(target.machine, draft.choice ?? undefined, defaults) : undefined;
  const open = focused || draft.text.length > 0 || draft.picked || !!error || busy;

  const edit = (patch: Partial<Omit<GoalDraft, "id">>) => {
    setError(null);
    // Something's being drafted: it stays where it is now until the user moves it.
    setDraft((d) => editGoalDraft(d, pinned || !target ? patch : { project: target.project.id, ...patch }));
  };
  const pin = () => {
    if (!pinned && target) setDraft((d) => ({ ...d, project: target.project.id }));
  };

  // Grows with the text, up to six lines.
  useEffect(() => {
    const el = area.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 144)}px`;
  }, [draft.text, open]);

  const submit = async () => {
    const goal = draft.text.trim();
    if (!goal || busy) return;
    if (!target) return openPalette({ kind: "add-project" });
    const sent = draft;
    const token = ++waiting.current;
    setBusy(true);
    setError(null);
    try {
      await launch(sent.kind, goal, target, choice, sent.id);
      if (waiting.current !== token) return;
      // Only what was sent goes: anything typed meanwhile stays.
      setDraft((d) => (d.id === sent.id ? sentGoalDraft(d) : d));
      area.current?.blur();
    } catch (e) {
      if (waiting.current !== token) return;
      setError(e instanceof Error ? e.message : String(e));
      if (!mayHaveStarted(e)) setDraft((d) => (d.id === sent.id ? editGoalDraft(d, {}) : d));
    } finally {
      if (waiting.current === token) setBusy(false);
    }
  };
  const cancel = () => {
    waiting.current++;
    setBusy(false);
    area.current?.focus();
  };

  // Every project that can take a goal, plus the pinned one if its machine can't right now.
  const options = useMemo(() => {
    const option = (t: Target, hint?: string): SelectOption => ({ value: t.project.id, label: targetLabel(t, machines.size), icon: <ProjectIcon project={t.project} className="size-3.5" />, hint });
    const list = targets.map((t) => option(t));
    if (target && !targets.some((t) => t.project.id === target.project.id)) list.unshift(option(target, "offline"));
    return [...list, { value: ADD, label: "Add a project…", icon: <FolderPlus className="size-3.5 text-subtle" /> }];
  }, [targets, target, machines.size]);
  const offline = !!target && !reachable(target.machine);

  return (
    <div ref={ref} className="@container/bar pointer-events-none absolute inset-x-0 bottom-0 z-20 flex flex-col items-center gap-2 px-3" data-nodrag>
      {above && <div className="pointer-events-auto w-full">{above}</div>}
      <form
        ref={box}
        aria-label="Ask agents"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
        onFocus={() => {
          setFocused(true);
          pin();
        }}
        onBlur={(e) => {
          const to = e.relatedTarget as HTMLElement | null;
          // The pickers' popups are portaled out of the form; the bar stays open while they're in use.
          if (box.current?.contains(to) || to?.closest('[data-model-picker], [data-option-menu], [role="listbox"]')) return;
          setFocused(false);
          // Nothing drafted: back to following the default.
          setDraft((d) => (!d.text && !d.picked && d.project ? { ...d, project: null } : d));
        }}
        className={`palette-glass pointer-events-auto mb-[max(3.5rem,calc(env(safe-area-inset-bottom)+3rem))] w-full overflow-hidden transition-[border-radius,box-shadow] duration-200 ease-out @min-[55rem]/bar:mb-4 @min-[55rem]/bar:w-[min(640px,calc(100%-25rem))] ${open ? "rounded-2xl" : "rounded-full"}`}
      >
        <div className={`flex items-start gap-2 ${open ? "px-3.5 pt-3" : "px-4"}`}>
          {!open && <Workflow className="mt-3 size-4 shrink-0 text-accent" aria-hidden />}
          <textarea
            ref={area}
            rows={1}
            value={draft.text}
            onChange={(e) => edit({ text: e.target.value })}
            onKeyDown={(e) => {
              if (e.nativeEvent.isComposing) return;
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                void submit();
              } else if (e.key === "Escape") {
                e.preventDefault();
                e.stopPropagation();
                if (busy) cancel();
                else area.current?.blur();
              }
            }}
            placeholder={draft.kind === "orchestrate" ? "What should the agents get done?" : "What should a new agent do?"}
            aria-label={draft.kind === "orchestrate" ? "A goal for agents" : "A prompt for a new agent"}
            className={`min-w-0 flex-1 resize-none bg-transparent text-[13.5px] leading-[1.45] text-fg outline-none placeholder:text-subtle ${open ? "min-h-[44px] py-0.5" : "h-10 py-2.5"}`}
          />
          {!open && (
            <kbd className="mt-2.5 shrink-0 rounded bg-fg/[0.08] px-1.5 py-0.5 font-sans text-[11px] text-muted max-sm:hidden" title="The command palette">
              ⌘K
            </kbd>
          )}
        </div>
        {open && error && (
          <p role="alert" className="mx-3.5 mt-1.5 flex items-start gap-2 rounded-lg bg-danger/[0.08] px-2.5 py-1.5 text-[12px] leading-[1.45] text-danger">
            <span className="min-w-0 flex-1 break-words">{error}</span>
            <button type="button" onClick={() => setError(null)} aria-label="Dismiss" className="mt-px shrink-0 rounded opacity-70 hover:opacity-100">
              <X className="size-3.5" />
            </button>
          </p>
        )}
        {open && (
          <div className="palette-view flex flex-wrap items-center gap-1.5 px-2.5 pt-1.5 pb-2.5">
            <div
              role="radiogroup"
              aria-label="Who does it"
              className="flex shrink-0 rounded-lg bg-fg/[0.05] p-0.5 text-[12px]"
              onKeyDown={(e) => {
                if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
                e.preventDefault();
                const next: GoalKind = draft.kind === "orchestrate" ? "agent" : "orchestrate";
                edit({ kind: next });
                box.current?.querySelector<HTMLElement>(`[data-kind="${next}"]`)?.focus();
              }}
            >
              {KINDS.map(([k, label, Icon, title]) => (
                <button
                  key={k}
                  type="button"
                  role="radio"
                  data-kind={k}
                  aria-checked={draft.kind === k}
                  tabIndex={draft.kind === k ? 0 : -1}
                  onClick={() => {
                    edit({ kind: k });
                    area.current?.focus();
                  }}
                  className={`flex items-center gap-1.5 rounded-md px-2 py-1 whitespace-nowrap transition-colors ${draft.kind === k ? "bg-fg/[0.1] text-fg" : "text-muted hover:text-fg"}`}
                  title={title}
                >
                  <Icon className={`size-3.5 ${draft.kind === k && k === "orchestrate" ? "text-accent" : ""}`} />
                  {/* A phone keeps the icons; the name is still what it's called. */}
                  <span className="@max-[30rem]/bar:sr-only">{label}</span>
                </button>
              ))}
            </div>
            {target ? (
              <Select
                label="Project"
                variant="ghost"
                side="top"
                title="Where it starts"
                className={`h-7 max-w-[14rem] min-w-0 rounded-md @max-[30rem]/bar:max-w-[9rem] px-2 text-[12px] ${offline ? "text-danger" : "text-muted hover:text-fg"}`}
                icon={<ProjectIcon project={target.project} className="size-3.5" />}
                value={target.project.id}
                onChange={(id) => {
                  if (id === ADD) return openPalette({ kind: "add-project" });
                  edit({ project: id, picked: true });
                  area.current?.focus();
                }}
                options={options}
              />
            ) : (
              <button type="button" onClick={() => openPalette({ kind: "add-project" })} className="flex h-7 items-center gap-1 rounded-md px-2 text-[12px] text-muted hover:bg-fg/[0.06] hover:text-fg">
                <FolderPlus className="size-3.5" />
                Add a project
              </button>
            )}
            {target && choice && (
              <ModelPicker
                providers={installedProviders(target.machine)}
                value={choice}
                onChange={(c: ModelChoice) => edit({ choice: c })}
                side="top"
              />
            )}
            <span className="flex-1" />
            {busy && (
              <span className="flex shrink-0 items-center gap-2 text-[12px]">
                <span className="shimmer text-muted">Starting…</span>
                <button type="button" onClick={cancel} className="rounded-md px-1.5 py-0.5 text-muted hover:bg-fg/[0.06] hover:text-fg" title="Stop waiting (Esc). Sending it again never starts it twice.">
                  Cancel
                </button>
              </span>
            )}
            <button
              type="submit"
              disabled={!draft.text.trim() || busy}
              aria-label={draft.kind === "orchestrate" ? "Orchestrate" : "Start the agent"}
              title={draft.kind === "orchestrate" ? "Orchestrate (Enter)" : "Start the agent (Enter)"}
              className="grid size-7 shrink-0 place-items-center rounded-full bg-fg text-bg transition hover:opacity-85 disabled:opacity-30"
            >
              <ArrowUp className="size-4" strokeWidth={2} />
            </button>
          </div>
        )}
      </form>
    </div>
  );
}
