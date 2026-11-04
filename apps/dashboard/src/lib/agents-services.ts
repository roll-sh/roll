/**
 * What the canvas asks of roll besides the agents state and commands: sandboxes, the owner's roll
 * code repos, starting a repo as a project, a thread's work item, and the canvas's own arrangement.
 * `httpServices` talks to the relay and roll code; the demo answers the same interface in memory
 * (`agents-demo.ts`), and component tests pass fakes.
 */

import {
  agentsApi,
  CanvasConflict,
  MAX_CANVAS,
  type AgentsApi,
  type MachineId,
  type NewRepoProject,
  type Reply,
  type SavedCanvas,
  type TakeWork,
  type ThreadId,
  type WorkItemRef,
} from "./agents-api";
import { ApiError, codeApi, type Repo } from "./code-api";
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
  /**
   * The canvas as the owner arranged it, kept by roll so every device shows the same. Opaque here:
   * the canvas parses it. `load` is `null` before the first save. `save` with `revision` saves only
   * over that revision (0: none yet) and otherwise throws a `CanvasConflict` holding what is there
   * now; without it the last writer wins. Other devices hear a save as `AgentsData.canvas`.
   */
  canvas: {
    load(owner: string): Promise<{ doc: unknown; revision: number } | null>;
    save(owner: string, doc: unknown, revision?: number): Promise<{ revision: number }>;
  };
}

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);

/** `canvas` over the relay. */
export function httpCanvas(api: AgentsApi = agentsApi): AgentsServices["canvas"] {
  return {
    load: async (owner) => {
      const c = await api.canvas(owner);
      return c.canvas === null ? null : { doc: c.canvas, revision: c.revision };
    },
    save: async (owner, doc, revision) => {
      if (!isObject(doc)) throw new TypeError("the canvas must be a JSON object");
      return { revision: (await api.saveCanvas(owner, doc, revision)).revision };
    },
  };
}

/**
 * `canvas` in memory, for the demo and tests: what `httpCanvas` does against the relay
 * (revisions, conflicts, a copy kept rather than the caller's object), without the network.
 */
export function memoryCanvas(): AgentsServices["canvas"] {
  const saved = new Map<string, { doc: string; revision: number; updated_at: number }>();
  const view = (owner: string): SavedCanvas => {
    const s = saved.get(owner);
    return s ? { canvas: JSON.parse(s.doc) as Record<string, unknown>, revision: s.revision, updated_at: s.updated_at } : { canvas: null, revision: 0, updated_at: null };
  };
  return {
    load: async (owner) => {
      const s = saved.get(owner);
      return s ? { doc: JSON.parse(s.doc) as unknown, revision: s.revision } : null;
    },
    save: async (owner, doc, revision) => {
      if (!isObject(doc)) throw new TypeError("the canvas must be a JSON object");
      const json = JSON.stringify(doc);
      if (new TextEncoder().encode(json).length > MAX_CANVAS) throw new ApiError(`the canvas can be at most ${MAX_CANVAS} bytes of JSON`, 413, null);
      const current = saved.get(owner)?.revision ?? 0;
      if (revision !== undefined && revision !== current) throw new CanvasConflict(view(owner));
      saved.set(owner, { doc: json, revision: current + 1, updated_at: Date.now() });
      return { revision: current + 1 };
    },
  };
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
    canvas: httpCanvas(api),
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
