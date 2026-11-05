/**
 * A goal typed outside a conversation (the command palette, the canvas's prompt bar), turned into
 * a new thread: "Orchestrate" starts an orchestrator that plans the goal and hands the parts to
 * subagents on the canvas under it (`thread.create {orchestrator}`, docs/code/agents.md
 * "Delegation"); "One agent" starts one agent with the goal as its prompt. Where and with what
 * follow the same rules as the launcher (`new-thread.ts`).
 */

import { useCallback, useMemo } from "react";
import type { ModelOptions, ThreadDefaults } from "@/lib/agents-api";
import { CommandError, commandId, reachable } from "@/lib/agents-api";
import type { ModelChoice } from "./model-picker";
import { choiceOn, defaultProject, lastProject, newThreadSettings, placedProjects, usableIn, useStartThread, type Target, type ThreadCreate } from "./new-thread";
import { useAgents, usePrefs } from "./use-agents";

export type GoalKind = "orchestrate" | "agent";

/**
 * The `thread.create` for `goal`, or why it can't start there. An orchestrator stays in the
 * checkout and in default mode (it plans, and gives each part its own worktree); one agent takes
 * the new-thread defaults like the launcher does. `choice` is a model picked for this goal (the
 * prompt bar's picker), used if the machine has its provider; else the defaults decide.
 */
export function goalCommand(kind: GoalKind, goal: string, { project, machine }: Target, choice?: ModelChoice, owner?: ThreadDefaults): ThreadCreate | { error: string } {
  const prompt = goal.trim();
  if (!prompt) return { error: "Say what should get done." };
  if (!reachable(machine)) return { error: `${machine.name} is offline. Start it with roll agent up, or pick another project.` };
  const d = newThreadSettings(project, machine, owner);
  if (!d.provider) return { error: `No agent is installed on ${machine.name}.` };
  const picked = choiceOn(machine, choice, d);
  return {
    type: "thread.create",
    project: project.id,
    provider: picked.provider,
    ...(picked.model ? { model: picked.model } : {}),
    ...(picked.effort ? { effort: picked.effort } : {}),
    ...(Object.keys(picked.options).length ? { options: picked.options } : {}),
    runtime_mode: d.runtime_mode,
    interaction_mode: kind === "orchestrate" ? "default" : d.interaction_mode,
    worktree: kind === "orchestrate" ? false : d.worktree,
    prompt,
    ...(kind === "orchestrate" ? { orchestrator: true } : {}),
  };
}

/**
 * Where goals can go: the default (the launcher's rule, among machines that can take a thread)
 * first, then every other project on such a machine, by name. The default follows the data; a
 * draft that must not move pins its own (`prompt-bar.tsx`).
 */
export function useGoalTargets(): { target: Target | undefined; targets: Target[] } {
  const { client } = useAgents();
  const data = client.data;
  return useMemo(() => {
    const { projects, machines } = placedProjects(data);
    const threads = [...data.threads.values()].map((r) => r.value);
    const targets = projects.filter(usableIn(machines)).map((project) => ({ project, machine: machines.get(project.machine)! }));
    const first = defaultProject(
      targets.map((t) => t.project),
      threads,
      undefined,
      lastProject(),
    );
    const target = targets.find((t) => t.project.id === first?.id);
    targets.sort((a, b) => Number(b === target) - Number(a === target) || a.project.name.localeCompare(b.project.name));
    return { target, targets };
    // The maps are replaced when they change; `data` itself isn't.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data.machines, data.projects, data.threads]);
}

/**
 * Start a goal there and open its conversation beside the canvas; rejects with what went wrong.
 * `id` is the command's idempotency key: sending the same draft again after a timeout never starts
 * it twice.
 */
export function useLaunchGoal() {
  const start = useStartThread();
  const owner = usePrefs().defaults;
  return useCallback(
    async (kind: GoalKind, goal: string, target: Target, choice?: ModelChoice, id?: string) => {
      const command = goalCommand(kind, goal, target, choice, owner);
      if ("error" in command) throw new Error(command.error);
      return start(target.machine, command, id);
    },
    [start, owner],
  );
}

// ── the prompt bar's draft ──

/**
 * What's typed in the prompt bar, kept per owner in this browser (T3's composer drafts) so it
 * survives a conversation opening, a reload, and the bar unmounting. `project` pins where it goes
 * for the draft's life (only the user moves it); `picked` says the user chose it with the chip (it
 * then stays until the goal is started, even with nothing typed). `choice` is the model picked
 * here; it stays for the next goal (T3's sticky model). `id` is the start command's idempotency
 * key: it changes with the draft, and after a failure only when the daemon surely didn't start it.
 */
export interface GoalDraft {
  text: string;
  kind: GoalKind;
  project: string | null;
  picked: boolean;
  choice: ModelChoice | null;
  id: string;
}

export const goalDraftKey = (owner: string) => `roll:agents:goal:${owner}`;

export function emptyGoalDraft(): GoalDraft {
  return { text: "", kind: "orchestrate", project: null, picked: false, choice: null, id: commandId() };
}

/** A saved draft, or an empty one for anything malformed (it's whatever this browser kept). */
export function parseGoalDraft(raw: string | null): GoalDraft {
  const d = emptyGoalDraft();
  let v: unknown;
  try {
    v = raw ? JSON.parse(raw) : null;
  } catch {
    return d;
  }
  if (!v || typeof v !== "object") return d;
  const o = v as Record<string, unknown>;
  const str = (x: unknown) => (typeof x === "string" ? x : null);
  const project = str(o.project);
  const id = str(o.id);
  return {
    text: str(o.text)?.slice(0, 100_000) ?? "",
    kind: o.kind === "agent" ? "agent" : "orchestrate",
    project,
    picked: o.picked === true && !!project,
    choice: parseChoice(o.choice),
    id: id?.startsWith("cmd_") ? id : d.id,
  };
}

function parseChoice(v: unknown): ModelChoice | null {
  if (!v || typeof v !== "object") return null;
  const c = v as Record<string, unknown>;
  if (typeof c.provider !== "string" || !c.provider) return null;
  const options: ModelOptions = {};
  if (c.options && typeof c.options === "object" && !Array.isArray(c.options))
    for (const [k, x] of Object.entries(c.options)) if (typeof x === "string" || typeof x === "boolean") options[k] = x;
  return { provider: c.provider, model: typeof c.model === "string" ? c.model : null, effort: typeof c.effort === "string" ? c.effort : null, options };
}

/** The draft with `patch` applied: a new start command, since it's a different goal now. */
export function editGoalDraft(d: GoalDraft, patch: Partial<Omit<GoalDraft, "id">>): GoalDraft {
  return { ...d, ...patch, id: commandId() };
}

/** After its goal started: the text and the pinned project go; the kind and the model stay. */
export function sentGoalDraft(d: GoalDraft): GoalDraft {
  return { ...emptyGoalDraft(), kind: d.kind, choice: d.choice };
}

/**
 * Whether a failed start may still have started (a timeout, a link that dropped, the network): then
 * sending the same draft again must reuse its command id, so the daemon replays instead of
 * starting a second one. A refusal gets a new id, so a retry after fixing things isn't the old error.
 */
export function mayHaveStarted(e: unknown): boolean {
  return !(e instanceof CommandError) || e.code === "timeout" || e.code === "machine_offline";
}
