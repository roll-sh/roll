/**
 * The status the page reports through `rollDesktop.setStatus`, and what the tray, the badge and
 * notifications make of it together with the daemon's state. The page is remote content, so its
 * payload is parsed and bounded here before anything uses it.
 */
import type { DesktopNotice, DesktopStatus, PendingRequest } from "../shared/bridge.ts";
import type { DaemonState } from "./supervisor.ts";

const MAX_PENDING = 200;
const MAX_MACHINES = 200;
const MAX_TEXT = 200;
const MAX_ID = 128;

const text = (v: unknown, max = MAX_TEXT): string | null => (typeof v === "string" ? v.slice(0, max) : null);
const id = (v: unknown): string | null => (typeof v === "string" && v.length > 0 && v.length <= MAX_ID ? v : null);

export function parseStatus(raw: unknown): DesktopStatus | null {
  if (typeof raw !== "object" || raw === null) return null;
  const o = raw as Record<string, unknown>;
  const running = typeof o.running === "number" && Number.isFinite(o.running) ? Math.max(0, Math.floor(o.running)) : 0;
  const pending: PendingRequest[] = [];
  if (Array.isArray(o.pending)) {
    for (const p of o.pending.slice(0, MAX_PENDING)) {
      if (typeof p !== "object" || p === null) continue;
      const r = p as Record<string, unknown>;
      const thread = id(r.thread);
      const request = id(r.request);
      if (!thread || !request) continue;
      const item: PendingRequest = {
        thread,
        request,
        title: text(r.title) ?? "",
        kind: r.kind === "question" ? "question" : "approval",
      };
      const summary = text(r.summary);
      if (summary) item.summary = summary;
      pending.push(item);
    }
  }
  const status: DesktopStatus = { running, pending };
  if (Array.isArray(o.machines)) {
    status.machines = [];
    for (const m of o.machines.slice(0, MAX_MACHINES)) {
      const mid = typeof m === "object" && m !== null ? id((m as Record<string, unknown>).id) : null;
      if (mid) status.machines.push({ id: mid, online: (m as Record<string, unknown>).online === true });
    }
  }
  return status;
}

export function parseNotice(raw: unknown): DesktopNotice | null {
  if (typeof raw !== "object" || raw === null) return null;
  const o = raw as Record<string, unknown>;
  const title = text(o.title);
  if (!title) return null;
  const notice: DesktopNotice = { title };
  const body = text(o.body, 500);
  if (body) notice.body = body;
  const thread = id(o.thread);
  if (thread) notice.thread = thread;
  return notice;
}

/**
 * Remembers which requests were already announced, so each pending request notifies once,
 * however often the page re-sends its status. Forgets answered ones.
 */
export class Announcer {
  #seen = new Set<string>();
  #primed = false;

  /**
   * The requests that are new since the last status. The first status after launch only primes:
   * requests that were already waiting before the app opened show in the badge, not as a burst
   * of notifications.
   */
  fresh(status: DesktopStatus): PendingRequest[] {
    const now = new Set(status.pending.map((p) => p.request));
    const fresh = this.#primed ? status.pending.filter((p) => !this.#seen.has(p.request)) : [];
    this.#seen = now;
    this.#primed = true;
    return fresh;
  }
}

export function noticeFor(p: PendingRequest): DesktopNotice {
  const title = p.kind === "question" ? "A question is waiting" : "Approval needed";
  const what = p.title || "An agent";
  return { title, body: p.summary ? `${what}: ${p.summary}` : what, thread: p.thread };
}

export type LoginState = { kind: "signed_in" } | { kind: "signed_out" } | { kind: "signing_in"; code: string | null };

/** Everything the tray shows, as plain data (the menu is built from it). */
export interface TrayModel {
  /** One line: "Connected", "Offline · retrying in 4 s", "Waiting for approval · ABCD-EFGH", … */
  machine: string;
  running: number;
  pending: number;
  paused: boolean;
  /** Pause/resume makes sense (there is a CLI and the machine is signed in). */
  canPause: boolean;
  /** "Connect this machine…" makes sense (there is a CLI and the machine isn't signed in). */
  canSignIn: boolean;
  tooltip: string;
}

/** The daemon's link to the relay, as its log says (`crates/roll-cli/src/agent.rs`, `up`). */
export type LinkState = "connecting" | "connected" | "reconnecting";

/** The link state a line of `roll agent up` output announces, if any. */
export function parseDaemonLine(line: string): LinkState | null {
  if (!line.startsWith("roll agent: ")) return null;
  if (line.startsWith("roll agent: connected to roll as ")) return "connected";
  if (/; retrying in [0-9.]+ s$/.test(line)) return "reconnecting";
  return null;
}

export interface TrayInput {
  daemon: DaemonState;
  /** What the running daemon last said about its link. */
  link: LinkState;
  /** No `roll` binary was found. */
  missingBinary: boolean;
  /** The shell doesn't run the daemon (`settings.daemon: false`). */
  external: boolean;
  login: LoginState;
  paused: boolean;
  status: DesktopStatus | null;
  /** This machine's id, from agent.json. */
  machineId: string | null;
  now: number;
}

export function trayModel(i: TrayInput): TrayModel {
  const running = i.status?.running ?? 0;
  const pending = i.status?.pending.length ?? 0;
  const online = i.machineId ? i.status?.machines?.find((m) => m.id === i.machineId)?.online : undefined;
  let machine: string;
  if (i.external) machine = online === true ? "Connected" : online === false ? "Offline" : "Managed outside roll";
  else if (i.missingBinary) machine = "roll CLI not found";
  else if (i.login.kind === "signing_in") machine = i.login.code ? `Waiting for approval · ${i.login.code}` : "Signing in…";
  else if (i.login.kind === "signed_out") machine = "Not signed in";
  else if (i.paused) machine = "Paused";
  else
    switch (i.daemon.kind) {
      case "running":
        if (i.link === "connected") machine = "Connected";
        else if (i.link === "reconnecting") machine = "Offline · reconnecting";
        else machine = online === true ? "Connected" : "Connecting…";
        break;
      case "restarting":
        machine = `Offline · retrying in ${Math.max(1, Math.ceil((i.daemon.at - i.now) / 1000))} s`;
        break;
      case "stopping":
      case "stopped":
        machine = "Offline";
        break;
    }
  const parts = [`roll — ${machine}`];
  if (running) parts.push(`${running} running`);
  if (pending) parts.push(`${pending} waiting`);
  return {
    machine,
    running,
    pending,
    paused: i.paused,
    canPause: !i.external && !i.missingBinary && i.login.kind === "signed_in",
    canSignIn: !i.external && !i.missingBinary && i.login.kind === "signed_out",
    tooltip: parts.join(" · "),
  };
}

/** "3 running · 1 waiting" for the tray menu, or null when nothing is happening. */
export function activityLine(m: TrayModel): string | null {
  const parts: string[] = [];
  if (m.running) parts.push(`${m.running} agent${m.running === 1 ? "" : "s"} running`);
  if (m.pending) parts.push(`${m.pending} waiting for you`);
  return parts.length ? parts.join(" · ") : null;
}
