/**
 * Where a new thread starts and what it starts with, one set of rules for every way to start one:
 * the launcher (`new-agent.tsx`, its composer), the canvas's prompt bar and the command palette's
 * goals (`orchestrate.ts`).
 *
 * - **Where**: the project asked for, else the last one a thread was started in (this browser),
 *   else the one with the newest thread, else the first; a project on a machine that can take a
 *   thread beats one that can't. The caller decides once and keeps it: a draft's target never
 *   moves because some thread was updated (`prompt-bar.tsx` pins it for the draft's life).
 * - **With what**: the new-thread defaults of the project, then its machine's, then the owner's
 *   (`newThreadDefaults`), where the machine has their provider; else the first logged-in
 *   provider's default model. Full access unless the defaults say otherwise; a worktree in a roll
 *   code project.
 * - **Starting**: `thread.create`, remember the project, wait for the record, open it beside the
 *   canvas (`useStartThread`).
 */

import { useCallback } from "react";
import type { Command, InteractionMode, Machine, Project, ProviderInfo, RuntimeMode, Thread, ThreadDefaults } from "@/lib/agents-api";
import { commandId, expectOk, reachable } from "@/lib/agents-api";
import type { AgentsClient } from "@/lib/agents-client";
import { acceptedOptions } from "@/lib/agents-model-options";
import { newThreadDefaults } from "@/lib/owner-settings";
import { DEFAULT_RUNTIME_MODE, resolveModel, type ModelChoice } from "./model-picker";
import { useAgents } from "./use-agents";

export const LAST_PROJECT = "roll:agents:last-project";

export type ThreadCreate = Extract<Command, { type: "thread.create" }>;

/** A project with the machine its record came from. */
export type PlacedProject = Project & { machine: string };

/** Where a new thread goes: a project on a machine. */
export interface Target {
  project: PlacedProject;
  machine: Machine;
}

/** What a new thread starts with unless the user picks otherwise. */
export interface NewThreadSettings extends ModelChoice {
  runtime_mode: RuntimeMode;
  interaction_mode: InteractionMode;
  worktree: boolean;
}

/** The project the last thread was started in, on this browser. */
export function lastProject(): string | null {
  try {
    return localStorage.getItem(LAST_PROJECT);
  } catch {
    return null;
  }
}

/** Remember where the last thread went, for the next one (launcher, prompt bar, palette). */
export function rememberProject(id: string) {
  try {
    localStorage.setItem(LAST_PROJECT, id);
  } catch {
    // Not remembered.
  }
}

/**
 * Where a new thread starts by default: `wanted` if it exists; else, among the projects `usable`
 * says can take one (all by default), the last one used, the one with the newest thread, the
 * first; else the same among the rest (so an offline machine's project still shows, as offline).
 */
export function defaultProject<P extends Project>(
  projects: readonly P[],
  threads: readonly Pick<Thread, "project" | "updated_at">[],
  wanted?: string,
  last?: string | null,
  usable: (p: P) => boolean = () => true,
): P | undefined {
  const asked = wanted ? projects.find((p) => p.id === wanted) : undefined;
  if (asked) return asked;
  const newest = [...threads].sort((a, b) => b.updated_at - a.updated_at);
  const pick = (pool: readonly P[]) => {
    const by = (id: string | null | undefined) => (id ? pool.find((p) => p.id === id) : undefined);
    return by(last) ?? newest.map((t) => by(t.project)).find(Boolean) ?? pool[0];
  };
  return pick(projects.filter(usable)) ?? pick(projects);
}

/** The installed providers on `machine`. */
export function installedProviders(machine: Machine | undefined): ProviderInfo[] {
  return (machine?.info?.providers ?? []).filter((p) => p.installed);
}

/** The default provider: the first logged-in one, else the first installed. */
export function defaultProvider(providers: readonly ProviderInfo[]): ProviderInfo | undefined {
  return providers.find((p) => p.installed && p.auth === "ready") ?? providers.find((p) => p.installed);
}

/**
 * What a new thread in `project` on `machine` starts with: the project's, then the machine's, then
 * the owner's defaults (`owner`) where the machine has their provider, else the default provider's
 * default model. `provider` is "" when the machine has none installed.
 */
export function newThreadSettings(project: Project, machine: Machine | undefined, owner?: ThreadDefaults): NewThreadSettings {
  const providers = installedProviders(machine);
  const d = newThreadDefaults([project.settings?.defaults, machine?.info?.settings?.set.defaults, owner], providers.map((p) => p.id));
  const preset = providers.find((p) => p.id === d.provider);
  const provider = preset ?? defaultProvider(providers);
  const model = resolveModel(provider, preset ? d.model : null);
  return {
    provider: provider?.id ?? "",
    model: model?.id ?? null,
    effort: preset ? (d.effort ?? null) : null,
    options: preset ? acceptedOptions(model, d.options) : {},
    runtime_mode: d.runtime_mode ?? DEFAULT_RUNTIME_MODE,
    interaction_mode: d.interaction_mode ?? "default",
    // A roll code project publishes threads as changes, which needs a worktree: on by default there.
    worktree: d.worktree ?? !!project.repo,
  };
}

/** `choice` if the machine still has its provider, else the new-thread defaults' model. */
export function choiceOn(machine: Machine, choice: ModelChoice | undefined, defaults: ModelChoice): ModelChoice {
  return choice && installedProviders(machine).some((p) => p.id === choice.provider) ? choice : defaults;
}

/** Every project with its machine, and the targets that can take a thread now. */
export function placedProjects(data: AgentsClient["data"]): { projects: PlacedProject[]; machines: Map<string, Machine> } {
  const machines = new Map([...data.machines.values()].map((r) => [r.value.id, r.value]));
  const projects = [...data.projects.values()].map((r) => ({ ...r.value, machine: r.machine }));
  return { projects, machines };
}

/** "roll on ada-mbp", "roll" when there's one machine. */
export function targetLabel(t: Target, machines: number): string {
  return machines > 1 ? `${t.project.name} on ${t.machine.name}` : t.project.name;
}

/** Whether a project's machine can take a new thread now. */
export function usableIn(machines: ReadonlyMap<string, Machine>) {
  return (p: PlacedProject) => {
    const m = machines.get(p.machine);
    return !!m && reachable(m);
  };
}

/**
 * Start a thread: send `command` (under `id` when given: the same id never starts a second thread,
 * the daemon replays its reply), remember its project, and open it beside the canvas once its
 * record is in, unless `open` is false (several models at once open only the first, once all
 * started). Answers the thread's id; rejects with what went wrong.
 */
export function useStartThread() {
  const { client, openThread } = useAgents();
  return useCallback(
    async (machine: Machine, command: ThreadCreate, id = commandId(), open = true) => {
      const r = expectOk<{ type: "created"; id: string }>(await client.send(machine.id, command, id), "created");
      rememberProject(command.project);
      await recordOf(client, r.id);
      if (open) openThread(r.id);
      return r.id;
    },
    [client, openThread],
  );
}

/**
 * Once the client has `thread` (at most a couple of seconds): the reply comes before the store has
 * applied the thread's record (it applies on the next frame), and a conversation that isn't there
 * falls back to the canvas.
 */
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
