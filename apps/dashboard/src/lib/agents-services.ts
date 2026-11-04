/**
 * What the canvas asks of roll besides the agents state and commands: sandboxes, renaming and
 * removing machines, the owner's roll code repos, starting a repo as a project, a thread's work
 * item, and the owner's documents (the canvas's arrangement, the agents settings).
 * `httpServices` talks to the relay and roll code; the demo answers the same interface in memory
 * (`agents-demo.ts`), and component tests pass fakes.
 */

import {
  agentsApi,
  CanvasConflict,
  MAX_CANVAS,
  MAX_SETTINGS,
  SettingsConflict,
  type AgentsApi,
  type Machine,
  type MachineId,
  type NewRepoProject,
  type Reply,
  type SavedCanvas,
  type SavedSettings,
  type TakeWork,
  type ThreadId,
  type WorkItemRef,
} from "./agents-api";
import { ApiError, codeApi, type Repo } from "./code-api";
import type { AutomationsApi } from "./automations-api";
import type { IntegrationsApi } from "./integrations-api";
import { sandboxesApi, type SandboxesApi } from "./sandboxes-api";

/** A repo to clone onto a machine (only what the picker shows). */
export type RepoChoice = Pick<Repo, "owner" | "name" | "full_name" | "description" | "visibility" | "updated_at">;

/**
 * A document roll keeps per owner, opaque here: `load` is `null` before the first save; `save`
 * with `revision` saves only over that revision (0: none yet) and otherwise throws a conflict
 * holding what is there now (`CanvasConflict`, `SettingsConflict`); without it the last writer wins.
 */
export interface OwnerDoc {
  load(owner: string): Promise<{ doc: unknown; revision: number } | null>;
  save(owner: string, doc: unknown, revision?: number): Promise<{ revision: number }>;
}

export interface AgentsServices {
  sandboxes: Pick<SandboxesApi, "list" | "create" | "start" | "stop" | "remove" | "usage">;
  /** Renaming and removing (revoking) a machine: the owner's, from its settings. */
  machines: {
    rename(owner: string, machine: MachineId, name: string): Promise<Machine>;
    remove(owner: string, machine: MachineId): Promise<void>;
  };
  /** The owner's roll code repos, most recently updated first. */
  repos(owner: string): Promise<RepoChoice[]>;
  /** Create a roll code repo and clone it onto the machine as a project. */
  createProject(owner: string, machine: MachineId, req: NewRepoProject): Promise<Reply>;
  /** The thread takes a work item; `null` when there's none to claim. */
  takeWork(owner: string, thread: ThreadId, req: TakeWork): Promise<WorkItemRef | null>;
  releaseWork(owner: string, thread: ThreadId): Promise<void>;
  /**
   * The canvas as the owner arranged it, kept by roll so every device shows the same (the canvas
   * parses it). Other devices hear a save as `AgentsData.canvas`.
   */
  canvas: OwnerDoc;
  /** The owner's agents settings (`owner-settings.ts` parses them). Other devices hear a save as `AgentsData.settings`. */
  settings: OwnerDoc;
  /** The owner's automations (`automations-api.ts`); unset: the Automations view says they aren't here. */
  automations?: AutomationsApi;
  /** The owner's Slack, Discord and GitHub connections (chat ops; `integrations-api.ts`). */
  integrations: IntegrationsApi;
}

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);

/** `canvas` over the relay. */
export function httpCanvas(api: AgentsApi = agentsApi): OwnerDoc {
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

/** `settings` over the relay. */
export function httpSettings(api: AgentsApi = agentsApi): OwnerDoc {
  return {
    load: async (owner) => {
      const c = await api.settings(owner);
      return c.settings === null ? null : { doc: c.settings, revision: c.revision };
    },
    save: async (owner, doc, revision) => {
      if (!isObject(doc)) throw new TypeError("the settings must be a JSON object");
      return { revision: (await api.saveSettings(owner, doc, revision)).revision };
    },
  };
}

/**
 * An owner's document in memory, for the demo and tests: what the relay does with it (revisions,
 * conflicts, a size limit, a copy kept rather than the caller's object), without the network.
 */
function memoryDoc(what: string, max: number, conflict: (s: { doc: Record<string, unknown> | null; revision: number; updated_at: number | null }) => Error): OwnerDoc {
  const saved = new Map<string, { doc: string; revision: number; updated_at: number }>();
  return {
    load: async (owner) => {
      const s = saved.get(owner);
      return s ? { doc: JSON.parse(s.doc) as unknown, revision: s.revision } : null;
    },
    save: async (owner, doc, revision) => {
      if (!isObject(doc)) throw new TypeError(`the ${what} must be a JSON object`);
      const json = JSON.stringify(doc);
      if (new TextEncoder().encode(json).length > max) throw new ApiError(`the ${what} can be at most ${max} bytes of JSON`, 413, null);
      const s = saved.get(owner);
      const current = s?.revision ?? 0;
      if (revision !== undefined && revision !== current) {
        throw conflict(s ? { doc: JSON.parse(s.doc) as Record<string, unknown>, revision: s.revision, updated_at: s.updated_at } : { doc: null, revision: 0, updated_at: null });
      }
      saved.set(owner, { doc: json, revision: current + 1, updated_at: Date.now() });
      return { revision: current + 1 };
    },
  };
}

/** `canvas` in memory (the demo, tests). */
export function memoryCanvas(): OwnerDoc {
  return memoryDoc("canvas", MAX_CANVAS, (s) => new CanvasConflict({ canvas: s.doc, revision: s.revision, updated_at: s.updated_at } satisfies SavedCanvas));
}

/** `settings` in memory (the demo, tests). */
export function memorySettings(): OwnerDoc {
  return memoryDoc("settings", MAX_SETTINGS, (s) => new SettingsConflict({ settings: s.doc, revision: s.revision, updated_at: s.updated_at } satisfies SavedSettings));
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
    machines: { rename: api.renameMachine, remove: api.removeMachine },
    canvas: httpCanvas(api),
    settings: httpSettings(api),
    automations: api.automations,
    integrations: api.integrations,
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
