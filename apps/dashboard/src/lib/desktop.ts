/**
 * The roll desktop app (apps/desktop) runs this dashboard in its window and exposes
 * `window.rollDesktop` there. Through it the canvas tells the shell what is running and waiting
 * (tray, dock badge, native notifications) and receives `roll://` deep links. In a browser there
 * is no bridge and every helper here does nothing.
 *
 * The bridge types mirror `apps/desktop/src/shared/bridge.ts`; a desktop test fails if they drift.
 */
import { useEffect, useRef } from "react";
import type { Machine, Request, Thread } from "./agents-api";
import type { AgentsData } from "./agents-store";

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

/** Where a `roll://` link points. */
export type DeepLink =
  | { kind: "agents" }
  | { kind: "thread"; thread: string }
  | { kind: "connect"; code: string };

export interface RollDesktop {
  /** The desktop app's version. */
  readonly version: string;
  readonly platform: "darwin" | "win32" | "linux";
  setStatus(status: DesktopStatus): void;
  notify(notice: DesktopNotice): void;
  /**
   * Called for every deep link while the page is open. While a listener is registered the shell
   * hands links to the page instead of reloading it. Returns an unsubscribe function.
   */
  onDeepLink(listener: (link: DeepLink) => void): () => void;
}

declare global {
  interface Window {
    rollDesktop?: RollDesktop;
  }
}

/** The bridge, or null in a browser (and during server rendering). */
export function desktop(): RollDesktop | null {
  return typeof window === "undefined" ? null : (window.rollDesktop ?? null);
}

/** The status the shell wants, from the agents shell state. Pure. */
export function desktopStatus(threads: Iterable<Thread>, requests: Iterable<Request>, machines: Iterable<Machine>): DesktopStatus {
  const byId = new Map<string, Thread>();
  let running = 0;
  for (const t of threads) {
    byId.set(t.id, t);
    if (t.archived_at === null && (t.status === "running" || t.status === "starting")) running++;
  }
  const pending: PendingRequest[] = [];
  const waiting = [...requests].filter((r) => r.status === "pending").sort((a, b) => a.created_at - b.created_at);
  for (const r of waiting) {
    const p: PendingRequest = {
      thread: r.thread,
      title: byId.get(r.thread)?.title ?? "",
      request: r.id,
      kind: r.body.type === "questions" ? "question" : "approval",
    };
    const summary = r.body.type === "questions" ? r.body.questions[0]?.question : (r.body.command ?? r.body.title);
    if (summary) p.summary = summary;
    pending.push(p);
  }
  return { running, pending, machines: [...machines].map((m) => ({ id: m.id, online: m.online })) };
}

/** Sends the status to the shell when there is one; does nothing in a browser. */
export function reportToDesktop(status: DesktopStatus): void {
  desktop()?.setStatus(status);
}

/** Subscribes to deep links when in the desktop app; returns the unsubscribe (a no-op in a browser). */
export function onDesktopDeepLink(listener: (link: DeepLink) => void): () => void {
  return desktop()?.onDeepLink(listener) ?? (() => {});
}

const values = <T>(m: ReadonlyMap<string, { value: T }>) => Array.from(m.values(), (r) => r.value);

/**
 * The canvas's one hook into the desktop app: reports the status whenever it changes (not on
 * every stream frame) and opens `roll://agents/thread/<id>` links through `focus("t:<id>")`.
 * Pass `null` data (the demo owner) to report nothing. Does nothing in a browser.
 */
export function useDesktop(data: AgentsData | null, focus: (key: string) => void): void {
  const sent = useRef("");
  useEffect(() => {
    if (!data || !desktop()) return;
    const status = desktopStatus(values(data.threads), values(data.requests), values(data.machines));
    const key = JSON.stringify(status);
    if (key === sent.current) return;
    sent.current = key;
    reportToDesktop(status);
  }, [data]);
  useEffect(
    () =>
      onDesktopDeepLink((link) => {
        if (link.kind === "thread") focus(`t:${link.thread}`);
      }),
    [focus],
  );
}
