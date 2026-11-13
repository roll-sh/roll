/**
 * The workspace as the dashboard holds it: projects (rolld's `/v1/workspace`), every product's
 * resources, this month's spend per placed resource, and each project's staged additions (new
 * resources that exist only here until Deploy creates them). One store for the sidebar, the home
 * page and the canvas, so a card dragged on the canvas moves in the sidebar's map too.
 */

import { useEffect, useSyncExternalStore } from "react";
import { workspaceApi, type ResourceKind, type WorkspaceIcon, type WorkspaceProject } from "./workspace-api";
import {
  createResource,
  freeSpot,
  loadInventory,
  monthOf,
  resourceKey,
  spentThisMonth,
  type ResourceView,
} from "./workspace-resources";

export interface Staged {
  id: string;
  kind: ResourceKind;
  name: string;
  x: number;
  y: number;
  phase: "staged" | "creating" | "failed";
  error?: string;
  /** Made in its product but not placed yet: a retry places this one instead of making another. */
  ref?: string;
}

export interface WorkspaceState {
  status: "loading" | "ready" | "error";
  error?: string;
  projects: WorkspaceProject[];
  inventory: Map<string, ResourceView>;
  /** The kinds whose product this rolld runs (what Add offers). */
  kinds: Set<ResourceKind>;
  /** USD this month per `kind:ref`; absent while loading, null when the product couldn't say. */
  spend: Map<string, number | null>;
  staged: Record<string, Staged[]>;
}

// Staged additions live in this browser until Deploy makes them, so a reload keeps them.
const STAGED_KEY = "roll.workspace.staged";

function savedStaged(): Record<string, Staged[]> {
  try {
    const raw = typeof localStorage === "undefined" ? null : localStorage.getItem(STAGED_KEY);
    const saved = raw ? (JSON.parse(raw) as Record<string, Staged[]>) : {};
    // One that was being made when the page went away is staged again (Deploy says if it exists).
    return Object.fromEntries(Object.entries(saved).map(([p, list]) => [p, list.map((s) => ({ ...s, phase: s.phase === "creating" ? "staged" : s.phase }))]));
  } catch {
    return {};
  }
}

function saveStaged(staged: Record<string, Staged[]>) {
  try {
    const kept = Object.fromEntries(Object.entries(staged).filter(([, list]) => list.length));
    if (Object.keys(kept).length) localStorage.setItem(STAGED_KEY, JSON.stringify(kept));
    else localStorage.removeItem(STAGED_KEY);
  } catch {
    // Private mode or no storage: staged additions last until the page closes.
  }
}

let state: WorkspaceState = { status: "loading", projects: [], inventory: new Map(), kinds: new Set(), spend: new Map(), staged: savedStaged() };
const listeners = new Set<() => void>();
let loading: Promise<void> | null = null;

function set(next: Partial<WorkspaceState>) {
  if (next.staged && next.staged !== state.staged) saveStaged(next.staged);
  state = { ...state, ...next };
  for (const l of listeners) l();
}

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** A resource its product made that couldn't be placed on the canvas: `ref` exists. */
export class PlaceError extends Error {
  constructor(
    readonly ref: string,
    cause: unknown,
  ) {
    super(`made, but not placed on the canvas: ${errorText(cause)}`);
  }
}

function withProject(id: string, f: (p: WorkspaceProject) => WorkspaceProject) {
  set({ projects: state.projects.map((p) => (p.id === id ? f(p) : p)) });
}

async function loadSpend() {
  const month = monthOf(new Date());
  const placed = state.projects.flatMap((p) => p.resources).filter((r) => !state.spend.has(resourceKey(r)));
  await Promise.all(
    placed.map(async (r) => {
      const v = await spentThisMonth(r.kind, r.ref, month).catch(() => null);
      set({ spend: new Map(state.spend).set(resourceKey(r), v) });
    }),
  );
}

export function loadWorkspace(): Promise<void> {
  loading ??= (async () => {
    try {
      const [projects, inv] = await Promise.all([workspaceApi.projects(), loadInventory()]);
      const ids = new Set(projects.map((p) => p.id));
      const staged = Object.fromEntries(Object.entries(state.staged).filter(([id]) => ids.has(id)));
      set({ status: "ready", error: undefined, projects, inventory: inv.views, kinds: inv.kinds, staged });
      void loadSpend();
    } catch (e) {
      set({ status: "error", error: errorText(e) });
    } finally {
      loading = null;
    }
  })();
  return loading;
}

export async function refreshInventory() {
  const inv = await loadInventory();
  set({ inventory: inv.views, kinds: inv.kinds });
}

export const workspace = {
  async createProject(name: string, look: { description?: string; icon?: WorkspaceIcon | null } = {}) {
    const p = await workspaceApi.create(name, look);
    set({ projects: [...state.projects, p] });
    return p;
  },
  async update(id: string, patch: { name?: string; description?: string; icon?: WorkspaceIcon | null }) {
    const p = await workspaceApi.update(id, patch);
    withProject(id, () => p);
  },
  async removeProject(id: string) {
    await workspaceApi.remove(id);
    const { [id]: _, ...staged } = state.staged;
    set({ projects: state.projects.filter((p) => p.id !== id), staged });
  },

  /** Moves a card (or adds a resource), at once on screen; the server's answer settles it. */
  async place(projectId: string, kind: ResourceKind, ref: string, x: number, y: number) {
    const k = resourceKey({ kind, ref });
    const before = state.projects;
    const now = Date.now();
    set({
      projects: state.projects.map((p) => {
        const rest = p.resources.filter((r) => resourceKey(r) !== k);
        if (p.id !== projectId) return rest.length === p.resources.length ? p : { ...p, resources: rest };
        const old = p.resources.find((r) => resourceKey(r) === k);
        return { ...p, resources: [...rest, { kind, ref, x, y, added_at: old?.added_at ?? now }] };
      }),
    });
    try {
      await workspaceApi.place(projectId, kind, ref, x, y);
      if (!state.spend.has(k)) void loadSpend();
    } catch (e) {
      set({ projects: before });
      throw e;
    }
  },
  async unplace(projectId: string, kind: ResourceKind, ref: string) {
    const k = resourceKey({ kind, ref });
    const before = state.projects;
    withProject(projectId, (p) => ({ ...p, resources: p.resources.filter((r) => resourceKey(r) !== k) }));
    try {
      await workspaceApi.unplace(projectId, kind, ref);
    } catch (e) {
      set({ projects: before });
      throw e;
    }
  },

  /** A resource its product made and placed itself (a service: rolld puts its card where it was asked). */
  added(projectId: string, view: ResourceView, x: number, y: number) {
    const k = resourceKey(view);
    set({ inventory: new Map(state.inventory).set(k, view) });
    withProject(projectId, (p) => ({
      ...p,
      resources: [...p.resources.filter((r) => resourceKey(r) !== k), { kind: view.kind, ref: view.ref, x, y, added_at: Date.now() }],
    }));
    if (!state.spend.has(k)) void loadSpend();
  },
  /** Fresher views of some resources (a service's state, polled while the canvas shows it). */
  refreshViews(views: ResourceView[]) {
    const changed = views.filter((v) => JSON.stringify(state.inventory.get(resourceKey(v))) !== JSON.stringify(v));
    if (!changed.length) return;
    const inventory = new Map(state.inventory);
    for (const v of changed) inventory.set(resourceKey(v), v);
    set({ inventory });
  },
  /** A resource its product deleted (and took off its canvas). */
  forget(kind: ResourceKind, ref: string) {
    const k = resourceKey({ kind, ref });
    const inventory = new Map(state.inventory);
    inventory.delete(k);
    set({ inventory, projects: state.projects.map((p) => ({ ...p, resources: p.resources.filter((r) => resourceKey(r) !== k) })) });
  },

  /** A new resource on the canvas, made only on Deploy. */
  stage(projectId: string, kind: ResourceKind, name: string): Staged {
    const p = state.projects.find((q) => q.id === projectId);
    const taken = [...(p?.resources ?? []), ...(state.staged[projectId] ?? [])];
    const s: Staged = { id: `stg_${Math.random().toString(36).slice(2, 10)}`, kind, name, ...freeSpot(taken), phase: "staged" };
    set({ staged: { ...state.staged, [projectId]: [...(state.staged[projectId] ?? []), s] } });
    return s;
  },
  updateStaged(projectId: string, id: string, patch: Partial<Staged>) {
    set({ staged: { ...state.staged, [projectId]: (state.staged[projectId] ?? []).map((s) => (s.id === id ? { ...s, ...patch } : s)) } });
  },
  unstage(projectId: string, id?: string) {
    set({ staged: { ...state.staged, [projectId]: (state.staged[projectId] ?? []).filter((s) => id !== undefined && s.id !== id) } });
  },

  /** Creates a resource in its product (unless `made` names one already) and places it on the project's canvas at `at`; its ref. A failed placing throws a {@link PlaceError} with the ref. */
  async make(projectId: string, kind: ResourceKind, name: string, at: { x: number; y: number }, made?: string) {
    const ref = made ?? (await createResource(kind, name));
    try {
      await workspaceApi.place(projectId, kind, ref, at.x, at.y);
    } catch (e) {
      throw new PlaceError(ref, e);
    }
    withProject(projectId, (p) => ({ ...p, resources: [...p.resources, { kind, ref, x: at.x, y: at.y, added_at: Date.now() }] }));
    return ref;
  },

  /** Creates every staged resource in its product and places it where its card is. Failures stay staged, with the reason. */
  async deploy(projectId: string) {
    const todo = (state.staged[projectId] ?? []).filter((s) => s.phase !== "creating");
    for (const s of todo) workspace.updateStaged(projectId, s.id, { phase: "creating", error: undefined });
    await Promise.all(
      todo.map(async (s) => {
        try {
          await workspace.make(projectId, s.kind, s.name, s, s.ref);
          workspace.unstage(projectId, s.id);
        } catch (e) {
          workspace.updateStaged(projectId, s.id, { phase: "failed", error: errorText(e), ...(e instanceof PlaceError ? { ref: e.ref } : {}) });
        }
      }),
    );
    await refreshInventory().catch(() => {});
    void loadSpend();
  },
};

/** The state now, outside React. */
export const workspaceState = () => state;

const subscribe = (l: () => void) => {
  listeners.add(l);
  return () => listeners.delete(l);
};

const serverState: WorkspaceState = { status: "loading", projects: [], inventory: new Map(), kinds: new Set(), spend: new Map(), staged: {} };

/** The workspace, loading it the first time anything asks. */
export function useWorkspace(): WorkspaceState {
  const s = useSyncExternalStore(subscribe, () => state, () => serverState);
  useEffect(() => {
    if (state.status !== "ready" && !loading) void loadWorkspace();
  }, []);
  return s;
}

/** This month's spend for a set of resources: the sum, and whether every figure is in yet. */
export function spendOf(s: WorkspaceState, resources: { kind: ResourceKind; ref: string }[]) {
  let usd = 0;
  let pending = false;
  for (const r of resources) {
    const v = s.spend.get(resourceKey(r));
    if (v === undefined) pending = true;
    else if (v !== null) usd += v;
  }
  return { usd, pending };
}
