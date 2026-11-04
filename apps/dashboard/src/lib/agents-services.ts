/**
 * What the canvas asks of roll besides the agents state and commands: sandboxes, the owner's roll
 * code repos, starting a repo as a project, and a thread's work item. `httpServices` talks to the
 * relay and roll code; the demo answers the same interface in memory (`agents-demo.ts`), and
 * component tests pass fakes.
 */

import { agentsApi, type AgentsApi, type MachineId, type NewRepoProject, type Reply, type TakeWork, type ThreadId, type WorkItemRef } from "./agents-api";
import { codeApi, type Repo } from "./code-api";
import { sandboxesApi, type SandboxesApi } from "./sandboxes-api";

/** A repo to clone onto a machine (only what the picker shows). */
export type RepoChoice = Pick<Repo, "owner" | "name" | "full_name" | "description" | "visibility" | "updated_at">;

export interface AgentsServices {
  sandboxes: Pick<SandboxesApi, "list" | "create" | "start" | "stop" | "remove" | "usage">;
  /** The owner's roll code repos, most recently updated first. */
  repos(owner: string): Promise<RepoChoice[]>;
  /** Create a roll code repo and clone it onto the machine as a project. */
  createProject(owner: string, machine: MachineId, req: NewRepoProject): Promise<Reply>;
  /** The thread takes a work item; `null` when there's none to claim. */
  takeWork(owner: string, thread: ThreadId, req: TakeWork): Promise<WorkItemRef | null>;
  releaseWork(owner: string, thread: ThreadId): Promise<void>;
}

export function httpServices(
  api: AgentsApi = agentsApi,
  sandboxes: SandboxesApi = sandboxesApi,
  repos: (owner: string) => Promise<Repo[]> = (owner) => codeApi.repos(owner),
): AgentsServices {
  return {
    sandboxes,
    repos: async (owner) =>
      (await repos(owner))
        // The list is everything the caller can see; only the owner's own repos can be cloned.
        .filter((r) => r.owner === owner)
        .sort((a, b) => b.updated_at - a.updated_at),
    createProject: api.createProject,
    takeWork: api.takeWork,
    releaseWork: api.releaseWork,
  };
}

/** `owner/repo#12`, `wi_…`: what the work field accepts. */
export function parseWorkRef(s: string): TakeWork | null {
  const t = s.trim();
  if (/^wi_[A-Za-z0-9]+$/.test(t)) return { work: t };
  if (/^[\w.-]+\/[\w.-]+#\d+$/.test(t)) return { work: t };
  if (/^[\w.-]+\/[\w.-]+$/.test(t)) return { repo: t };
  return null;
}
