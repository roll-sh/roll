/**
 * `state.json`: what the app remembers between launches (window bounds, whether the agents are
 * paused). Unlike settings it is rewritten by the app, never meant to be edited.
 */
import { readFileSync } from "node:fs";
import { writeJsonAtomic } from "./settings.ts";

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface AppState {
  bounds: Rect | null;
  maximized: boolean;
  /** "Pause agents" survives a restart: paused agents never start on their own. */
  paused: boolean;
}

export const MIN_SIZE = { width: 720, height: 480 };
export const DEFAULT_SIZE = { width: 1280, height: 820 };

const finite = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

export function parseState(raw: unknown): AppState {
  const o = (typeof raw === "object" && raw !== null ? raw : {}) as Record<string, unknown>;
  const b = o.bounds as Record<string, unknown> | undefined;
  const bounds =
    b && finite(b.x) && finite(b.y) && finite(b.width) && finite(b.height)
      ? { x: Math.round(b.x), y: Math.round(b.y), width: Math.round(b.width), height: Math.round(b.height) }
      : null;
  return { bounds, maximized: o.maximized === true, paused: o.paused === true };
}

export function readState(file: string): AppState {
  try {
    return parseState(JSON.parse(readFileSync(file, "utf8")));
  } catch {
    return parseState(undefined);
  }
}

export function writeState(file: string, state: AppState): void {
  writeJsonAtomic(file, state);
}

/**
 * Saved bounds, if enough of the window is still on one of the displays (a monitor may have been
 * unplugged); clamped to the minimum size. Null means "use the default size, centred".
 */
export function fitBounds(saved: Rect | null, workAreas: readonly Rect[]): Rect | null {
  if (!saved) return null;
  const width = Math.max(saved.width, MIN_SIZE.width);
  const height = Math.max(saved.height, MIN_SIZE.height);
  const visible = workAreas.some((a) => {
    const w = Math.min(saved.x + width, a.x + a.width) - Math.max(saved.x, a.x);
    const h = Math.min(saved.y + height, a.y + a.height) - Math.max(saved.y, a.y);
    // The title bar must be reachable: at least 100×40 px of the window on the display.
    return w >= 100 && h >= 40 && saved.y >= a.y - 8 && saved.y < a.y + a.height - 40;
  });
  if (!visible) return null;
  return { x: saved.x, y: saved.y, width, height };
}
