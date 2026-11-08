/**
 * Notifications in the browser, by the owner's `notifications` setting (T3 Code's
 * `notificationMode`: off, notifications, sound, or both): an approval or a question that starts
 * waiting, and a thread that finishes its work, while the page isn't in front. The desktop app
 * notifies on its own (`desktop.ts`), so this stays quiet there.
 */

import { useEffect, useRef } from "react";
import type { Thread } from "./agents-api";
import { rolling } from "./agents-graph";
import type { AgentsData } from "./agents-store";
import { desktop, desktopStatus } from "./desktop";
import type { NotifyMode } from "./owner-settings";

export interface Notice {
  /** One per thing, so a repeat replaces it (`Notification.tag`). */
  tag: string;
  title: string;
  body: string;
  thread: string;
}

/** What is waiting and what is working: what the next look compares against. */
export interface Seen {
  requests: Set<string>;
  busy: Set<string>;
}

export function seen(data: AgentsData): Seen {
  const { busy, requests } = rolling(data);
  return { requests: new Set(requests.map((r) => r.id)), busy: new Set(busy.filter((t) => t.status !== "waiting").map((t) => t.id)) };
}

/** What changed from `before` to `data` that the owner hears about. */
export function notices(before: Seen, data: AgentsData): Notice[] {
  const threads = [...data.threads.values()].map((r) => r.value);
  const status = desktopStatus(threads, [...data.requests.values()].map((r) => r.value), []);
  const out: Notice[] = [];
  for (const p of status.pending) {
    if (before.requests.has(p.request)) continue;
    out.push({ tag: p.request, title: p.kind === "question" ? "An agent has a question" : "An agent needs your approval", body: [p.title, p.summary].filter(Boolean).join(": "), thread: p.thread });
  }
  const now = new Map<string, Thread>(threads.map((t) => [t.id, t]));
  for (const id of before.busy) {
    const t = now.get(id);
    if (!t || t.archived_at !== null || t.status === "running" || t.status === "starting" || t.status === "waiting") continue;
    // Subagents report to their parent, not to the owner.
    if (t.lineage?.relation === "subagent") continue;
    out.push({ tag: `done:${t.id}`, title: t.status === "error" ? "An agent stopped with an error" : "An agent finished", body: t.title, thread: t.id });
  }
  return out;
}

/** A short, quiet chime (two soft sine notes), without an audio file. */
function chime() {
  const Ctx = (window as unknown as { AudioContext?: typeof AudioContext }).AudioContext;
  if (!Ctx) return;
  const ctx = new Ctx();
  const at = ctx.currentTime;
  for (const [i, freq] of [660, 880].entries()) {
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = "sine";
    osc.frequency.value = freq;
    gain.gain.setValueAtTime(0.0001, at + i * 0.12);
    gain.gain.exponentialRampToValueAtTime(0.08, at + i * 0.12 + 0.02);
    gain.gain.exponentialRampToValueAtTime(0.0001, at + i * 0.12 + 0.3);
    osc.connect(gain).connect(ctx.destination);
    osc.start(at + i * 0.12);
    osc.stop(at + i * 0.12 + 0.32);
  }
  setTimeout(() => void ctx.close(), 800);
}

/**
 * Notifies by `mode` about what changes in `data` (nothing at first: what's already waiting when
 * the page opens was seen). Clicking a notification opens its thread (`open`).
 */
export function useAgentNotifications(data: AgentsData | null, mode: NotifyMode, open: (thread: string) => void): void {
  const last = useRef<Seen | null>(null);
  const opener = useRef(open);
  opener.current = open;
  useEffect(() => {
    if (!data) return;
    const before = last.current;
    last.current = seen(data);
    if (!before || mode === "off" || desktop() || typeof document === "undefined") return;
    if (document.visibilityState === "visible" && document.hasFocus()) return;
    const found = notices(before, data);
    if (!found.length) return;
    if ((mode === "notify" || mode === "both") && typeof Notification !== "undefined" && Notification.permission === "granted") {
      for (const n of found.slice(0, 3)) {
        const shown = new Notification(n.title, { body: n.body, tag: n.tag });
        shown.onclick = () => {
          window.focus();
          opener.current(n.thread);
          shown.close();
        };
      }
    }
    if (mode === "sound" || mode === "both") chime();
  }, [data, mode]);
}
