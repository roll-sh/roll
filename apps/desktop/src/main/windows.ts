/**
 * The app's two windows: the main window (the dashboard and roll code, `settings.url`) and the
 * Agents window (the canvas, `/dashboard/agents`). Pure decisions about which window a link goes
 * to and how a target reaches the Agents window; main.ts does the Electron side.
 */
import type { AgentsTarget, DeepLink } from "../shared/bridge.ts";
import { deepLinkPath, THREAD_ID } from "./deeplink.ts";

/** Where a `roll://` link goes: the Agents window on a target, or a page in the main window. */
export type LinkRoute = { window: "agents"; target: AgentsTarget } | { window: "main"; path: string };

export function routeLink(link: DeepLink): LinkRoute {
  switch (link.kind) {
    case "agents":
      return { window: "agents", target: { kind: "canvas" } };
    case "thread":
      return { window: "agents", target: { kind: "thread", thread: link.thread } };
    case "new":
      return { window: "agents", target: { kind: "new" } };
    case "connect":
      return { window: "main", path: deepLinkPath(link) };
  }
}

/** The deep link an open Agents window's page receives for `target`. */
export function targetLink(target: AgentsTarget): DeepLink {
  return target.kind === "canvas" ? { kind: "agents" } : target;
}

/** The URL the Agents window loads to show `target` (`?focus=t:<thread>`, `?new`). */
export function agentsUrl(origin: string, target: AgentsTarget): string {
  return origin + deepLinkPath(targetLink(target));
}

/** A target sent by a page over IPC; anything that isn't exactly one is dropped. */
export function parseAgentsTarget(raw: unknown): AgentsTarget | null {
  if (typeof raw !== "object" || raw === null) return null;
  const o = raw as Record<string, unknown>;
  switch (o.kind) {
    case "canvas":
      return { kind: "canvas" };
    case "new":
      return { kind: "new" };
    case "thread":
      return typeof o.thread === "string" && THREAD_ID.test(o.thread) ? { kind: "thread", thread: o.thread } : null;
    default:
      return null;
  }
}

/** What the Agents window is doing when a target arrives. */
export interface AgentsWindowState {
  open: boolean;
  /** The app's page in it has a deep-link listener. */
  listening: boolean;
  /** A page is still loading (it may start listening soon). */
  loading: boolean;
}

/**
 * How a target reaches the Agents window: `open` it on the target's URL, `send` the link to the
 * listening page, `queue` it until the loading page listens (main.ts loads the URL if it never
 * does), or `load` the target's URL into a page that doesn't listen.
 */
export type Delivery = "open" | "send" | "queue" | "load";

export function delivery(w: AgentsWindowState): Delivery {
  if (!w.open) return "open";
  if (w.listening) return "send";
  return w.loading ? "queue" : "load";
}
