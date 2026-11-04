/**
 * A goal typed outside a conversation (the command palette, the canvas's prompt bar), turned into
 * a new thread: "Orchestrate" starts an orchestrator that plans the goal and hands the parts to
 * subagents on the canvas under it (`thread.create {orchestrator}`, docs/code/agents.md
 * "Delegation"); "New agent" starts one agent with the goal as its prompt. Both start where the
 * last new agent did (the project the new-agent composer remembers), from that project's
 * new-thread defaults, and full access unless the defaults say otherwise.
 */

import { useCallback, useMemo } from "react";
import type { Command, Machine, Project, ProviderInfo, Thread } from "@/lib/agents-api";
import { reachable } from "@/lib/agents-api";
import type { AgentsClient } from "@/lib/agents-client";
import { resolveModel } from "./model-picker";
import { defaultProject, LAST_PROJECT } from "./new-agent";
import { useAgents } from "./use-agents";

export type GoalKind = "orchestrate" | "agent";

export type ThreadCreate = Extract<Command, { type: "thread.create" }>;

/** A project with the machine its record came from. */
export type PlacedProject = Project & { machine: string };

/** Where a goal goes: a project on a machine that can take it. */
export interface GoalTarget {
  project: PlacedProject;
  machine: Machine;
}

/** The provider a new thread there runs: the project's default if the machine has it, else the first logged-in one, else the first installed. */
export function goalProvider(project: Project, machine: Machine): ProviderInfo | undefined {
  const installed = (machine.info?.providers ?? []).filter((p) => p.installed);
  const wanted = project.settings?.defaults?.provider;
  return installed.find((p) => p.id === wanted) ?? installed.find((p) => p.auth === "ready") ?? installed[0];
}

/**
 * The `thread.create` for `goal`, or why it can't start there. An orchestrator stays in the
 * checkout and in default mode (it plans, and gives each part its own worktree); one agent takes
 * the project's defaults like the composer does.
 */
export function goalCommand(kind: GoalKind, goal: string, { project, machine }: GoalTarget): ThreadCreate | { error: string } {
  const prompt = goal.trim();
  if (!prompt) return { error: "Say what should get done." };
  if (!reachable(machine)) return { error: `${machine.name} is offline.` };
  const provider = goalProvider(project, machine);
  if (!provider) return { error: `No agent is installed on ${machine.name}.` };
  const d = project.settings?.defaults ?? {};
  const same = d.provider === provider.id;
  const model = resolveModel(provider, same ? d.model : null);
  return {
    type: "thread.create",
    project: project.id,
    provider: provider.id,
    ...(model ? { model: model.id } : {}),
    ...(same && d.effort ? { effort: d.effort } : {}),
    runtime_mode: d.runtime_mode ?? "full_access",
    interaction_mode: kind === "orchestrate" ? "default" : (d.interaction_mode ?? "default"),
    worktree: kind === "orchestrate" ? false : (d.worktree ?? !!project.repo),
    prompt,
    ...(kind === "orchestrate" ? { orchestrator: true } : {}),
  };
}

function lastProjectId(): string | null {
  try {
    return localStorage.getItem(LAST_PROJECT);
  } catch {
    return null;
  }
}

/** Remember where the last goal went, for the next one and the new-agent composer. */
export function rememberProject(id: string) {
  try {
    localStorage.setItem(LAST_PROJECT, id);
  } catch {
    // Not remembered.
  }
}

/** Where a goal goes by default: the last project used (on a machine that can take it), else the one with the newest thread. */
export function defaultTarget(projects: readonly PlacedProject[], machines: ReadonlyMap<string, Machine>, threads: readonly Pick<Thread, "project" | "updated_at">[]): GoalTarget | undefined {
  const usable = projects.filter((p) => {
    const m = machines.get(p.machine);
    return m && reachable(m);
  });
  const project = defaultProject(usable, threads, undefined, lastProjectId());
  const machine = project && machines.get(project.machine);
  return project && machine ? { project, machine } : undefined;
}

/** Where goals can go: the default first, then every other project on a machine that can take one. */
export function useGoalTargets(): { target: GoalTarget | undefined; targets: GoalTarget[] } {
  const { client } = useAgents();
  const data = client.data;
  return useMemo(() => {
    const machines = new Map([...data.machines.values()].map((r) => [r.value.id, r.value]));
    const projects = [...data.projects.values()].map((r) => ({ ...r.value, machine: r.machine }));
    const threads = [...data.threads.values()].map((r) => r.value);
    const target = defaultTarget(projects, machines, threads);
    const targets = projects
      .flatMap((project) => {
        const machine = machines.get(project.machine);
        return machine && reachable(machine) ? [{ project, machine }] : [];
      })
      .sort((a, b) => Number(b.project.id === target?.project.id) - Number(a.project.id === target?.project.id) || a.project.name.localeCompare(b.project.name));
    return { target, targets };
  }, [data.machines, data.projects, data.threads]);
}

/** Start a goal there and open its conversation beside the canvas; rejects with what went wrong. */
export function useLaunchGoal() {
  const { client, openThread } = useAgents();
  return useCallback(
    async (kind: GoalKind, goal: string, target: GoalTarget) => {
      const command = goalCommand(kind, goal, target);
      if ("error" in command) throw new Error(command.error);
      const r = await client.run<{ type: "created"; id: string }>(target.machine.id, command, "created");
      rememberProject(target.project.id);
      // The reply comes before the store has applied the thread's record (it applies on the next
      // frame), and a conversation that isn't there falls back to the canvas.
      await recordOf(client, r.id);
      openThread(r.id);
      return r.id;
    },
    [client, openThread],
  );
}

/** Once the client has `thread` (at most a couple of seconds). */
function recordOf(client: AgentsClient, thread: string): Promise<void> {
  if (client.data.threads.has(thread)) return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => {
      clearTimeout(timer);
      stop();
      resolve();
    };
    const timer = setTimeout(done, 2000);
    const stop = client.subscribe(() => client.data.threads.has(thread) && done());
  });
}

/** "roll on ada-mbp", "roll" when there's one machine. */
export function targetLabel(t: GoalTarget, machines: number): string {
  return machines > 1 ? `${t.project.name} on ${t.machine.name}` : t.project.name;
}
