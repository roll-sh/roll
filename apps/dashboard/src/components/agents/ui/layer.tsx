/**
 * Popups, menus and dialogs portal to <body>, outside the agents surface's `[data-agents]`, so they
 * took the dashboard's shared tokens (`--fg`, `--muted`, `--line`, `--panel-2`) instead of the
 * agents ladder. Inside the surface (`AgentsLayer`), a portal's own element carries `data-agents`
 * too (globals.css: a `[data-layer]` takes the tokens without painting a background). The same
 * components used elsewhere (the public share page, Connect) keep the dashboard's look.
 */

import { createContext, useContext, type ReactNode } from "react";

const InSurface = createContext(false);

/** Everything under it is the agents surface: its portals carry the agents tokens. */
export function AgentsLayer({ children }: { children: ReactNode }) {
  return <InSurface.Provider value={true}>{children}</InSurface.Provider>;
}

const LAYER = { "data-agents": "", "data-layer": "" } as const;

/** Props for a portal's element: the agents tokens inside the surface, nothing outside it. */
export function useLayer(): typeof LAYER | undefined {
  return useContext(InSurface) ? LAYER : undefined;
}
