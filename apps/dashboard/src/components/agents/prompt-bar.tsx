"use client";

import { ArrowUp, Bot, ChevronDown, Workflow } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { openPalette } from "@/lib/agents-palette";
import { targetLabel, useGoalTargets, useLaunchGoal, type GoalKind } from "./orchestrate";
import { useAgents } from "./use-agents";

const KIND = "roll:agents:prompt-kind";

/**
 * "What should get done?" at the bottom of the canvas: a goal typed here starts an orchestrator
 * that plans it and spins up agents under it on the canvas, or (switched to "One agent") a single
 * agent with it as its prompt, in the last project used. The project chip opens the command
 * palette to pick another. A quiet pill until it's focused or holds text; then T3's composer
 * shape: the text, and a toolbar under it.
 */
export function PromptBar() {
  const { client } = useAgents();
  const { target } = useGoalTargets();
  const launch = useLaunchGoal();
  const [text, setText] = useState("");
  const [kind, setKind] = useState<GoalKind>("orchestrate");
  const [focused, setFocused] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const area = useRef<HTMLTextAreaElement>(null);
  const box = useRef<HTMLFormElement>(null);
  const open = focused || text.length > 0 || !!error;

  useEffect(() => {
    try {
      if (localStorage.getItem(KIND) === "agent") setKind("agent");
    } catch {}
  }, []);
  const pick = (k: GoalKind) => {
    setKind(k);
    try {
      localStorage.setItem(KIND, k);
    } catch {}
    area.current?.focus();
  };

  // Grows with the text, up to six lines.
  useEffect(() => {
    const el = area.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 144)}px`;
  }, [text, open]);

  const submit = async () => {
    const goal = text.trim();
    if (!goal || busy) return;
    if (!target) return openPalette({ kind: "add-project" });
    setBusy(true);
    setError(null);
    try {
      await launch(kind, goal, target);
      setText("");
      area.current?.blur();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const where = target ? targetLabel(target, client.data.machines.size) : "Add a project";
  return (
    <form
      ref={box}
      aria-label="Ask agents"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
      onFocus={() => setFocused(true)}
      onBlur={(e) => !box.current?.contains(e.relatedTarget as Node) && setFocused(false)}
      className={`palette-glass absolute bottom-4 left-1/2 z-10 w-[min(640px,calc(100%-30rem))] min-w-72 -translate-x-1/2 overflow-hidden transition-[border-radius,box-shadow] duration-200 ease-out ${open ? "rounded-2xl" : "rounded-full"}`}
      data-nodrag
    >
      <div className={`flex items-start gap-2 ${open ? "px-3.5 pt-3" : "px-4"}`}>
        {!open && <Workflow className="mt-3 size-4 shrink-0 text-accent" aria-hidden />}
        <textarea
          ref={area}
          rows={1}
          value={text}
          onChange={(e) => {
            setText(e.target.value);
            setError(null);
          }}
          onKeyDown={(e) => {
            if (e.nativeEvent.isComposing) return;
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void submit();
            } else if (e.key === "Escape") {
              e.preventDefault();
              e.stopPropagation();
              area.current?.blur();
            }
          }}
          placeholder={kind === "orchestrate" ? "What should the agents get done?" : "What should a new agent do?"}
          aria-label={kind === "orchestrate" ? "A goal for agents" : "A prompt for a new agent"}
          className={`min-w-0 flex-1 resize-none bg-transparent text-[13.5px] leading-[1.45] text-fg outline-none placeholder:text-subtle ${open ? "min-h-[44px] py-0.5" : "h-10 py-2.5"}`}
        />
        {!open && (
          <kbd className="mt-2.5 shrink-0 rounded bg-fg/[0.08] px-1.5 py-0.5 font-sans text-[11px] text-muted max-sm:hidden" title="The command palette">
            ⌘K
          </kbd>
        )}
      </div>
      {open && (
        <div className="palette-view flex items-center gap-1.5 px-2.5 pt-1 pb-2.5">
          <div role="radiogroup" aria-label="Who does it" className="flex rounded-lg bg-fg/[0.05] p-0.5 text-[12px]">
            {(
              [
                ["orchestrate", "Orchestrate", Workflow],
                ["agent", "One agent", Bot],
              ] as const
            ).map(([k, label, Icon]) => (
              <button
                key={k}
                type="button"
                role="radio"
                aria-checked={kind === k}
                onClick={() => pick(k)}
                className={`flex items-center gap-1.5 rounded-md px-2 py-1 transition-colors ${kind === k ? "bg-fg/[0.1] text-fg" : "text-muted hover:text-fg"}`}
                title={k === "orchestrate" ? "An orchestrator plans it and spins up agents" : "One agent with this as its prompt"}
              >
                <Icon className={`size-3.5 ${kind === k && k === "orchestrate" ? "text-accent" : ""}`} />
                {label}
              </button>
            ))}
          </div>
          <button
            type="button"
            onClick={() => openPalette({ kind: "orchestrate", goal: text })}
            className="flex min-w-0 items-center gap-1 rounded-md px-2 py-1 text-[12px] text-muted transition-colors hover:bg-fg/[0.06] hover:text-fg"
            title="Start it somewhere else"
          >
            <span className="truncate">{where}</span>
            <ChevronDown className="size-3 shrink-0" />
          </button>
          {error ? <span className="min-w-0 flex-1 truncate text-right text-[12px] text-danger">{error}</span> : <span className="flex-1" />}
          {busy && <span className="shimmer shrink-0 text-[12px] text-muted">Starting…</span>}
          <button
            type="submit"
            disabled={!text.trim() || busy}
            aria-label={kind === "orchestrate" ? "Orchestrate" : "Start the agent"}
            className="grid size-7 shrink-0 place-items-center rounded-full bg-fg text-bg transition hover:opacity-85 disabled:opacity-30"
          >
            <ArrowUp className="size-4" strokeWidth={2} />
          </button>
        </div>
      )}
    </form>
  );
}
