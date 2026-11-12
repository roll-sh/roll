/**
 * Which project dialog is open (docs/code/agents.md, "Projects"): adding a project to a machine,
 * or a new sandbox for a project. Page state, opened from wherever a project or machine shows
 * (its card, the folded rows, the sidebar, ⌘K, the empty states) and shown by one host.
 */

import { useSyncExternalStore } from "react";

export type ProjectDialog =
  /** Add a project (folder, clone, new repo), on a machine or one picked in the dialog. */
  | { kind: "add"; machine: string | null }
  /** Rent a sandbox with the project cloned and ready, then start an agent there. */
  | { kind: "sandbox"; project: string };

let current: ProjectDialog | null = null;
const subs = new Set<() => void>();

function set(next: ProjectDialog | null) {
  current = next;
  for (const f of subs) f();
}

const subscribe = (f: () => void) => {
  subs.add(f);
  return () => {
    subs.delete(f);
  };
};

export function useProjectDialog(): ProjectDialog | null {
  return useSyncExternalStore(subscribe, () => current, () => null);
}

export const openProjectDialog = (d: ProjectDialog) => set(d);
export const closeProjectDialog = () => current && set(null);

/** What's open now (outside React). */
export const currentProjectDialog = () => current;

/** Tests start with none open. */
export function resetProjectDialogs() {
  current = null;
}
