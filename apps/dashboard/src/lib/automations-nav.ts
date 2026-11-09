/**
 * The automations sheet's stack (docs/code/automations-ui.md, "The sheet"). The view in the URL is
 * the top of the stack; this turns it into a level, knows each level's parent (for a deep link, where
 * nothing came before), and keeps the trail of what was opened, so Back returns where you came from
 * and the sheet knows whether a page slides in from the right (deeper) or the left (back).
 *
 * Levels, outermost first: the list (its tabs are one level) → an automation's overview → its
 * builder (Build and Settings are one level) or its runs → one run → a conversation that run started.
 */

import { sameView, type AutomationsView } from "./agents-surface";

export type Level =
  | { kind: "list" }
  | { kind: "automation"; id: string }
  | { kind: "builder"; id: string }
  | { kind: "history"; id: string }
  | { kind: "run"; run: string }
  | { kind: "thread"; thread: string };

export function levelOf(v: AutomationsView): Level {
  if (v.thread) return { kind: "thread", thread: v.thread };
  if (v.run) return { kind: "run", run: v.run };
  if (!v.open) return { kind: "list" };
  if (v.open === "new" || v.tab === "edit" || v.tab === "settings") return { kind: "builder", id: v.open };
  if (v.tab === "runs") return { kind: "history", id: v.open };
  return { kind: "automation", id: v.open };
}

/** Two views on one level (a tab switch) replace each other rather than stack. */
export function levelKey(l: Level): string {
  switch (l.kind) {
    case "list":
      return "list";
    case "automation":
    case "builder":
    case "history":
      return `${l.kind}:${l.id}`;
    case "run":
      return `run:${l.run}`;
    case "thread":
      return `thread:${l.thread}`;
  }
}

/** The builder fills the working area when maximized; nothing else does. */
export const isBuilder = (v: AutomationsView) => levelOf(v).kind === "builder";

/** Where Back goes when nothing was opened before (a deep link); `null` closes the sheet. */
export function parentOf(v: AutomationsView): AutomationsView | null {
  const list: AutomationsView = v.list ? { kind: "automations", list: v.list } : { kind: "automations" };
  const l = levelOf(v);
  switch (l.kind) {
    case "list":
      return null;
    case "thread": {
      const { thread: _, ...rest } = v;
      return rest;
    }
    case "run":
      return v.open ? { ...list, open: v.open } : { kind: "automations", list: "runs" };
    case "builder":
      return v.open === "new" ? list : { ...list, open: v.open };
    case "history":
      return { ...list, open: v.open };
    case "automation":
      return list;
  }
}

/** What was opened in the sheet, oldest first; the last is what shows. */
export type Trail = readonly AutomationsView[];

/**
 * The trail after showing `next`, and which way the page moves: `1` deeper (slides in from the
 * right), `-1` back to something opened before (from the left), `0` the same level (a tab).
 */
export function step(trail: Trail, next: AutomationsView): { trail: Trail; dir: -1 | 0 | 1 } {
  const top = trail.at(-1);
  if (top && sameView(top, next)) return { trail, dir: 0 };
  const key = levelKey(levelOf(next));
  if (top && levelKey(levelOf(top)) === key) return { trail: [...trail.slice(0, -1), next], dir: 0 };
  // A new automation just saved (or left) isn't somewhere to come back to.
  if (top?.open === "new" && next.open !== "new") return step(trail.slice(0, -1), next);
  const at = trail.findIndex((v) => levelKey(levelOf(v)) === key);
  if (at >= 0) return { trail: [...trail.slice(0, at), next], dir: -1 };
  // Its parent is somewhere back there (a run opened from the overview, then the overview again
  // from a link): moving up is back, even past what came between.
  if (top && depth(next) < depth(top)) return { trail: [...trail.filter((v) => depth(v) < depth(next)), next], dir: -1 };
  return { trail: [...trail, next], dir: 1 };
}

/** How many levels down `v` is (the list is 0). */
export function depth(v: AutomationsView): number {
  let n = 0;
  for (let p = parentOf(v); p; p = parentOf(p)) n++;
  return n;
}

/** Where Back goes from the top of the trail: what came before, else its parent; `null` closes. */
export function backOf(trail: Trail): AutomationsView | null {
  const top = trail.at(-1);
  if (!top) return null;
  return trail.at(-2) ?? parentOf(top);
}
