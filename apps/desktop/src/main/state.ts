/**
 * `state.json`: what the app remembers between launches (each window's bounds, whether the agents
 * are paused). Unlike settings it is rewritten by the app, never meant to be edited.
 */
import { readFileSync } from "node:fs";
import { writeJsonAtomic } from "./settings.ts";

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** Where a window was. */
export interface WindowState {
  bounds: Rect | null;
  maximized: boolean;
}

/** The main window's state at the top level (as before there were two), the Agents window's under `agents`. */
export interface AppState extends WindowState {
  agents: WindowState;
  /** "Pause agents" survives a restart: paused agents never start on their own. */
  paused: boolean;
}

export const MIN_SIZE = { width: 720, height: 480 };
export const DEFAULT_SIZE = { width: 1280, height: 820 };

const finite = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

const record = (v: unknown): Record<string, unknown> => (typeof v === "object" && v !== null ? (v as Record<string, unknown>) : {});

function parseWindow(o: Record<string, unknown>): WindowState {
  const b = record(o.bounds);
  const bounds =
    finite(b.x) && finite(b.y) && finite(b.width) && finite(b.height)
      ? { x: Math.round(b.x), y: Math.round(b.y), width: Math.round(b.width), height: Math.round(b.height) }
      : null;
  return { bounds, maximized: o.maximized === true };
}

export function parseState(raw: unknown): AppState {
  const o = record(raw);
  return { ...parseWindow(o), agents: parseWindow(record(o.agents)), paused: o.paused === true };
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
