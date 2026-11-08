/**
 * What the canvas folded and how to get at it (`foldQuiet` in lib/agents-graph.ts): the fold cards
 * (`folded-threads.tsx`) and the project cards (`nodes.tsx`) read it.
 */

import { createContext, useContext, type ReactNode } from "react";
import type { Fold } from "@/lib/agents-graph";

export interface CanvasFolds {
  /** Per container key (`p:…` in the overview, `g:…` inside a project), what it folded. */
  folded: ReadonlyMap<string, Fold>;
  /** Show a container's folded threads on the canvas, or fold them again. */
  toggle: (container: string) => void;
  /** Go into a project's space (absent: a read-only canvas). */
  drill?: (project: string) => void;
  /** The project whose space the canvas is, if it is one. */
  drilled?: string | null;
}

const Ctx = createContext<CanvasFolds>({ folded: new Map(), toggle: () => {} });

export function CanvasFoldsProvider({ value, children }: { value: CanvasFolds; children: ReactNode }) {
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export const useCanvasFolds = () => useContext(Ctx);
