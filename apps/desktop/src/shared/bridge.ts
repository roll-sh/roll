/**
 * The `window.rollDesktop` bridge the preload exposes to the roll web app.
 *
 * `apps/dashboard/src/lib/desktop.ts` declares the same types for the dashboard;
 * `src/main/bridge-parity.test.ts` fails if the two drift.
 */

/** A runtime request (approval or question) waiting for the user. */
export interface PendingRequest {
  /** Thread id (`roll://agents/thread/<thread>` focuses it). */
  thread: string;
  /** The thread's title. */
  title: string;
  /** Request id: notifications fire once per id. */
  request: string;
  kind: "approval" | "question";
  /** One line about what is asked (the command, the question). */
  summary?: string;
}

/** What the canvas tells the shell whenever its shell state changes. */
export interface DesktopStatus {
  /** Threads currently running (starting or running). */
  running: number;
  pending: PendingRequest[];
  /** Machines the relay knows and whether they are connected; lets the tray say "connected". */
  machines?: { id: string; online: boolean }[];
}

/** A one-off native notification. Clicking it focuses the window (on `thread`, if given). */
export interface DesktopNotice {
  title: string;
  body?: string;
  thread?: string;
}

/** What the Agents window should show. */
export type AgentsTarget = { kind: "canvas" } | { kind: "thread"; thread: string } | { kind: "new" };

/** Where a `roll://` link points. */
export type DeepLink =
  | { kind: "agents" }
  | { kind: "thread"; thread: string }
  | { kind: "new" }
  | { kind: "connect"; code: string };

export interface RollDesktop {
  /** The desktop app's version. */
  readonly version: string;
  readonly platform: "darwin" | "win32" | "linux";
  /** Which window this page is in. */
  readonly window: "main" | "agents";
  setStatus(status: DesktopStatus): void;
  notify(notice: DesktopNotice): void;
  /**
   * Called for every deep link while the page is open. While a listener is registered the shell
   * hands links to the page instead of reloading it. Returns an unsubscribe function.
   */
  onDeepLink(listener: (link: DeepLink) => void): () => void;
  /** Open (or focus) the Agents window on `target`. */
  openAgents(target: AgentsTarget): void;
}

/** IPC channel names shared by main and preload. */
export const ipc = {
  setStatus: "roll:status",
  notify: "roll:notify",
  /** Renderer → main: the page has (1) or no longer has (0) a deep-link listener. */
  deepLinkListening: "roll:deeplink-listening",
  /** Main → renderer: a deep link. */
  deepLink: "roll:deeplink",
  /** Renderer → main: open (or focus) the Agents window on a target. */
  openAgents: "roll:open-agents",
} as const;

/** The renderer argument (`webPreferences.additionalArguments`) carrying the app's version. */
export const VERSION_ARG = "--roll-desktop-version=";
/** The renderer argument naming the window the page is in (`main` or `agents`). */
export const WINDOW_ARG = "--roll-desktop-window=";

export type DesktopWindow = RollDesktop["window"];

/** The window named in a renderer's arguments; `main` when absent or unknown. */
export function windowFromArgv(argv: readonly string[]): DesktopWindow {
  return argv.includes(`${WINDOW_ARG}agents`) ? "agents" : "main";
}
